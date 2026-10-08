#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "tests read rows unscoped through the raw connection"
)]

//! Reactions API (D "Message Reaction API", ADR-0004 reaction rows).

mod common;

use axum::http::StatusCode;
use mini_chat::infra::db::entity::message_reaction;
use mini_chat::infra::db::repos::{MessageRepo, ReactionRepo};
use sea_orm::EntityTrait;
use serde_json::{Value, json};
use uuid::Uuid;

use common::*;

const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";
const MESSAGE_RESOURCE: &str = "gts.cf.core.mini_chat.message.v1~";

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn reaction_path(chat: Uuid, msg: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages/{msg}/reaction")
}

/// Run one completed turn; returns `(user_message_id, assistant_message_id)`.
async fn turn(app: &TestApp, client: &UserClient<'_>, chat: Uuid) -> (Uuid, Uuid) {
    push_hello(app);
    let resp = client
        .post_json(&stream_path(chat), &json!({"content": "Question?"}))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.sse_events().last().unwrap().0, "done");
    let page = client.get(&messages_path(chat)).await.json();
    let list = page["items"].as_array().unwrap();
    let id_of = |role: &str| -> Uuid {
        list.iter().find(|m| m["role"] == role).unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    (id_of("user"), id_of("assistant"))
}

async fn list(client: &UserClient<'_>, chat: Uuid) -> Vec<Value> {
    let resp = client.get(&messages_path(chat)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    resp.json()["items"].as_array().unwrap().clone()
}

fn my_reaction_of(items: &[Value], msg: Uuid) -> Value {
    items.iter().find(|m| m["id"] == msg.to_string()).unwrap()["my_reaction"].clone()
}

async fn rows(app: &TestApp) -> Vec<message_reaction::Model> {
    message_reaction::Entity::find()
        .all(&app.raw)
        .await
        .unwrap()
}

fn assert_problem(resp: &TestResponse, status: StatusCode) -> Value {
    assert_eq!(resp.status, status, "{}", resp.text());
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("application/problem+json"),
        "{}",
        resp.text()
    );
    resp.json()
}

fn assert_not_found(resp: &TestResponse, resource: &str) {
    let body = assert_problem(resp, StatusCode::NOT_FOUND);
    assert_eq!(body["context"]["resource_type"], resource, "{body}");
}

fn assert_reaction_target(resp: &TestResponse) {
    let body = assert_problem(resp, StatusCode::BAD_REQUEST);
    let v = &body["context"]["violations"][0];
    assert_eq!(v["subject"], "reaction_target", "{body}");
    assert_eq!(v["type"], "STATE", "{body}");
}

#[tokio::test]
async fn set_like_then_dislike_replaces() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_, assistant) = turn(&app, &client, chat).await;
    let path = reaction_path(chat, assistant);

    let resp = client.put_json(&path, &json!({"reaction": "like"})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["message_id"], assistant.to_string());
    assert_eq!(body["reaction"], "like");
    assert!(body["created_at"].is_string(), "{body}");
    assert_eq!(
        my_reaction_of(&list(&client, chat).await, assistant),
        "like"
    );

    let resp = client
        .put_json(&path, &json!({"reaction": "dislike"}))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["message_id"], assistant.to_string());
    assert_eq!(body["reaction"], "dislike");

    let stored = rows(&app).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].reaction, "dislike");
    assert_eq!(stored[0].user_id, user);
    assert_eq!(stored[0].tenant_id, tenant);
    assert_eq!(stored[0].message_id, assistant);
    assert_eq!(
        my_reaction_of(&list(&client, chat).await, assistant),
        "dislike"
    );
}

#[tokio::test]
async fn remove_is_idempotent_204() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_, assistant) = turn(&app, &client, chat).await;
    let path = reaction_path(chat, assistant);

    // No reaction yet: still 204.
    let resp = client.delete(&path).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    assert!(resp.body.is_empty());

    let resp = client.put_json(&path, &json!({"reaction": "like"})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(rows(&app).await.len(), 1);

    for _ in 0..2 {
        let resp = client.delete(&path).await;
        assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
        assert!(resp.body.is_empty());
    }
    assert!(rows(&app).await.is_empty());
    assert_eq!(
        my_reaction_of(&list(&client, chat).await, assistant),
        Value::Null
    );
}

#[tokio::test]
async fn user_message_rejected_400_reaction_target_for_put_and_delete() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (user_msg, _) = turn(&app, &client, chat).await;
    let path = reaction_path(chat, user_msg);

    let resp = client.put_json(&path, &json!({"reaction": "like"})).await;
    assert_reaction_target(&resp);
    let resp = client.delete(&path).await;
    assert_reaction_target(&resp);
    assert!(rows(&app).await.is_empty());
}

