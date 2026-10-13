//! US7 — background processing: chat cleanup, attachment cleanup retries, upload reaper,
//! orphan watchdog, thread summary, background indexing (T067, T068, T071).

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

#[allow(clippy::unwrap_used, reason = "test helper: a failed upload must fail the test")]
async fn upload_doc(env: &TestEnv, chat: Uuid, name: &str) -> Value {
    let (ct, body) = multipart(name, Some("text/plain"), b"document body");
    let r = env
        .raw(
            "a1",
            Request::builder().method(Method::POST).uri(format!("/mini-chat/v1/chats/{chat}/attachments")).header("content-type", ct),
            Body::from(body),
        )
        .await;
    let status = r.status();
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status, StatusCode::CREATED, "{v}");
    v
}

#[tokio::test]
async fn chat_deletion_cleans_provider_files_then_the_vector_store() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    upload_doc(&env, chat, "a.txt").await;
    upload_doc(&env, chat, "b.txt").await;
    env.mock.reset();
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let done = env
        .eventually(Duration::from_secs(15), || async {
            (env.count("SELECT COUNT(*) FROM chat_vector_stores").await == 0).then_some(())
        })
        .await;
    assert!(done.is_some(), "vector store row not removed");
    assert_eq!(env.count("SELECT COUNT(*) FROM attachments WHERE cleanup_status = 'done'").await, 2);
    let reqs = env.mock.requests();
    let deletes: Vec<&str> = reqs.iter().filter(|r| r.method == Method::DELETE).map(|r| r.path.as_str()).collect();
    assert_eq!(deletes.iter().filter(|p| p.starts_with("/v1/files/")).count(), 2);
    let vs_pos = deletes.iter().position(|p| p.starts_with("/v1/vector_stores/")).expect("vector store delete");
    assert_eq!(vs_pos, 2, "vector store must be deleted after the files: {deletes:?}");
}

#[tokio::test]
async fn failing_provider_deletes_are_retried_then_marked_failed() {
    let env = TestEnv::with(EnvOptions { config: json!({"cleanup_worker": {"max_attempts": 2}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    upload_doc(&env, chat, "a.txt").await;
    env.mock.configure(|c| c.delete_status = 500);
    env.mock.reset();
    env.json("a1", Method::DELETE, &format!("/chats/{chat}"), None).await;
    let failed = env
        .eventually(Duration::from_secs(30), || async {
            (env.count("SELECT COUNT(*) FROM attachments WHERE cleanup_status = 'failed' AND cleanup_attempts = 2 AND last_cleanup_error IS NOT NULL").await == 1).then_some(())
        })
        .await;
    assert!(failed.is_some(), "attachment cleanup not marked failed");
    // The vector store delete also fails and the row is kept for a dead-letter replay.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_vector_stores").await, 1);
}

#[tokio::test]
async fn upload_reaper_fails_abandoned_uploads_and_schedules_cleanup() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let doc = upload_doc(&env, chat, "a.txt").await;
    let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
    env.exec(&format!("UPDATE attachments SET status = 'uploaded', updated_at = '2000-01-01T00:00:00.000000001Z' WHERE id = {}", blob(id))).await;
    let reaped = env.svc.upload_reaper_scan().await.unwrap();
    assert_eq!(reaped, 1);
    let (_, g) = env.json("a1", Method::GET, &format!("/chats/{chat}/attachments/{id}"), None).await;
    assert_eq!(g["status"], "failed");
    assert_eq!(g["error_code"], "upload_abandoned");
    let cleaned = env
        .eventually(Duration::from_secs(10), || async {
            (env.count("SELECT COUNT(*) FROM attachments WHERE cleanup_status = 'done'").await == 1).then_some(())
        })
        .await;
    assert!(cleaned.is_some());
    // A second scan finds nothing; fresh rows are left alone.
    assert_eq!(env.svc.upload_reaper_scan().await.unwrap(), 0);
}

#[tokio::test]
async fn orphan_watchdog_finalizes_stale_running_turns() {
    let env = Arc::new(TestEnv::start().await);
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let e2 = env.clone();
    let task = tokio::spawn(async move { e2.stream("a1", chat, json!({"content": "[[hang]]", "request_id": rid})).await });
    env.eventually(Duration::from_secs(5), || async {
        (env.count("SELECT COUNT(*) FROM chat_turns WHERE state = 'running'").await == 1).then_some(())
    })
    .await
    .unwrap();
    // Not stale yet.
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 0);
    env.exec("UPDATE chat_turns SET started_at = '2000-01-01T00:00:00.000000001Z', last_progress_at = '2000-01-01T00:00:00.000000001Z'").await;
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 1);
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "orphan_timeout");
    let usage = env
        .eventually(Duration::from_secs(10), || async { env.policy.usage_events().into_iter().find(|u| u.request_id == rid) })
        .await
        .expect("usage event");
    assert_eq!(usage.billing_outcome, "aborted");
    assert_eq!(usage.settlement_method, "estimated");
    assert_eq!(usage.selected_model, usage.effective_model);
    let audit = env
        .eventually(Duration::from_secs(10), || async {
            env.audit.turns.lock().unwrap().iter().find(|a| a.request_id == rid).cloned()
        })
        .await
        .expect("audit");
    assert_eq!(audit.event_type, "turn_failed");
    assert_eq!(audit.policy_decisions.quota.decision, "unknown");
    // Reserve released: nothing remains reserved.
    assert_eq!(env.count("SELECT COALESCE(SUM(reserved_credits_micro),0) FROM quota_usage").await, 0);
    // A second scan does not finalize it again.
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 0);
    task.abort();
}

