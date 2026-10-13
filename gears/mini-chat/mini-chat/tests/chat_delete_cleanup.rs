#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Chat deletion schedules provider cleanup (DESIGN "Cleanup on Chat Deletion"): in the
//! soft-delete transaction the chat's attachments are marked `cleanup_status = 'pending'` and a
//! chat-cleanup outbox message is enqueued.

mod common;

use common::{chat_id, create_default_chat, delete_chat, get_chat, uuid_hex};
use http::StatusCode;
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

async fn exec(env: &TestEnv, sql: String) {
    env.db()
        .execute_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("statement must succeed");
}

async fn scalar_text(env: &TestEnv, sql: String) -> Option<String> {
    let row = env
        .db()
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("query must succeed")?;
    row.try_get::<Option<String>>("", "v").unwrap()
}

fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

/// Inserts an attachment row of the chat and returns its id.
async fn insert_attachment(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    cleanup: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    let chat = Uuid::parse_str(chat).unwrap();
    let cleanup = cleanup.map_or_else(|| "NULL".to_owned(), |status| format!("'{status}'"));
    exec(
        env,
        format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
             status, attachment_kind, cleanup_status, created_at, updated_at) VALUES \
             ({}, {}, {}, {}, 'a.txt', 'ready', 'document', {cleanup}, \
             '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
            blob(id),
            blob(user.tenant_id),
            blob(chat),
            blob(user.user_id),
        ),
    )
    .await;
    id
}

async fn cleanup_status(env: &TestEnv, attachment: Uuid) -> Option<String> {
    scalar_text(
        env,
        format!(
            "SELECT cleanup_status AS v FROM attachments WHERE hex(id) = '{}'",
            uuid_hex(attachment)
        ),
    )
    .await
}

#[tokio::test]
async fn deleting_a_chat_enqueues_one_chat_cleanup_message() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let id = chat_id(&create_default_chat(&env, &user).await);

    let resp = delete_chat(&env, &user, &id).await;

    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    let payloads = env.outbox_payloads("mini-chat.chat_cleanup").await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let payload = &payloads[0];
    assert_eq!(payload["tenant_id"], json!(user.tenant_id));
    assert_eq!(payload["chat_id"], json!(id));
    assert_eq!(payload["reason"], json!("chat_soft_delete"));
    let request_id = Uuid::parse_str(payload["system_request_id"].as_str().unwrap()).unwrap();
    assert_eq!(request_id.get_version_num(), 4);
    let deleted_at = OffsetDateTime::parse(payload["chat_deleted_at"].as_str().unwrap(), &Rfc3339);
    assert!(deleted_at.is_ok(), "{payload}");
}

#[tokio::test]
async fn nothing_is_enqueued_when_the_delete_does_not_happen() {
    let env = TestEnv::new().await;
    let owner = TestUser::new();
    let intruder = TestUser::in_tenant(owner.tenant_id);
    let id = chat_id(&create_default_chat(&env, &owner).await);

    delete_chat(&env, &intruder, &id).await;
    delete_chat(&env, &owner, &Uuid::new_v4().to_string()).await;
    delete_chat(&env, &owner, &id).await;
    delete_chat(&env, &owner, &id).await;

    assert_eq!(env.outbox_payloads("mini-chat.chat_cleanup").await.len(), 1);
}

#[tokio::test]
async fn the_cleanup_queue_name_comes_from_the_configuration() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "outbox": { "chat_cleanup_queue_name": "custom.chat-cleanup" } }))
        .build()
        .await;
    let user = TestUser::new();
    let id = chat_id(&create_default_chat(&env, &user).await);

    delete_chat(&env, &user, &id).await;

    assert_eq!(env.outbox_payloads("custom.chat-cleanup").await.len(), 1);
    assert!(
        env.outbox_payloads("mini-chat.chat_cleanup")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn the_chats_attachments_are_marked_for_provider_cleanup() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let id = chat_id(&create_default_chat(&env, &user).await);
    let other = chat_id(&create_default_chat(&env, &user).await);
    let fresh = insert_attachment(&env, &user, &id, None).await;
    let already_done = insert_attachment(&env, &user, &id, Some("done")).await;
    let foreign = insert_attachment(&env, &user, &other, None).await;

    delete_chat(&env, &user, &id).await;

    assert_eq!(
        cleanup_status(&env, fresh).await.as_deref(),
        Some("pending")
    );
    assert_eq!(
        cleanup_status(&env, already_done).await.as_deref(),
        Some("done")
    );
    assert_eq!(
        cleanup_status(&env, foreign).await,
        None,
        "other chats are untouched"
    );
}

#[tokio::test]
async fn a_delete_leaves_the_chat_hidden_and_its_row_in_place() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let id = chat_id(&create_default_chat(&env, &user).await);
    delete_chat(&env, &user, &id).await;

    let resp = get_chat(&env, &user, &id).await;

    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    let deleted_at = scalar_text(
        &env,
        format!(
            "SELECT deleted_at AS v FROM chats WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(&id).unwrap())
        ),
    )
    .await;
    assert!(deleted_at.is_some(), "deleted_at must be set");
    assert_eq!(env.outbox_payloads("mini-chat.chat_cleanup").await.len(), 1);
}
