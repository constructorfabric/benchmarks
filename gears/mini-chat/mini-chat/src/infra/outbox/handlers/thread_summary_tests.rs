#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::*;
use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::context::test_support::*;
use crate::domain::summary::prompt::ANALYSIS_INSTRUCTION;
use crate::infra::db::entities::message as message_entity;
use crate::infra::outbox::PAYLOAD_THREAD_SUMMARY;
use crate::testing::{MockProvider, STANDARD, TENANT_A, TestApp, USER_A1};

struct Fixture {
    t: TestApp,
    chat: Uuid,
    /// Messages of the first two turns (u1, a1, u2, a2) and of the causing turn (u3, a3).
    m: Vec<message_entity::Model>,
}

async fn fixture_with(t: TestApp) -> Fixture {
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let mut m = Vec::new();
    for i in 1..=3 {
        let (_, u, a) = insert_turn(&t.app, TENANT_A, chat, &format!("question {i}"), &format!("answer {i}")).await;
        m.push(u);
        m.push(a);
    }
    Fixture { t, chat, m }
}

async fn fixture() -> Fixture {
    fixture_with(TestApp::new().await).await
}

fn payload(f: &Fixture, base: Option<&message_entity::Model>, target: &message_entity::Model) -> ThreadSummaryPayload {
    ThreadSummaryPayload {
        tenant_id: TENANT_A,
        chat_id: f.chat,
        system_request_id: Uuid::new_v4(),
        system_task_type: "thread_summary_update".into(),
        base_frontier_created_at: base.map(|b| b.created_at),
        base_frontier_message_id: base.map(|b| b.id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
    }
}

fn outbox_msg(p: &ThreadSummaryPayload, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload: serde_json::to_vec(p).unwrap(),
        payload_type: PAYLOAD_THREAD_SUMMARY.into(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

async fn handle(f: &Fixture, p: &ThreadSummaryPayload, attempts: i16) -> MessageResult {
    ThreadSummaryHandler::new(Arc::clone(&f.t.app)).handle(&outbox_msg(p, attempts)).await
}

/// Bodies of the non-streaming (summary) provider calls.
fn summary_requests(p: &MockProvider) -> Vec<serde_json::Value> {
    p.recorded().into_iter().filter(|r| !r.streaming && r.method == "POST").filter_map(|r| r.json).collect()
}

fn prompt_text(body: &serde_json::Value) -> String {
    body.pointer("/input/0/content/0/text").and_then(serde_json::Value::as_str).unwrap().to_owned()
}

async fn compressed_ids(f: &Fixture) -> Vec<Uuid> {
    get_messages(&f.t.app, f.chat).await.into_iter().filter(|m| m.is_compressed).map(|m| m.id).collect()
}

fn completion(status: u16, body: &serde_json::Value) -> HttpResponse {
    HttpResponse { status, retry_after_secs: None, body: Bytes::from(body.to_string()) }
}

fn completion_text(text: &str) -> HttpResponse {
    completion(
        200,
        &serde_json::json!({
            "id": "resp_x", "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}],
            "usage": {"input_tokens": 50, "output_tokens": 0}
        }),
    )
}

#[tokio::test]
async fn first_summary_commits_row_range_and_usage_event() {
    let f = fixture().await;
    let p = payload(&f, None, &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));

    let s = get_summary(&f.t.app, f.chat).await.expect("summary row");
    assert_eq!(s.tenant_id, TENANT_A);
    assert_eq!(s.summary_text, "Summary text");
    assert_eq!((s.summarized_up_to_created_at, s.summarized_up_to_message_id), (f.m[3].created_at, f.m[3].id));
    assert_eq!(s.token_estimate, 20, "output_tokens - reasoning_tokens");
    assert_eq!(compressed_ids(&f).await, vec![f.m[0].id, f.m[1].id, f.m[2].id, f.m[3].id]);

    // Request shape.
    let reqs = summary_requests(&f.t.provider);
    assert_eq!(reqs.len(), 1);
    let body = &reqs[0];
    assert_eq!(body["model"], format!("{STANDARD}-provider"));
    assert_eq!(body["stream"], false);
    assert_eq!(body["instructions"], DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["metadata"]["request_type"], "summary");
    assert_eq!(body["metadata"]["feature"], "none");
    assert_eq!(body["metadata"]["chat_id"], f.chat.to_string());
    assert_eq!(body["metadata"]["user_id"], DEFAULT_SUBJECT_ID.to_string());
    assert_eq!(body["user"], format!("{}{}", TENANT_A.simple(), DEFAULT_SUBJECT_ID.simple()));
    assert!(body.get("tools").is_none());
    let text = prompt_text(body);
    assert!(text.starts_with(
        "Summarize the following conversation:\n\nUser: question 1\n\nAssistant: answer 1\n\nUser: question 2\n\nAssistant: answer 2\n\n"
    ), "{text}");
    assert!(text.ends_with(ANALYSIS_INSTRUCTION));
    assert!(!text.contains("question 3"), "the causing turn is never summarized");

    // System usage event published through the usage queue.
    let policy = Arc::clone(&f.t.policy);
    f.t.eventually("system usage event published", || {
        let policy = Arc::clone(&policy);
        async move { !policy.published.lock().unwrap().is_empty() }
    })
    .await;
    let ev = f.t.policy.published.lock().unwrap()[0].clone();
    assert_eq!(ev.tenant_id, TENANT_A);
    assert_eq!(ev.user_id, None);
    assert_eq!(ev.chat_id, f.chat);
    assert_eq!(ev.turn_id, None);
    assert_eq!(ev.request_id, p.system_request_id);
    assert_eq!(ev.effective_model, STANDARD);
    assert_eq!(ev.selected_model, STANDARD);
    assert_eq!(ev.terminal_state, "completed");
    assert_eq!(ev.billing_outcome, "system_task");
    assert_eq!(ev.settlement_method, "none");
    assert_eq!(ev.actual_credits_micro, 0);
    assert_eq!(ev.requester_type, "system");
    assert_eq!(ev.system_task_type.as_deref(), Some("thread_summary_update"));
    assert_eq!(
        ev.dedupe_key,
        format!("{}/thread_summary_update/{}", TENANT_A.simple(), p.system_request_id.simple())
    );
    let usage = ev.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (100, 20));
}

#[tokio::test]
async fn incremental_summary_includes_existing_summary_and_advances_frontier() {
    let f = fixture().await;
    insert_summary(&f.t.app, TENANT_A, f.chat, "Old summary", (f.m[1].created_at, f.m[1].id), 5).await;
    set_message_flags(&f.t.app, f.m[0].id, false, true).await;
    set_message_flags(&f.t.app, f.m[1].id, false, true).await;
    let p = payload(&f, Some(&f.m[1]), &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));

    let s = get_summary(&f.t.app, f.chat).await.unwrap();
    assert_eq!(s.summary_text, "Summary text");
    assert_eq!(s.summarized_up_to_message_id, f.m[3].id);
    assert_eq!(compressed_ids(&f).await, vec![f.m[0].id, f.m[1].id, f.m[2].id, f.m[3].id]);

    let text = prompt_text(&summary_requests(&f.t.provider)[0]);
    assert!(text.starts_with("The existing summary below covers the earlier conversation."), "{text}");
    assert!(
        text.contains("<existing_summary>\nOld summary\n</existing_summary>\n\nNew messages to incorporate:\n\nUser: question 2\n\nAssistant: answer 2\n\n"),
        "{text}"
    );
    assert!(!text.contains("question 1"), "already summarized messages are not resent");
}

