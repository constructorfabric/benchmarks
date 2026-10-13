//! AC: Authorization, Principles (tenant and owner isolation), Error Mapping (canonical contract).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::*;
use serde_json::json;
use uuid::Uuid;

/// Every chat-scoped endpoint of a foreign chat answers 404 (no existence leak).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_chat_is_not_found_everywhere() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let ev = h.say(ALICE, chat, "hello").await;
    let start = find(&ev, "stream_started").clone();
    let rid = start["request_id"].as_str().unwrap().to_owned();
    let msgs = h.messages(chat).await;
    let assistant = msgs.iter().find(|m| m.role == "assistant").unwrap().id;
    let up = h.upload(ALICE, chat, "a.txt", "text/plain", b"some text").await;
    assert_eq!(up.status, 201, "{}", up.text());
    let att = up.json()["id"].as_str().unwrap().to_owned();

    for who in [BOB, CAROL] {
        let checks: Vec<(&str, String, Option<serde_json::Value>)> = vec![
            ("GET", format!("/chats/{chat}"), None),
            ("PATCH", format!("/chats/{chat}"), Some(json!({"title": "x"}))),
            ("GET", format!("/chats/{chat}/messages"), None),
            ("POST", format!("/chats/{chat}/messages:stream"), Some(json!({"content": "hi"}))),
            ("GET", format!("/chats/{chat}/attachments/{att}"), None),
            ("DELETE", format!("/chats/{chat}/attachments/{att}"), None),
            ("GET", format!("/chats/{chat}/turns/{rid}"), None),
            ("POST", format!("/chats/{chat}/turns/{rid}/retry"), None),
            ("PATCH", format!("/chats/{chat}/turns/{rid}"), Some(json!({"content": "edit"}))),
            ("DELETE", format!("/chats/{chat}/turns/{rid}"), None),
            ("PUT", format!("/chats/{chat}/messages/{assistant}/reaction"), Some(json!({"reaction": "like"}))),
            ("DELETE", format!("/chats/{chat}/messages/{assistant}/reaction"), None),
            ("DELETE", format!("/chats/{chat}"), None),
        ];
        for (m, p, b) in checks {
            let r = h.send(who, m, &p, b).await;
            assert_eq!(r.status, 404, "{m} {p} as {who:?}: {}", r.text());
        }
        let r = h.upload(who, chat, "b.txt", "text/plain", b"x").await;
        assert_eq!(r.status, 404, "upload as {who:?}");
    }
    // Owner still sees everything intact; no provider call was made for foreign requests.
    assert_eq!(h.provider.chat_requests().len(), 1);
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["message_count"], json!(2));
    assert!(h.attachment(Uuid::parse_str(&att).unwrap()).await.deleted_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pdp_deny_is_403() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, None).await;
    h.set_pdp(PDP_DENY);
    for (m, p, b) in [
        ("POST", "/chats".to_owned(), Some(json!({}))),
        ("GET", "/chats".to_owned(), None),
        ("GET", format!("/chats/{chat}"), None),
        ("POST", format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x"}))),
        ("GET", "/models".to_owned(), None),
        ("GET", "/quota/status".to_owned(), None),
    ] {
        let r = h.send(ALICE, m, &p, b).await;
        let pr = r.problem(403);
        assert!(pr["type"].as_str().unwrap().contains("permission_denied"), "{pr}");
    }
    assert!(h.provider.chat_requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pdp_failure_is_503_with_retry_after() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, None).await;
    h.set_pdp(PDP_FAIL);
    for (m, p, b) in [
        ("GET", format!("/chats/{chat}"), None),
        ("POST", format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x"}))),
    ] {
        let r = h.send(ALICE, m, &p, b).await;
        let pr = r.problem(503);
        assert!(r.headers.get("retry-after").is_some() || pr["context"]["retry_after_seconds"].is_number(), "{pr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_path_and_body_errors() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, None).await;
    // invalid UUID in path
    let r = h.send(ALICE, "GET", "/chats/not-a-uuid", None).await;
    r.problem(400);
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/xyz"), None).await;
    r.problem(400);
    // malformed JSON
    let r = h.raw(ALICE, "POST", "/chats", Some("application/json"), b"{not json".to_vec()).await;
    let p = r.problem(400);
    assert!(p["type"].as_str().unwrap().contains("invalid_argument"), "{p}");
    // wrong schema
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": 5}))).await;
    r.problem(422);
    let r = h.send(ALICE, "POST", "/chats", Some(json!({"title": ["not", "a", "string"]}))).await;
    r.problem(422);
    // wrong content type
    let r = h.raw(ALICE, "POST", "/chats", Some("text/plain"), b"{}".to_vec()).await;
    r.problem(415);
    // nonexistent chat
    let r = h.send(ALICE, "GET", &format!("/chats/{}", Uuid::new_v4()), None).await;
    let p = r.problem(404);
    assert_eq!(p["context"]["resource_type"], json!("gts.cf.core.mini_chat.chat.v1~"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_tenant_other_user_cannot_list_or_read_messages() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "private").await;
    let r = h.send(BOB, "GET", "/chats", None).await;
    assert_eq!(r.json()["items"], json!([]));
    let r = h.send(BOB, "GET", &format!("/chats/{chat}/messages"), None).await;
    r.problem(404);
    // Quota status is per user.
    let r = h.send(BOB, "GET", "/quota/status", None).await;
    assert_eq!(r.status, 200);
    let tiers = r.json()["tiers"].as_array().unwrap().clone();
    for t in tiers {
        for p in t["periods"].as_array().unwrap() {
            assert_eq!(p["remaining_percentage"], json!(100), "{p}");
        }
    }
}