fn long(n: usize, tag: &str) -> String {
    format!("{tag} {}", "lorem ipsum dolor sit amet ".repeat(n))
}

#[tokio::test]
async fn thread_summary_is_generated_committed_and_used() {
    let env = TestEnv::with(EnvOptions { config: json!({"thread_summary_worker": {"compression_threshold_pct": 1, "summary_model_id": "gpt-4.1-mini"}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    env.stream("a1", chat, json!({"content": long(200, "first")})).await;
    // First turn: nothing earlier to summarize.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 0);
    env.stream("a1", chat, json!({"content": long(200, "second")})).await;
    let ok = env
        .eventually(Duration::from_secs(15), || async { (env.count("SELECT COUNT(*) FROM thread_summaries").await == 1).then_some(()) })
        .await;
    assert!(ok.is_some(), "summary not committed");
    let rows = env.query("SELECT summary_text, token_estimate FROM thread_summaries").await;
    let text: String = rows[0].try_get("", "summary_text").unwrap();
    assert_eq!(text, "Summary of the conversation.");
    let est: i32 = rows[0].try_get("", "token_estimate").unwrap();
    assert_eq!(est, 20);
    // The summarized range (first turn) is compressed; the causing turn is not.
    assert_eq!(env.count("SELECT COUNT(*) FROM messages WHERE is_compressed = 1").await, 2);
    // Summary request: non-streaming, system identity, summary model.
    let sreq = env.mock.responses_requests().into_iter().find(|r| r.body["stream"] == false).expect("summary request");
    assert_eq!(sreq.body["model"], "gpt-4.1-mini-provider");
    assert_eq!(sreq.body["metadata"]["request_type"], "summary");
    assert_eq!(sreq.body["user"], format!("{}{}", TENANT_A.simple(), SYS_USER.simple()));
    let prompt = sreq.body["input"].to_string();
    assert!(prompt.contains("Summarize the following conversation:"));
    assert!(prompt.contains("User: first"));
    assert!(!prompt.contains("User: second"), "causing turn must not be summarized");
    // System usage event.
    let ev = env
        .eventually(Duration::from_secs(10), || async {
            env.policy.usage_events().into_iter().find(|u| u.billing_outcome == "system_task")
        })
        .await
        .expect("system usage event");
    assert_eq!(ev.requester_type, "system");
    assert!(ev.user_id.is_none() && ev.turn_id.is_none());
    assert_eq!(ev.actual_credits_micro, 0);
    assert_eq!(ev.settlement_method, "none");
    assert_eq!(ev.system_task_type.as_deref(), Some("thread_summary_update"));
    assert_eq!(ev.dedupe_key, format!("{}/thread_summary_update/{}", TENANT_A.simple(), ev.request_id.simple()));
    // The next turn receives the summary instead of the compressed messages.
    let r = env.stream("a1", chat, json!({"content": "third"})).await;
    assert_eq!(r.event("stream_started").unwrap()["thread_summary_applied"]["token_estimate"], est);
    let req = env.mock.responses_requests().into_iter().rfind(|r| r.body["stream"] == true).unwrap();
    let input = req.body["input"].to_string();
    assert!(input.contains("This conversation has earlier messages that have been summarized."));
    assert!(input.contains("Summary of the conversation."));
    assert!(!input.contains("first lorem"), "compressed messages must not be sent");
}

#[tokio::test]
async fn summary_model_unavailable_rejects_the_task_and_failures_keep_state() {
    let env = TestEnv::with(EnvOptions { config: json!({"thread_summary_worker": {"compression_threshold_pct": 1, "summary_model_id": "disabled-model", "max_attempts": 2}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    env.stream("a1", chat, json!({"content": long(200, "first")})).await;
    env.stream("a1", chat, json!({"content": long(200, "second")})).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 0);
    assert!(env.mock.responses_requests().iter().all(|r| r.body["stream"] == true));
    assert_eq!(env.count("SELECT COUNT(*) FROM messages WHERE is_compressed = 1").await, 0);
}

#[tokio::test]
async fn provider_failure_of_the_summary_call_keeps_the_previous_state() {
    let env = TestEnv::with(EnvOptions { config: json!({"thread_summary_worker": {"compression_threshold_pct": 1, "summary_model_id": "gpt-4.1-mini", "max_attempts": 2}}), ..EnvOptions::default() }).await;
    env.mock.configure(|c| c.summary_status = 500);
    let chat = env.create_chat("a1", json!({})).await;
    env.stream("a1", chat, json!({"content": long(200, "first")})).await;
    env.stream("a1", chat, json!({"content": long(200, "second")})).await;
    let attempts = env
        .eventually(Duration::from_secs(20), || async {
            (env.mock.responses_requests().iter().filter(|r| r.body["stream"] == false).count() >= 2).then_some(())
        })
        .await;
    assert!(attempts.is_some(), "summary call not retried");
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 0);
    assert_eq!(env.count("SELECT COUNT(*) FROM messages WHERE is_compressed = 1").await, 0);
}

#[tokio::test]
async fn mutation_of_a_summarized_turn_invalidates_the_summary() {
    let env = TestEnv::with(EnvOptions { config: json!({"thread_summary_worker": {"compression_threshold_pct": 1, "summary_model_id": "gpt-4.1-mini"}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    let r1 = Uuid::new_v4();
    let r2 = Uuid::new_v4();
    env.stream("a1", chat, json!({"content": long(200, "first"), "request_id": r1})).await;
    env.stream("a1", chat, json!({"content": long(200, "second"), "request_id": r2})).await;
    env.eventually(Duration::from_secs(15), || async { (env.count("SELECT COUNT(*) FROM thread_summaries").await == 1).then_some(()) })
        .await
        .expect("summary");
    // Delete the latest turn, then the first (now latest and covered by the summary).
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/turns/{r2}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/turns/{r1}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 0);
}

#[tokio::test]
async fn slow_indexing_returns_uploaded_and_finishes_in_the_background() {
    let env = TestEnv::start().await;
    env.mock.configure(|c| c.index_status = "in_progress".into());
    let chat = env.create_chat("a1", json!({})).await;
    let started = std::time::Instant::now();
    let doc = upload_doc(&env, chat, "slow.txt").await;
    assert!(started.elapsed() >= Duration::from_secs(24), "returned before the 25 s deadline");
    assert_eq!(doc["status"], "uploaded");
    let id = doc["id"].as_str().unwrap().to_owned();
    // Not ready yet: a message referencing it is rejected.
    let r = env.stream("a1", chat, json!({"content": "x", "attachment_ids": [id]})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");
    env.mock.configure(|c| c.index_status = "completed".into());
    let ready = env
        .eventually(Duration::from_secs(30), || async {
            let (_, g) = env.json("a1", Method::GET, &format!("/chats/{chat}/attachments/{id}"), None).await;
            (g["status"] == "ready").then_some(())
        })
        .await;
    assert!(ready.is_some(), "background indexing did not finish");
}