#[tokio::test]
async fn catalog_prompt_and_content_truncation() {
    let t = TestApp::with_config(|c| c.thread_summary_worker.message_content_limit = 5).await;
    t.policy.with_snapshot(|s| {
        for m in &mut s.model_catalog {
            if m.id == STANDARD {
                m.thread_summary_prompt = "Catalog summary prompt".into();
            }
        }
    });
    let f = fixture_with(t).await;
    let p = payload(&f, None, &f.m[1]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    let body = &summary_requests(&f.t.provider)[0];
    assert_eq!(body["instructions"], "Catalog summary prompt");
    let text = prompt_text(body);
    assert!(text.contains("User: quest...\n\nAssistant: answe...\n\n"), "{text}");
}

#[tokio::test]
async fn provider_error_keeps_state_and_retries_then_rejects() {
    let f = fixture().await;
    let p = payload(&f, None, &f.m[3]);
    let err = || completion(500, &serde_json::json!({"error": {"message": "boom", "type": "server_error"}}));
    f.t.provider.completions.lock().unwrap().push_back(err());
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Retry));
    assert!(get_summary(&f.t.app, f.chat).await.is_none());
    assert!(compressed_ids(&f).await.is_empty());

    // Last attempt (attempts = max_attempts - 1) is dead-lettered.
    f.t.provider.completions.lock().unwrap().push_back(err());
    assert!(matches!(handle(&f, &p, 2).await, MessageResult::Reject(_)));
    assert!(get_summary(&f.t.app, f.chat).await.is_none());

    // Existing summary is kept unchanged on failure.
    insert_summary(&f.t.app, TENANT_A, f.chat, "Old", (f.m[1].created_at, f.m[1].id), 3).await;
    f.t.provider.completions.lock().unwrap().push_back(err());
    let p = payload(&f, Some(&f.m[1]), &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Retry));
    let s = get_summary(&f.t.app, f.chat).await.unwrap();
    assert_eq!((s.summary_text.as_str(), s.summarized_up_to_message_id), ("Old", f.m[1].id));
    assert!(f.t.policy.published.lock().unwrap().is_empty());
}

