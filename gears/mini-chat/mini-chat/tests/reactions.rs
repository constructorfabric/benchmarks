//! T047: reactions on assistant messages.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn set_update_and_remove_reaction() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.send(chat, "hi").await;
    let msgs = h.messages(chat).await;
    let user_id = msgs[0]["id"].as_str().unwrap().to_owned();
    let asst = msgs[1]["id"].as_str().unwrap().to_owned();
    let url = format!("/mini-chat/v1/chats/{chat}/messages/{asst}/reaction");

    let (s, v, _) = h
        .req(&h.ctx(), "PUT", &url, Some(json!({"reaction": "like"})))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["message_id"], asst.as_str());
    assert_eq!(v["reaction"], "like");
    assert!(v["created_at"].is_string());
    assert_eq!(h.messages(chat).await[1]["my_reaction"], "like");

    // upsert
    let (s, v, _) = h
        .req(&h.ctx(), "PUT", &url, Some(json!({"reaction": "dislike"})))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["reaction"], "dislike");
    assert_eq!(h.messages(chat).await[1]["my_reaction"], "dislike");
    // idempotent repeat
    let (s, _, _) = h
        .req(&h.ctx(), "PUT", &url, Some(json!({"reaction": "dislike"})))
        .await;
    assert_eq!(s, StatusCode::OK);

    // reactions are per user: another user of the tenant cannot see the chat at all
    let (s, _, _) = h
        .req(
            &ctx_for(h.tenant, Uuid::new_v4()),
            "PUT",
            &url,
            Some(json!({"reaction": "like"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _, _) = h.req(&h.ctx(), "DELETE", &url, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(h.messages(chat).await[1]["my_reaction"].is_null());
    let (s, _, _) = h.req(&h.ctx(), "DELETE", &url, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "delete is idempotent");

    // user message is not a valid target
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "PUT",
            &format!("/mini-chat/v1/chats/{chat}/messages/{user_id}/reaction"),
            Some(json!({"reaction": "like"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("reaction_target"), "{v}");
}

#[tokio::test]
async fn reaction_validation_errors() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.send(chat, "hi").await;
    let asst = h.messages(chat).await[1]["id"].as_str().unwrap().to_owned();
    let url = format!("/mini-chat/v1/chats/{chat}/messages/{asst}/reaction");

    let (s, v, _) = h
        .req(&h.ctx(), "PUT", &url, Some(json!({"reaction": "love"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_field_reason(&v, "reaction", "INVALID_REACTION");
    // invalid value is reported before authorization
    h.authz
        .mode
        .store(PDP_DENY, std::sync::atomic::Ordering::SeqCst);
    let (s, _, _) = h
        .req(&h.ctx(), "PUT", &url, Some(json!({"reaction": "love"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.authz
        .mode
        .store(PDP_ALLOW, std::sync::atomic::Ordering::SeqCst);

    let (s, _, _) = h.req(&h.ctx(), "PUT", &url, Some(json!({}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    let missing = Uuid::new_v4();
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "PUT",
            &format!("/mini-chat/v1/chats/{chat}/messages/{missing}/reaction"),
            Some(json!({"reaction": "like"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        v["context"]["resource_type"]
            .as_str()
            .unwrap()
            .contains("mini_chat.message"),
        "{v}"
    );
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "PUT",
            &format!(
                "/mini-chat/v1/chats/{}/messages/{asst}/reaction",
                Uuid::new_v4()
            ),
            Some(json!({"reaction": "like"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        v["context"]["resource_type"]
            .as_str()
            .unwrap()
            .contains("mini_chat.chat"),
        "{v}"
    );
}
