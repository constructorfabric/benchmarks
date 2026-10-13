//! Cleanup & recovery: chat deletion cleanup and thread summaries.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use crate::common::*;
use crate::turns::tiny_png;

/// Chat deletion triggers reliable background cleanup of provider-side resources.
#[tokio::test]
async fn chat_deletion_cleans_provider_resources() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let a = Uuid::parse_str(h.upload(U1, chat, "a.pdf", "application/pdf", b"%PDF").await.json()["id"].as_str().unwrap()).unwrap();
    let b = Uuid::parse_str(h.upload(U1, chat, "b.txt", "text/plain", b"b").await.json()["id"].as_str().unwrap()).unwrap();
    let c = Uuid::parse_str(h.upload(U1, chat, "c.png", "image/png", &tiny_png()).await.json()["id"].as_str().unwrap()).unwrap();
    h.send_message(U1, chat, json!({"content": "x", "attachment_ids": [c]})).await;
    assert_eq!(h.vector_stores(chat).await.len(), 1);
    // First delete attempts fail; the cleanup is retried until it succeeds.
    *h.provider.delete_status.lock().unwrap() = 503;
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}"), None).await.status, 204);
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}"), None).await.status, 404);
    assert!(h.wait_until(|| h.provider.calls("DELETE", "/files/") >= 1).await);
    for id in [a, b, c] {
        let row = h.attachment(id).await;
        assert!(row.deleted_at.is_none() || row.cleanup_status.is_some());
        assert_ne!(row.cleanup_status.as_deref(), Some("done"));
    }
    *h.provider.delete_status.lock().unwrap() = 200;
    let mut all_done = false;
    for _ in 0..400 {
        let mut done = true;
        for id in [a, b, c] {
            if h.attachment(id).await.cleanup_status.as_deref() != Some("done") {
                done = false;
            }
        }
        if done && h.provider.calls("DELETE", "/vector_stores/") >= 1 {
            all_done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(all_done, "files and vector store cleaned");
    let deleted: Vec<String> = h
        .provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| r.uri.clone())
        .collect();
    for id in [a, b, c] {
        let fid = h.attachment(id).await.provider_file_id.unwrap();
        assert!(deleted.iter().any(|u| u.contains(&fid)), "{fid} deleted");
    }
    let vs = h.vector_stores(chat).await;
    assert!(vs.is_empty() || vs[0].vector_store_id.as_ref().is_some_and(|id| deleted.iter().any(|u| u.ends_with(id))));
    // Deleted chats accept no further operations.
    assert_eq!(h.send_message(U1, chat, json!({"content": "x"})).await.status, 404);
    assert_eq!(h.upload(U1, chat, "d.txt", "text/plain", b"d").await.status, 404);
    // Audit: chat deletion leaves other chats alone.
    let other = h.create_chat(U1, None).await;
    assert_eq!(h.call(U1, "GET", &format!("/chats/{other}"), None).await.status, 200);

    // Deleting a chat while a turn runs cancels nothing it should not and still cleans up.
    let chat = h.create_chat(U1, None).await;
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x"}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}"), None).await.status, 204);
    let _ = tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into());
    let _ = read_until(&mut body, &mut buf, |n, _| n == "done" || n == "error").await;
    drop(body);
    for _ in 0..200 {
        if h.turns(chat).await.iter().all(|t| t.state != "running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.turns(chat).await.iter().all(|t| t.state != "running"), "turn finalized");
    let q = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(q.reserved_credits_micro, 0, "reserve released");
}

async fn wait_summary(h: &Harness, chat: Uuid) -> Option<mini_chat::infra::db::entity::thread_summaries::Model> {
    for _ in 0..400 {
        if let Some(s) = h.summary(chat).await {
            return Some(s);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

/// Thread summary generation, failure/retry and mutation-driven invalidation.
#[tokio::test]
async fn thread_summary_generation_retry_and_invalidation() {
    let h = Harness::new().await;
    // The first summary call fails; the outbox retries it.
    h.provider.summary_replies.lock().unwrap().push_back(Err(500));
    h.provider.summary_replies.lock().unwrap().push_back(Ok(json!({
        "output_text": "<analysis>thinking</analysis><summary>User discussed turns 0-2.</summary>",
        "usage": {"input_tokens": 200, "output_tokens": 30}
    })));
    let chat = h.create_chat(U1, Some("tiny")).await;
    let big = "w".repeat(3000);
    let mut rids = Vec::new();
    for i in 0..4 {
        let r = h.send_message(U1, chat, json!({"content": format!("turn{i} {big}")})).await;
        rids.push(Uuid::parse_str(r.event("stream_started").unwrap()["request_id"].as_str().unwrap()).unwrap());
    }
    wait_summary(&h, chat).await.expect("summary after retry");
    // Let all queued summary tasks settle.
    let mut seen = h.provider.summary_requests().len();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = h.provider.summary_requests().len();
        if n == seen {
            break;
        }
        seen = n;
    }
    let s = h.summary(chat).await.unwrap();
    assert!(
        ["User discussed turns 0-2.", "Summary text"].contains(&s.summary_text.as_str()),
        "analysis block stripped: {}",
        s.summary_text
    );
    assert!(s.token_estimate > 0);
    assert!(h.provider.summary_requests().len() >= 2, "failed attempt retried");
    let sreq = h.provider.summary_requests().pop().unwrap();
    assert_eq!(sreq["model"], "standard-1-provider", "configured summary model");
    assert_eq!(sreq["stream"], false);
    // Summarized messages are marked compressed; the summary covers up to the previous turn.
    let msgs = h.messages(chat).await;
    assert!(msgs.iter().any(|m| m.is_compressed));
    assert!(msgs.iter().filter(|m| m.request_id == Some(rids[3])).all(|m| !m.is_compressed));
    // The system task is billed and published as a system usage event.
    let mut sys = None;
    for _ in 0..200 {
        sys = h.policy.published.lock().unwrap().iter().find(|p| p.requester_type == "system").cloned();
        if sys.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sys = sys.expect("system usage event");
    assert_eq!(sys.system_task_type.as_deref(), Some("thread_summary_update"));
    assert!(sys.user_id.is_none() && sys.turn_id.is_none());
    // Messages API still returns all messages (compression is internal).
    let api = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    assert_eq!(api["items"].as_array().unwrap().len(), 8);

    // Invalidation: deleting turns the summary does not cover keeps it;
    // deleting a covered turn drops it and un-compresses the messages.
    let compressed = |msgs: &[mini_chat::infra::db::entity::messages::Model], r: Uuid| {
        msgs.iter().filter(|m| m.request_id == Some(r)).all(|m| m.is_compressed)
    };
    let msgs = h.messages(chat).await;
    let covered: Vec<bool> = rids.iter().map(|r| compressed(&msgs, *r)).collect();
    let k = covered.iter().rposition(|c| *c).expect("some turns are covered");
    assert!(covered[..=k].iter().all(|c| *c), "summary covers a prefix: {covered:?}");
    assert!(k < 3, "the latest turn is never covered");
    for r in rids[k + 1..].iter().rev() {
        assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{r}"), None).await.status, 204);
        assert!(h.summary(chat).await.is_some(), "uncovered turn deletion keeps the summary");
    }
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{}", rids[k]), None).await.status, 204);
    assert!(h.summary(chat).await.is_none(), "summary invalidated by mutation");
    assert!(h.messages(chat).await.iter().all(|m| !m.is_compressed));
    // The next request carries no summary and includes the un-compressed history.
    let r = h.send_message(U1, chat, json!({"content": "fresh"})).await;
    assert!(r.event("stream_started").unwrap().get("thread_summary_applied").is_none());
    let req = h.provider.chat_requests().pop().unwrap();
    let first = req["input"][0]["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(!first.starts_with("This conversation has earlier messages"), "{first}");
    if k > 0 {
        assert!(first.starts_with("turn0"), "un-compressed history is sent again: {first}");
    }

    // Exhausted retries leave no summary and do not break chatting.
    let h = Harness::with(Options { config: json!({"thread_summary_worker": {"max_attempts": 1}}), ..Options::default() }).await;
    for _ in 0..5 {
        h.provider.summary_replies.lock().unwrap().push_back(Err(500));
    }
    let chat = h.create_chat(U1, Some("tiny")).await;
    for i in 0..4 {
        assert_eq!(h.send_message(U1, chat, json!({"content": format!("turn{i} {big}")})).await.status, 200);
    }
    assert!(h.wait_until(|| !h.provider.summary_requests().is_empty()).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.summary(chat).await.is_none());
    assert!(h.messages(chat).await.iter().all(|m| !m.is_compressed));
    assert_eq!(h.send_message(U1, chat, json!({"content": "still works"})).await.status, 200);
}