#[tokio::test]
async fn context_length_error_retries_with_fewer_messages() {
    let f = fixture().await;
    let p = payload(&f, None, &f.m[3]);
    f.t.provider.completions.lock().unwrap().push_back(completion(
        400,
        &serde_json::json!({"error": {"message": "This model's maximum context length is 10 tokens", "code": "context_length_exceeded"}}),
    ));
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    let reqs = summary_requests(&f.t.provider);
    assert_eq!(reqs.len(), 2);
    assert!(prompt_text(&reqs[0]).contains("User: question 1"));
    let second = prompt_text(&reqs[1]);
    assert!(!second.contains("User: question 1"), "oldest message dropped: {second}");
    assert!(second.contains("Assistant: answer 1"));
    assert_eq!(get_summary(&f.t.app, f.chat).await.unwrap().summarized_up_to_message_id, f.m[3].id);
}

#[tokio::test]
async fn empty_summary_retries_without_commit() {
    let f = fixture().await;
    let p = payload(&f, None, &f.m[3]);
    f.t.provider.completions.lock().unwrap().push_back(completion_text("<analysis>only thinking</analysis>"));
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Retry));
    assert!(get_summary(&f.t.app, f.chat).await.is_none());
    f.t.provider.completions.lock().unwrap().push_back(completion_text("<summary>   </summary>"));
    assert!(matches!(handle(&f, &p, 2).await, MessageResult::Reject(_)));
    assert!(get_summary(&f.t.app, f.chat).await.is_none());
    assert!(compressed_ids(&f).await.is_empty());
}

#[tokio::test]
async fn plain_text_answer_and_fallback_token_estimate() {
    let f = fixture().await;
    let p = payload(&f, None, &f.m[3]);
    f.t.provider.completions.lock().unwrap().push_back(completion_text("Plain summary\n\n\n\nsecond line"));
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    let s = get_summary(&f.t.app, f.chat).await.unwrap();
    assert_eq!(s.summary_text, "Plain summary\n\nsecond line");
    assert_eq!(s.token_estimate, 7, "ceil(26 bytes / 4) when the provider reports no output tokens");
}

