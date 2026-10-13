//! Background processing: chat cleanup with retries, thread summary
//! (generation, retry after failure, use in context, mutation-driven
//! invalidation), orphan watchdog, upload reaper, audit delivery.

mod common;

use common::*;
use futures::StreamExt;
use http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn chat_cleanup_retries_provider_failures() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.upload(&a, chat, "a.pdf", "application/pdf", b"%PDF").await;
    h.upload(&a, chat, "b.pdf", "application/pdf", b"%PDF").await;
    h.gw.delete_fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let d = h.req("DELETE", &format!("/mini-chat/v1/chats/{chat}"), &a, None).await;
    assert_eq!(d.status, StatusCode::NO_CONTENT, "returns before provider cleanup");
    h.eventually("cleanup attempted", || async {
        (h.gw.calls("/v1/files/").iter().filter(|c| c.method == "DELETE").count() >= 2).then_some(())
    })
    .await;
    let attempts = h
        .scalar(&format!("SELECT MAX(cleanup_attempts) FROM attachments WHERE chat_id = {}", blob(chat)))
        .await;
    assert!(attempts >= 1);
    let errs = h
        .rows(&format!("SELECT last_cleanup_error FROM attachments WHERE chat_id = {}", blob(chat)))
        .await;
    assert!(errs.iter().any(|r| r[0].is_some()));
    h.gw.delete_fail.store(false, std::sync::atomic::Ordering::SeqCst);
    h.eventually("cleanup done", || async {
        let n = h
            .scalar(&format!(
                "SELECT COUNT(*) FROM attachments WHERE chat_id = {} AND cleanup_status = 'done'",
                blob(chat)
            ))
            .await;
        let vs = h
            .scalar(&format!("SELECT COUNT(*) FROM chat_vector_stores WHERE chat_id = {}", blob(chat)))
            .await;
        (n == 2 && vs == 0).then_some(())
    })
    .await;
    assert!(h.gw.calls("/vector_stores/").iter().any(|c| c.method == "DELETE"));
    // soft-deleted rows are never hard-purged
    assert_eq!(h.scalar(&format!("SELECT COUNT(*) FROM chats WHERE id = {}", blob(chat))).await, 1);
}