#[tokio::test]
async fn invalid_value_400_before_authz() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_, assistant) = turn(&app, &client, chat).await;
    let path = reaction_path(chat, assistant);

    // Invalid value: 400 even when the PDP denies and the chat is unknown.
    app.pdp.set_mode(PdpMode::Deny);
    for target in [path.clone(), reaction_path(Uuid::new_v4(), Uuid::new_v4())] {
        for value in ["love", "", "LIKE", " like"] {
            let resp = client.put_json(&target, &json!({"reaction": value})).await;
            let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
            let v = &body["context"]["field_violations"][0];
            assert_eq!(v["field"], "reaction", "{body}");
            assert_eq!(v["reason"], "INVALID_REACTION", "{body}");
        }
    }

    // A valid value reaches authorization and is denied.
    let resp = client.put_json(&path, &json!({"reaction": "like"})).await;
    assert_problem(&resp, StatusCode::FORBIDDEN);
    assert!(rows(&app).await.is_empty());
}

#[tokio::test]
async fn missing_field_422() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_, assistant) = turn(&app, &client, chat).await;
    let path = reaction_path(chat, assistant);

    for body in [json!({}), json!({"reaction": null}), json!({"reaction": 1})] {
        let resp = client.put_json(&path, &body).await;
        assert_problem(&resp, StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert!(rows(&app).await.is_empty());
}

#[tokio::test]
async fn unknown_message_404_message() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_, assistant) = turn(&app, &client, chat).await;
    let other_chat = create_chat(&client, "s1").await;
    let missing = reaction_path(chat, Uuid::new_v4());
    // A message of another chat of the same user is unknown in this chat.
    let wrong_chat = reaction_path(other_chat, assistant);

    for path in [missing, wrong_chat] {
        let resp = client.put_json(&path, &json!({"reaction": "like"})).await;
        assert_not_found(&resp, MESSAGE_RESOURCE);
        let resp = client.delete(&path).await;
        assert_not_found(&resp, MESSAGE_RESOURCE);
    }
    assert!(rows(&app).await.is_empty());
}

#[tokio::test]
async fn foreign_chat_404_chat() {
    let app = TestApp::builder().build().await;
    let tenant = Uuid::new_v4();
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    let alice_client = app.as_user(alice, tenant);
    let chat = create_chat(&alice_client, "s1").await;
    let (_, assistant) = turn(&app, &alice_client, chat).await;
    let bob_client = app.as_user(bob, tenant);
    let path = reaction_path(chat, assistant);

    let resp = bob_client
        .put_json(&path, &json!({"reaction": "like"}))
        .await;
    assert_not_found(&resp, CHAT_RESOURCE);
    let resp = bob_client.delete(&path).await;
    assert_not_found(&resp, CHAT_RESOURCE);

    let resp = bob_client
        .put_json(
            &reaction_path(Uuid::new_v4(), assistant),
            &json!({"reaction": "like"}),
        )
        .await;
    assert_not_found(&resp, CHAT_RESOURCE);
    assert!(rows(&app).await.is_empty());
}

#[tokio::test]
async fn my_reaction_is_per_user() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (user_msg, assistant) = turn(&app, &client, chat).await;

    // Another user's reaction on the same message (written directly: other
    // users cannot reach this chat through the API).
    let other = Uuid::new_v4();
    let conn = app.db.conn().unwrap();
    let message = MessageRepo
        .find_by_id(&conn, &tenant_scope(tenant, user), assistant)
        .await
        .unwrap()
        .unwrap();
    ReactionRepo
        .insert(
            &conn,
            &tenant_scope(tenant, other),
            reaction_row(&message, other, "dislike"),
        )
        .await
        .unwrap();

    let items = list(&client, chat).await;
    assert_eq!(my_reaction_of(&items, assistant), Value::Null);

    let resp = client
        .put_json(
            &reaction_path(chat, assistant),
            &json!({"reaction": "like"}),
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let items = list(&client, chat).await;
    assert_eq!(my_reaction_of(&items, assistant), "like");
    assert_eq!(my_reaction_of(&items, user_msg), Value::Null);

    // Removing the caller's reaction leaves the other user's row alone.
    let resp = client.delete(&reaction_path(chat, assistant)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    let stored = rows(&app).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].user_id, other);
    assert_eq!(stored[0].reaction, "dislike");
}
