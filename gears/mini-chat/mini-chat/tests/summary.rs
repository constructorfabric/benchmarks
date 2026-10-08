//! T065: thread summary trigger, generation, CAS frontier, failure/retry, invalidation, use in context.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use mini_chat::domain::context::SUMMARY_PREAMBLE;
use mini_chat::domain::service::finalize::ThreadSummaryPayload;
use mini_chat::domain::service::summary::TaskOutcome;
use serde_json::{Value, json};
use uuid::Uuid;

async fn harness() -> Harness {
    let mut cfg = default_config();
    cfg["thread_summary_worker"] =
        json!({"summary_model_id": "std", "compression_threshold_pct": 1, "max_attempts": 2});
    let mut p = default_policy();
    let mut small = model("small", "standard", true, false, (false, false, false));
    small["context_window"] = json!(3000);
    p["model_catalog"].as_array_mut().unwrap().push(small);
    Harness::with(Opts {
        config: cfg,
        policy: p,
        ..Opts::default()
    })
    .await
}

async fn wait_summary(
    h: &Harness,
    chat: Uuid,
) -> Option<mini_chat::infra::db::entities::thread_summary::Model> {
    for _ in 0..200 {
        if let Some(s) = db::summary(h, chat).await {
            return Some(s);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

const LONG: &str =
    "Please remember that my favourite colour is teal and my cat is called Miso, thanks a lot.";

#[tokio::test]
async fn summary_generated_and_used_in_next_context() {
    let h = harness().await;
    let chat = h.create_chat_as(&h.ctx(), json!({"model": "small"})).await;
    h.provider.push(Script::ok("Noted: teal and Miso."));
    let r1 = h.send(chat, LONG).await;
    assert_eq!(r1.names().last(), Some(&"done"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        h.provider.summary_requests().is_empty(),
        "no earlier messages to summarize after the first turn"
    );

    h.send(chat, "And what else?").await;
    let s = wait_summary(&h, chat).await.expect("summary row");
    assert_eq!(s.summary_text, "The user said hello.");
    assert!(s.token_estimate > 0);
    let sreq = h.provider.summary_requests().pop().unwrap();
    assert_eq!(sreq["model"], "prov-std");
    assert_eq!(sreq["metadata"]["request_type"], "summary");
    let text = sreq.to_string();
    assert!(text.contains("User: Please remember"), "{text}");
    assert!(text.contains("Assistant: Noted: teal and Miso."), "{text}");
    assert!(
        !text.contains("And what else?"),
        "the causing turn is not summarized"
    );

    // first turn messages are compressed; the latest turn is not
    let msgs = db::messages(&h, chat).await;
    let compressed: Vec<&str> = msgs
        .iter()
        .filter(|m| m.is_compressed)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(compressed.len(), 2, "{compressed:?}");
    // system usage event for the summary call
    assert!(
        h.eventually(|| h
            .policy
            .published()
            .iter()
            .any(|e| e.system_task_type.as_deref() == Some("thread_summary_update")))
            .await
    );
    let ev = h
        .policy
        .published()
        .into_iter()
        .find(|e| e.system_task_type.is_some())
        .unwrap();
    assert_eq!(ev.requester_type, "system");
    assert_eq!(ev.chat_id, Some(chat));

    // next turn: summary applied as a user-role preamble message, compressed messages omitted
    let r3 = h.send(chat, "third").await;
    let applied = &r3.first("stream_started").unwrap()["thread_summary_applied"];
    assert_eq!(applied["token_estimate"], s.token_estimate);
    let req = h.provider.chat_requests().pop().unwrap();
    let input = req["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    let first_text = input[0]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| input[0]["content"].as_str().unwrap_or(""));
    assert!(
        first_text.contains(SUMMARY_PREAMBLE) && first_text.contains("The user said hello."),
        "{}",
        input[0]
    );
    assert!(
        !req.to_string().contains("favourite colour"),
        "compressed messages are not resent"
    );
    assert!(req.to_string().contains("And what else?"));
    // messages API still lists everything
    assert_eq!(h.messages(chat).await.len(), 6);
}

#[tokio::test]
async fn provider_failure_keeps_previous_state_and_retries_then_rejects() {
    let h = harness().await;
    h.provider.fail_summary.store(true, Ordering::SeqCst);
    let chat = h.create_chat_as(&h.ctx(), json!({"model": "small"})).await;
    h.send(chat, LONG).await;
    h.send(chat, "second").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(db::summary(&h, chat).await.is_none());
    let r = h.send(chat, "third").await;
    assert_eq!(r.names().last(), Some(&"done"));
    assert!(
        r.first("stream_started")
            .unwrap()
            .get("thread_summary_applied")
            .is_none()
    );

    // direct handler invocation: retry until the last attempt, then reject
    let msgs = db::messages(&h, chat).await;
    let mut sorted = msgs.clone();
    sorted.sort_by_key(|m| (m.created_at, m.id));
    let target = &sorted[1];
    let p = ThreadSummaryPayload {
        tenant_id: h.tenant,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".into(),
    };
    assert!(matches!(
        h.core.process_thread_summary(&p, 1).await,
        TaskOutcome::Retry(_)
    ));
    assert!(matches!(
        h.core.process_thread_summary(&p, 2).await,
        TaskOutcome::Reject(_)
    ));
    assert!(db::summary(&h, chat).await.is_none());

    // provider recovers: the same task succeeds; a stale base frontier then loses the CAS
    h.provider.fail_summary.store(false, Ordering::SeqCst);
    // (pending outbox retries may race with the direct call; either way a summary now exists)
    assert!(matches!(
        h.core.process_thread_summary(&p, 1).await,
        TaskOutcome::Ok
    ));
    let s = wait_summary(&h, chat).await.unwrap();
    assert!(
        sorted
            .iter()
            .position(|m| m.id == s.summarized_up_to_message_id)
            .unwrap()
            >= 1
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = db::summary(&h, chat).await.unwrap();
    *h.provider.summary_text.lock().unwrap() = "<summary>other</summary>".into();
    assert!(matches!(
        h.core.process_thread_summary(&p, 1).await,
        TaskOutcome::Ok
    ));
    assert_eq!(
        db::summary(&h, chat).await.unwrap().summary_text,
        s.summary_text,
        "CAS on base frontier"
    );

    // an existing summary survives a later failure
    h.provider.fail_summary.store(true, Ordering::SeqCst);
    let p2 = ThreadSummaryPayload {
        base_frontier_created_at: Some(s.summarized_up_to_created_at),
        base_frontier_message_id: Some(s.summarized_up_to_message_id),
        frozen_target_created_at: sorted[5].created_at,
        frozen_target_message_id: sorted[5].id,
        system_request_id: Uuid::new_v4(),
        ..p
    };
    assert!(matches!(
        h.core.process_thread_summary(&p2, 1).await,
        TaskOutcome::Retry(_)
    ));
    assert_eq!(
        db::summary(&h, chat).await.unwrap().summary_text,
        s.summary_text
    );
}

#[tokio::test]
async fn mutation_of_summarized_turn_invalidates_summary() {
    let h = harness().await;
    let chat = h.create_chat_as(&h.ctx(), json!({"model": "small"})).await;
    let r1 = h.send(chat, LONG).await.request_id();
    let r2 = h.send(chat, "second").await.request_id();
    wait_summary(&h, chat).await.expect("summary");
    // delete back to the summarized turn and retry it
    let (s, _, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/turns/{r2}"),
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 204);
    assert!(
        db::summary(&h, chat).await.is_some(),
        "deleting an unsummarized turn keeps the summary"
    );
    let r = h
        .sse(
            &h.ctx(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/turns/{r1}/retry"),
            None,
        )
        .await;
    assert_eq!(r.names().last(), Some(&"done"), "{r:?}");
    assert!(db::summary(&h, chat).await.is_none(), "summary invalidated");
    assert!(
        db::messages(&h, chat)
            .await
            .iter()
            .all(|m| !m.is_compressed)
    );
    let _: Value = json!(null);
}

#[tokio::test]
async fn no_trigger_below_threshold() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.send(chat, "a").await;
    h.send(chat, "b").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.provider.summary_requests().is_empty());
    assert!(db::summary(&h, chat).await.is_none());
}

#[tokio::test]
async fn truncation_triggers_urgent_summary_and_drops_oldest_turns() {
    // high proactive threshold: only the urgent (truncation) trigger can fire
    let mut cfg = default_config();
    cfg["thread_summary_worker"] =
        json!({"summary_model_id": "std", "compression_threshold_pct": 99});
    let mut p = default_policy();
    let mut small = model("small", "standard", true, false, (false, false, false));
    small["context_window"] = json!(3000);
    p["model_catalog"].as_array_mut().unwrap().push(small);
    let h = Harness::with(Opts {
        config: cfg,
        policy: p,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat_as(&h.ctx(), json!({"model": "small"})).await;
    let filler = "lorem ipsum ".repeat(125);
    for i in 1..=4 {
        h.provider.push(Script::ok("ok"));
        let r = h.send(chat, &format!("MARK{i} {filler}")).await;
        assert_eq!(r.names().last(), Some(&"done"), "turn {i}: {r:?}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.provider.summary_requests().is_empty(),
        "history still fits: no summary yet"
    );
    let req = h.provider.chat_requests().pop().unwrap().to_string();
    assert!(req.contains("MARK1"), "all turns fit before truncation");

    h.provider.push(Script::ok("ok"));
    let r = h.send(chat, &format!("MARK5 {filler}")).await;
    assert_eq!(r.names().last(), Some(&"done"));
    let req = h.provider.chat_requests().pop().unwrap();
    let text = req.to_string();
    assert!(!text.contains("MARK1"), "oldest whole turn dropped");
    assert!(
        text.contains("MARK2") && text.contains("MARK5"),
        "newer turns kept"
    );
    let input = req["input"].as_array().unwrap();
    assert_eq!(
        input[0]["role"], "user",
        "history never starts with an assistant message"
    );
    // truncation fires the urgent trigger despite the 99% threshold
    let s = wait_summary(&h, chat).await.expect("urgent summary");
    assert!(!s.summary_text.is_empty());
    assert!(
        h.provider
            .summary_requests()
            .pop()
            .unwrap()
            .to_string()
            .contains("MARK1")
    );
}