fn summary_options() -> Options {
    Options {
        catalog: vec![
            model(
                "chat-model",
                "standard",
                json!({"context_window": 3000, "max_output_tokens": 1000, "max_input_tokens": 0,
                       "preference": {"is_default": true}}),
            ),
            model("summarizer", "standard", json!({})),
        ],
        config: json!({"thread_summary_worker": {"summary_model_id": "summarizer", "compression_threshold_pct": 10}}),
        ..Options::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_summary_generation_and_use() {
    let h = Harness::with(summary_options()).await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let long = "words ".repeat(150);
    let t1 = h.send(&a, chat, &format!("first {long}")).await;
    assert_eq!(t1.names().last(), Some(&"done"));
    assert!(h.gw.summary_calls().is_empty(), "nothing to summarize after the first turn");
    let t2 = h.send(&a, chat, &format!("second {long}")).await;
    assert_eq!(t2.names().last(), Some(&"done"));
    let row = h
        .eventually("summary stored", || async {
            let r = h
                .rows(&format!(
                    "SELECT summary_text, hex(summarized_up_to_message_id), CAST(token_estimate AS TEXT) FROM thread_summaries WHERE chat_id = {}",
                    blob(chat)
                ))
                .await;
            r.into_iter().next()
        })
        .await;
    assert_eq!(row[0].as_deref(), Some("Earlier: the user greeted the assistant."));
    // frontier = last message before the triggering turn (t1's assistant message)
    assert_eq!(row[1].as_deref().unwrap().to_lowercase(), t1.message_id().simple().to_string());
    assert_eq!(row[2].as_deref(), Some("20"), "output_tokens - reasoning_tokens");
    let compressed = h
        .scalar(&format!("SELECT COUNT(*) FROM messages WHERE chat_id = {} AND is_compressed = 1", blob(chat)))
        .await;
    assert_eq!(compressed, 2, "t1 user + assistant compressed");

    // summary request shape
    let req = h.gw.summary_calls().pop().unwrap().json();
    assert_eq!(req["model"], "summarizer-provider");
    assert_ne!(req["stream"], true);
    assert_eq!(req["metadata"]["request_type"], "summary");
    let prompt = req["input"].to_string();
    assert!(prompt.contains("Summarize the following conversation"));
    assert!(prompt.contains("first words"));
    assert!(!prompt.contains("second words"), "the triggering turn is never summarized");

    // the next turn uses the summary + uncompressed recent messages
    let t3 = h.send(&a, chat, "third").await;
    let started = t3.first("stream_started").unwrap();
    assert!(started["thread_summary_applied"]["token_estimate"].as_u64().unwrap() > 0, "{started}");
    let call = h.gw.chat_calls().pop().unwrap().json();
    let input = call["input"].as_array().unwrap();
    let first = input[0].to_string();
    assert!(first.contains("Earlier: the user greeted the assistant."), "{call}");
    assert!(first.contains("summarized"), "preamble present: {first}");
    assert_eq!(input[0]["role"], "user");
    assert!(!call["input"].to_string().contains("first words"), "compressed messages are not resent");
    assert!(call["input"].to_string().contains("second words"));
    // the UI still sees every message
    assert_eq!(h.messages(&a, chat).await.len(), 6);

    // system usage event for the summary call
    h.drain_outbox().await;
    let ev = h.policy.published.lock().clone();
    let sys: Vec<_> = ev.iter().filter(|e| e.requester_type == "system").collect();
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0].billing_outcome, "system_task");
    assert_eq!(sys[0].system_task_type.as_deref(), Some("thread_summary_update"));
    assert!(sys[0].user_id.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_summary_retries_after_provider_failure() {
    let h = Harness::with(summary_options()).await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push_summary(Reply::Status(500, json!({"error": {"message": "summary failed"}})));
    let long = "words ".repeat(150);
    h.send(&a, chat, &format!("first {long}")).await;
    h.send(&a, chat, &format!("second {long}")).await;
    h.eventually("summary stored after retry", || async {
        (h.scalar(&format!("SELECT COUNT(*) FROM thread_summaries WHERE chat_id = {}", blob(chat))).await == 1)
            .then_some(())
    })
    .await;
    assert!(h.gw.summary_calls().len() >= 2, "failed call retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn mutation_of_covered_turn_drops_summary() {
    let h = Harness::with(summary_options()).await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let long = "words ".repeat(150);
    let t1 = h.send(&a, chat, &format!("first {long}")).await;
    let t2 = h.send(&a, chat, &format!("second {long}")).await;
    h.eventually("summary stored", || async {
        (h.scalar(&format!("SELECT COUNT(*) FROM thread_summaries WHERE chat_id = {}", blob(chat))).await == 1)
            .then_some(())
    })
    .await;
    // deleting t2 keeps the summary (it covers t1 only)
    let d = h
        .req("DELETE", &format!("/mini-chat/v1/chats/{chat}/turns/{}", t2.request_id()), &a, None)
        .await;
    assert_eq!(d.status, StatusCode::NO_CONTENT);
    assert_eq!(h.scalar(&format!("SELECT COUNT(*) FROM thread_summaries WHERE chat_id = {}", blob(chat))).await, 1);
    // retrying t1 (covered by the summary) drops it and clears is_compressed
    let r = h
        .sse("POST", &format!("/mini-chat/v1/chats/{chat}/turns/{}/retry", t1.request_id()), &a, None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.raw);
    assert_eq!(h.scalar(&format!("SELECT COUNT(*) FROM thread_summaries WHERE chat_id = {}", blob(chat))).await, 0);
    assert_eq!(
        h.scalar(&format!("SELECT COUNT(*) FROM messages WHERE chat_id = {} AND is_compressed = 1", blob(chat)))
            .await,
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn orphan_watchdog_finalizes_stale_turns() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    let rid = Uuid::new_v4();
    let resp = h
        .open_stream(
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            &a,
            json!({"content": "hang", "request_id": rid}),
        )
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
        if seen.contains("stream_started") {
            break;
        }
    }
    // nothing stale yet
    assert_eq!(h.svc.orphan_scan().await, 0);
    h.exec(&format!(
        "UPDATE chat_turns SET last_progress_at = '2000-01-01T00:00:00.000000001+00:00' WHERE request_id = {}",
        blob(rid)
    ))
    .await;
    assert_eq!(h.svc.orphan_scan().await, 1);
    let t = h.turn_row(rid).await;
    assert_eq!(t[0].as_deref(), Some("failed"));
    assert_eq!(t[1].as_deref(), Some("orphan_timeout"));
    let st = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), &a).await;
    assert_eq!(st.body["state"], "error");
    assert_eq!(st.body["error_code"], "orphan_timeout");
    assert_eq!(h.svc.orphan_scan().await, 0, "finalized once");
    // reserve settled (estimated), usage billed as aborted
    h.drain_outbox().await;
    assert_eq!(h.quota(USER_A, "total", "daily").await.1, 0);
    let ev: Vec<Value> = h
        .policy
        .published
        .lock()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["billing_outcome"], "aborted");
    assert_eq!(ev[0]["settlement_method"], "estimated");
    // the late provider completion loses the CAS: the relay reports stream_interrupted
    tx.send(delta_frame("late")).unwrap();
    tx.send(completed_frame(1, 1)).unwrap();
    drop(tx);
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
    }
    let events = parse_sse(&seen);
    let last = events.last().unwrap();
    assert_eq!(last.0, "error", "{seen}");
    assert_eq!(last.1["code"], "stream_interrupted");
    assert_eq!(h.turn_row(rid).await[0].as_deref(), Some("failed"), "watchdog result stands");
    h.drain_outbox().await;
    assert_eq!(h.policy.published.lock().len(), 1, "settled exactly once");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_reaper_fails_abandoned_uploads() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let ok = h.upload(&a, chat, "keep.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(ok.status, StatusCode::CREATED);
    let stale = Uuid::new_v4();
    let fresh = Uuid::new_v4();
    for (id, ts, file) in [
        (stale, "2000-01-01T00:00:00.000000001+00:00", "'file-abandoned0000000000000'"),
        (fresh, "2999-01-01T00:00:00.000000001+00:00", "NULL"),
    ] {
        h.exec(&format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, content_type, size_bytes, \
             storage_backend, provider_file_id, status, attachment_kind, for_file_search, for_code_interpreter, \
             cleanup_attempts, created_at, updated_at, secondary_status) VALUES ({}, {}, {}, {}, 'x.pdf', 'application/pdf', 1, \
             'openai', {file}, 'uploaded', 'document', 1, 0, 0, '{ts}', '{ts}', 'not_attempted')",
            blob(id),
            blob(TENANT_A),
            blob(chat),
            blob(USER_A)
        ))
        .await;
    }
    assert_eq!(h.svc.upload_reaper_scan().await, 1);
    let rows = h
        .rows(&format!("SELECT status, error_code, cleanup_status FROM attachments WHERE id = {}", blob(stale)))
        .await;
    assert_eq!(rows[0][0].as_deref(), Some("failed"));
    assert_eq!(rows[0][1].as_deref(), Some("upload_abandoned"));
    let fresh_row = h.rows(&format!("SELECT status FROM attachments WHERE id = {}", blob(fresh))).await;
    assert_eq!(fresh_row[0][0].as_deref(), Some("uploaded"));
    h.eventually("abandoned provider file deleted", || async {
        h.gw.calls("/files/file-abandoned").iter().any(|c| c.method == "DELETE").then_some(())
    })
    .await;
    let g = h.get(&format!("/mini-chat/v1/chats/{chat}/attachments/{stale}"), &a).await;
    assert_eq!(g.body["status"], "failed");
    assert_eq!(g.body["error_code"], "upload_abandoned");
    assert_eq!(h.svc.upload_reaper_scan().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_audit_events_are_delivered() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, "hello").await;
    h.gw.push(Reply::Status(500, json!({"error": {"message": "x"}})));
    let f = h.send(&a, chat, "again").await;
    h.drain_outbox().await;
    let events: Vec<Value> = h.audit.events.lock().iter().map(|e| serde_json::to_value(e).unwrap()).collect();
    let find = |rid: Uuid| events.iter().find(|e| e.to_string().contains(&rid.to_string())).cloned();
    let ok = find(s.request_id()).expect("completed audit");
    assert_eq!(ok["event_type"], "turn_completed");
    let failed = find(f.request_id()).expect("failed audit");
    assert_eq!(failed["event_type"], "turn_failed");
    assert!(!ok.to_string().contains("sk-"), "no secrets in audit");
}
