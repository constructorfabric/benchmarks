//! T046: turn status read model.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use tokio::sync::Notify;
use uuid::Uuid;

#[tokio::test]
async fn turn_status_mapping() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let url = |rid: Uuid| format!("/mini-chat/v1/chats/{chat}/turns/{rid}");

    // running
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["x".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let rid = Uuid::new_v4();
    let mut s = h
        .open_send(chat, json!({"content": "a", "request_id": rid}))
        .await;
    assert!(s.until("delta").await);
    let (st, v) = h.get(&url(rid)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "running");
    assert!(v.get("error_code").is_none() && v.get("assistant_message_id").is_none());
    assert_eq!(v["request_id"], rid.to_string());
    assert!(v["updated_at"].is_string());
    gate.notify_one();
    assert!(s.until("done").await);
    h.wait_turn_terminal(chat).await;

    // done
    let (_, v) = h.get(&url(rid)).await;
    assert_eq!(v["state"], "done");
    let msgs = h.messages(chat).await;
    assert_eq!(v["assistant_message_id"], msgs[1]["id"]);
    assert!(v.get("error_code").is_none());

    // error
    h.provider.push(Script::Failed {
        parts: vec![],
        message: "x".into(),
        usage: None,
    });
    let rid2 = Uuid::new_v4();
    h.send_body(chat, json!({"content": "b", "request_id": rid2}))
        .await;
    let (_, v) = h.get(&url(rid2)).await;
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "provider_error");

    // unknown -> 404 turn resource
    let (st, v) = h.get(&url(Uuid::new_v4())).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(
        v["context"]["resource_type"]
            .as_str()
            .unwrap()
            .contains("mini_chat.turn"),
        "{v}"
    );

    // deleted -> 404
    let (st, _, _) = h.req(&h.ctx(), "DELETE", &url(rid2), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = h.get(&url(rid2)).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