#[tokio::test]
async fn frontier_changed_before_call_is_a_cas_conflict() {
    let f = fixture().await;
    // Another run already committed a summary: a base-less task loses.
    insert_summary(&f.t.app, TENANT_A, f.chat, "Other", (f.m[1].created_at, f.m[1].id), 3).await;
    let p = payload(&f, None, &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    // A task based on a different frontier loses too.
    let p = payload(&f, Some(&f.m[0]), &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    assert!(summary_requests(&f.t.provider).is_empty(), "no provider call after a lost pre-check");
    let s = get_summary(&f.t.app, f.chat).await.unwrap();
    assert_eq!((s.summary_text.as_str(), s.summarized_up_to_message_id), ("Other", f.m[1].id));
}

#[tokio::test]
async fn missing_base_summary_finishes_without_work() {
    let f = fixture().await;
    let p = payload(&f, Some(&f.m[1]), &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    assert!(summary_requests(&f.t.provider).is_empty());
    assert!(get_summary(&f.t.app, f.chat).await.is_none());
}

#[tokio::test]
async fn commit_cas_conflict_writes_nothing() {
    let f = fixture().await;
    let model = f.t.policy.snapshot.lock().unwrap().model(STANDARD).unwrap().clone();

    // Base-less commit while a row exists.
    insert_summary(&f.t.app, TENANT_A, f.chat, "Winner", (f.m[1].created_at, f.m[1].id), 3).await;
    let p = payload(&f, None, &f.m[3]);
    let run = SummaryRun { app: &f.t.app, payload: &p, attempt: 1 };
    assert!(matches!(run.commit(&model, "Loser".into(), 1, None).await.unwrap(), Commit::Conflict));

    // Commit based on a stale frontier (conditional update matches no row).
    let p = payload(&f, Some(&f.m[0]), &f.m[3]);
    let run = SummaryRun { app: &f.t.app, payload: &p, attempt: 1 };
    assert!(matches!(run.commit(&model, "Loser".into(), 1, None).await.unwrap(), Commit::Conflict));

    let s = get_summary(&f.t.app, f.chat).await.unwrap();
    assert_eq!((s.summary_text.as_str(), s.summarized_up_to_message_id), ("Winner", f.m[1].id));
    assert!(compressed_ids(&f).await.is_empty());
}

#[tokio::test]
async fn deleted_target_skips_commit() {
    let f = fixture().await;
    set_message_flags(&f.t.app, f.m[3].id, true, false).await;
    let p = payload(&f, None, &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    assert!(get_summary(&f.t.app, f.chat).await.is_none());
    assert!(compressed_ids(&f).await.is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(f.t.policy.published.lock().unwrap().is_empty(), "no usage event without a commit");
}

#[tokio::test]
async fn missing_or_disabled_summary_model_rejects() {
    for model in ["old-model", "no-such-model"] {
        let t = TestApp::with_config(|c| c.thread_summary_worker.summary_model_id = model.into()).await;
        let f = fixture_with(t).await;
        let p = payload(&f, None, &f.m[3]);
        assert!(matches!(handle(&f, &p, 0).await, MessageResult::Reject(_)), "{model}");
        assert!(summary_requests(&f.t.provider).is_empty());
        assert!(get_summary(&f.t.app, f.chat).await.is_none());
    }
}

#[tokio::test]
async fn policy_failure_retries() {
    let f = fixture().await;
    f.t.policy.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let p = payload(&f, None, &f.m[3]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Retry));
    assert!(matches!(handle(&f, &p, 2).await, MessageResult::Reject(_)));
}

#[tokio::test]
async fn invalid_payload_rejects() {
    let f = fixture().await;
    let msg = OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload: b"not json".to_vec(),
        payload_type: PAYLOAD_THREAD_SUMMARY.into(),
        created_at: chrono::Utc::now(),
        attempts: 0,
    };
    let r = ThreadSummaryHandler::new(Arc::clone(&f.t.app)).handle(&msg).await;
    assert!(matches!(r, MessageResult::Reject(_)));
}

#[tokio::test]
async fn empty_range_finishes_without_call() {
    let f = fixture().await;
    // Target equals nothing new after the base: (a1, a1].
    insert_summary(&f.t.app, TENANT_A, f.chat, "S", (f.m[1].created_at, f.m[1].id), 3).await;
    let p = payload(&f, Some(&f.m[1]), &f.m[1]);
    assert!(matches!(handle(&f, &p, 0).await, MessageResult::Ok));
    assert!(summary_requests(&f.t.provider).is_empty());
}
