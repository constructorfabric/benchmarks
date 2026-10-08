//! Authorization.

use std::sync::atomic::Ordering;

use serde_json::json;
use uuid::Uuid;

use crate::common::*;

/// Every operation is scoped to its owner; foreign or denied access is rejected consistently.
#[tokio::test]
async fn every_operation_scoped_and_rejected_consistently() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let r = h.send_message(U1, chat, json!({"content": "hello"})).await;
    let rid = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    let att = h.upload(U1, chat, "a.txt", "text/plain", b"x").await.json()["id"].as_str().unwrap().to_owned();
    let msgs = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    let asst = msgs["items"][1]["id"].as_str().unwrap().to_owned();
    let ops = vec![
        ("POST", "/chats".to_owned(), Some(json!({}))),
        ("GET", "/chats".to_owned(), None),
        ("GET", format!("/chats/{chat}"), None),
        ("PATCH", format!("/chats/{chat}"), Some(json!({"title": "x"}))),
        ("GET", format!("/chats/{chat}/messages"), None),
        ("POST", format!("/chats/{chat}/messages:stream"), Some(json!({"content": "hi"}))),
        ("GET", format!("/chats/{chat}/attachments/{att}"), None),
        ("DELETE", format!("/chats/{chat}/attachments/{att}"), None),
        ("GET", format!("/chats/{chat}/turns/{rid}"), None),
        ("POST", format!("/chats/{chat}/turns/{rid}/retry"), None),
        ("PATCH", format!("/chats/{chat}/turns/{rid}"), Some(json!({"content": "e"}))),
        ("DELETE", format!("/chats/{chat}/turns/{rid}"), None),
        ("PUT", format!("/chats/{chat}/messages/{asst}/reaction"), Some(json!({"reaction": "like"}))),
        ("DELETE", format!("/chats/{chat}/messages/{asst}/reaction"), None),
        ("GET", "/models".to_owned(), None),
        ("GET", "/models/premium-1".to_owned(), None),
        ("GET", "/quota/status".to_owned(), None),
        ("DELETE", format!("/chats/{chat}"), None),
    ];
    let calls = h.provider.chat_requests().len();

    // PDP denies -> 403 for every operation, nothing changes.
    h.pdp.mode.store(1, Ordering::SeqCst);
    for (m, p, b) in &ops {
        let r = h.call(U1, m, p, b.clone()).await;
        assert_eq!(r.status, 403, "{m} {p}: {}", r.text());
        assert_eq!(r.json()["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~");
        assert_eq!(r.reason(), "AUTHZ_DENIED");
    }
    assert_eq!(h.upload(U1, chat, "b.txt", "text/plain", b"x").await.status, 403);

    // PDP unavailable -> 503 with Retry-After, fail closed.
    h.pdp.mode.store(2, Ordering::SeqCst);
    for (m, p, b) in &ops {
        let r = h.call(U1, m, p, b.clone()).await;
        assert_eq!(r.status, 503, "{m} {p}: {}", r.text());
        assert_eq!(r.headers["retry-after"], "5");
        assert_eq!(r.json()["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~");
    }
    assert_eq!(h.upload(U1, chat, "b.txt", "text/plain", b"x").await.status, 503);
    h.pdp.mode.store(0, Ordering::SeqCst);
    assert_eq!(h.provider.chat_requests().len(), calls, "no provider call without authorization");
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}"), None).await.json()["title"], serde_json::Value::Null);
    assert_eq!(h.turns(chat).await.len(), 1);

    // Owner scoping: same-tenant other user and other tenant get 404 on every chat resource.
    for other in [U2, U3] {
        for (m, p, b) in &ops {
            if !p.contains(&chat.to_string()) {
                continue;
            }
            let r = h.call(other, m, p, b.clone()).await;
            assert_eq!(r.status, 404, "{m} {p} as {other:?}: {}", r.text());
        }
        let list = h.call(other, "GET", "/chats", None).await.json();
        assert!(list["items"].as_array().unwrap().is_empty());
        // Quota status is per user.
        let q = h.call(other, "GET", "/quota/status", None).await.json();
        assert!(q["tiers"][1]["periods"][0]["used_credits_micro"] == 0);
    }
    // Attachment ids of another user cannot be referenced, even in the same tenant.
    let chat2 = h.create_chat(U2, None).await;
    let r = h.send_message(U2, chat2, json!({"content": "x", "attachment_ids": [att]})).await;
    assert_eq!(r.status, 400);
    // Owner still has full access.
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}/turns/{rid}"), None).await.status, 200);
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}"), None).await.status, 204);
    let _ = Uuid::nil();
}
