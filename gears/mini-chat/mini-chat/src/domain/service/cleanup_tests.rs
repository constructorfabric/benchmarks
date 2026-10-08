#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use super::*;
use crate::domain::outbox_payloads::attachment_event_types;
use crate::domain::service::attachments::test_helpers::*;
use crate::domain::service::test_support::{TENANT_A, TestEnv, TestOptions, USER_A1};

fn msg(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: "test".into(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn attachment_event(tenant: Uuid, chat: Uuid, row: &attachment::Model) -> AttachmentCleanupEvent {
    AttachmentCleanupEvent {
        event_type: attachment_event_types::DELETED.to_owned(),
        tenant_id: tenant,
        chat_id: chat,
        attachment_id: row.id,
        provider_file_id: row.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: row.storage_backend.clone(),
        attachment_kind: row.attachment_kind.clone(),
        deleted_at: OffsetDateTime::now_utc(),
        secondary_ref: None,
    }
}

fn chat_event(tenant: Uuid, chat: Uuid) -> ChatCleanupEvent {
    ChatCleanupEvent {
        tenant_id: tenant,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        reason: "chat_soft_delete".into(),
        chat_deleted_at: OffsetDateTime::now_utc(),
    }
}

fn is_reject(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Reject(_))
}

async fn deleted_attachment(env: &TestEnv, chat: Uuid) -> attachment::Model {
    let id = insert_attachment(env, TENANT_A, chat, USER_A1, |a| {
        a.deleted_at = Set(Some(OffsetDateTime::now_utc()));
        a.cleanup_status = Set(Some("pending".into()));
    })
    .await;
    row(env, TENANT_A, id).await
}

// ── Attachment cleanup ─────────────────────────────────────────────────────

#[tokio::test]
async fn attachment_cleanup_deletes_file_and_marks_done() {
    let env = TestEnv::default_env().await;
    let h = AttachmentCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let a = deleted_attachment(&env, chat).await;
    let ev = attachment_event(TENANT_A, chat, &a);
    let r = h.handle(&msg(serde_json::to_vec(&ev).unwrap(), 0)).await;
    assert!(matches!(r, MessageResult::Ok));
    assert_eq!(
        calls(&env.storage, "delete_file"),
        vec![format!("delete_file {}", a.provider_file_id.clone().unwrap())]
    );
    let after = row(&env, TENANT_A, a.id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("done"));
    assert!(after.cleanup_updated_at.is_some());

    // Redelivery is a no-op.
    let r = h.handle(&msg(serde_json::to_vec(&ev).unwrap(), 0)).await;
    assert!(matches!(r, MessageResult::Ok));
    assert_eq!(calls(&env.storage, "delete_file").len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn attachment_cleanup_404_is_success_and_no_file_marks_done() {
    let env = TestEnv::default_env().await;
    let h = AttachmentCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.delete_error.lock() = Some(StorageError::Http {
        status: 404,
        message: "gone".into(),
    });
    let a = deleted_attachment(&env, chat).await;
    let r = h
        .handle(&msg(serde_json::to_vec(&attachment_event(TENANT_A, chat, &a)).unwrap(), 0))
        .await;
    assert!(matches!(r, MessageResult::Ok));
    assert_eq!(row(&env, TENANT_A, a.id).await.cleanup_status.as_deref(), Some("done"));

    let id = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.provider_file_id = Set(None);
        a.cleanup_status = Set(Some("pending".into()));
    })
    .await;
    let b = row(&env, TENANT_A, id).await;
    let r = h
        .handle(&msg(serde_json::to_vec(&attachment_event(TENANT_A, chat, &b)).unwrap(), 0))
        .await;
    assert!(matches!(r, MessageResult::Ok));
    assert_eq!(row(&env, TENANT_A, id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(calls(&env.storage, "delete_file").len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn attachment_cleanup_failure_retries_then_fails_terminally() {
    let mut opts = TestOptions::default();
    opts.cfg.cleanup_worker.max_attempts = 3;
    let env = TestEnv::new(opts).await;
    let h = AttachmentCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.delete_error.lock() = Some(StorageError::Http {
        status: 500,
        message: "boom".into(),
    });
    let a = deleted_attachment(&env, chat).await;
    let payload = serde_json::to_vec(&attachment_event(TENANT_A, chat, &a)).unwrap();
    for attempt in 1..=2 {
        let r = h.handle(&msg(payload.clone(), 0)).await;
        assert!(matches!(r, MessageResult::Retry), "attempt {attempt}");
        let after = row(&env, TENANT_A, a.id).await;
        assert_eq!(after.cleanup_attempts, attempt);
        assert_eq!(after.cleanup_status.as_deref(), Some("pending"));
        assert!(after.last_cleanup_error.as_deref().unwrap().contains("500"));
    }
    let r = h.handle(&msg(payload, 0)).await;
    assert!(is_reject(&r), "{r:?}");
    let after = row(&env, TENANT_A, a.id).await;
    assert_eq!(after.cleanup_attempts, 3);
    assert_eq!(after.cleanup_status.as_deref(), Some("failed"));
    env.shutdown().await;
}

#[tokio::test]
async fn attachment_cleanup_skips_deleted_chat_and_rejects_garbage() {
    let env = TestEnv::default_env().await;
    let h = AttachmentCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let a = deleted_attachment(&env, chat).await;
    soft_delete_chat(&env, TENANT_A, chat).await;
    let r = h
        .handle(&msg(serde_json::to_vec(&attachment_event(TENANT_A, chat, &a)).unwrap(), 0))
        .await;
    assert!(matches!(r, MessageResult::Ok));
    assert!(calls(&env.storage, "delete_file").is_empty());
    assert_eq!(row(&env, TENANT_A, a.id).await.cleanup_status.as_deref(), Some("pending"));

    let r = h.handle(&msg(b"{not json".to_vec(), 0)).await;
    assert!(is_reject(&r));
    env.shutdown().await;
}

#[tokio::test]
async fn attachment_cleanup_handles_abandoned_upload_rows() {
    let env = TestEnv::default_env().await;
    let h = AttachmentCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let id = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("failed".into());
        a.error_code = Set(Some("upload_abandoned".into()));
        a.cleanup_status = Set(Some("pending".into()));
    })
    .await;
    let a = row(&env, TENANT_A, id).await;
    let mut ev = attachment_event(TENANT_A, chat, &a);
    ev.event_type = attachment_event_types::UPLOAD_ABANDONED.into();
    let r = h.handle(&msg(serde_json::to_vec(&ev).unwrap(), 0)).await;
    assert!(matches!(r, MessageResult::Ok));
    let after = row(&env, TENANT_A, id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("done"));
    assert!(after.deleted_at.is_none());
    env.shutdown().await;
}

// ── Chat cleanup ───────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_cleanup_deletes_files_then_vector_store() {
    let env = TestEnv::default_env().await;
    let h = ChatCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let a = insert_attachment(&env, TENANT_A, chat, USER_A1, |_| {}).await;
    let b = insert_attachment(&env, TENANT_A, chat, USER_A1, |x| x.provider_file_id = Set(None)).await;
    insert_vector_store(&env, TENANT_A, chat, Some("vs_1"), "openai", OffsetDateTime::now_utc()).await;
    soft_delete_chat(&env, TENANT_A, chat).await;

    let r = h.handle(&msg(serde_json::to_vec(&chat_event(TENANT_A, chat)).unwrap(), 0)).await;
    assert!(matches!(r, MessageResult::Ok), "{r:?}");
    assert_eq!(calls(&env.storage, "delete_file").len(), 1);
    assert_eq!(calls(&env.storage, "delete_vs"), vec!["delete_vs vs_1".to_owned()]);
    assert_eq!(row(&env, TENANT_A, a).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row(&env, TENANT_A, b).await.cleanup_status.as_deref(), Some("done"));
    assert!(vector_stores(&env, TENANT_A, chat).await.is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn chat_cleanup_rejects_active_chat_and_garbage() {
    let env = TestEnv::default_env().await;
    let h = ChatCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let r = h.handle(&msg(serde_json::to_vec(&chat_event(TENANT_A, chat)).unwrap(), 0)).await;
    assert!(is_reject(&r));
    let r = h.handle(&msg(b"[]".to_vec(), 0)).await;
    assert!(is_reject(&r));
    env.shutdown().await;
}

#[tokio::test]
async fn chat_cleanup_retries_while_files_pending_then_gives_up_per_file() {
    let mut opts = TestOptions::default();
    opts.cfg.cleanup_worker.max_attempts = 2;
    let env = TestEnv::new(opts).await;
    let h = ChatCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let a = insert_attachment(&env, TENANT_A, chat, USER_A1, |_| {}).await;
    insert_vector_store(&env, TENANT_A, chat, Some("vs_1"), "openai", OffsetDateTime::now_utc()).await;
    soft_delete_chat(&env, TENANT_A, chat).await;
    *env.storage.delete_error.lock() = Some(StorageError::Transport("down".into()));
    let payload = serde_json::to_vec(&chat_event(TENANT_A, chat)).unwrap();

    let r = h.handle(&msg(payload.clone(), 0)).await;
    assert!(matches!(r, MessageResult::Retry));
    assert_eq!(row(&env, TENANT_A, a).await.cleanup_attempts, 1);
    assert!(calls(&env.storage, "delete_vs").is_empty(), "no store delete while files are pending");

    // Second failure reaches max_attempts: the file becomes `failed`, the store is deleted.
    let r = h.handle(&msg(payload, 1)).await;
    assert!(matches!(r, MessageResult::Ok), "{r:?}");
    assert_eq!(row(&env, TENANT_A, a).await.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(calls(&env.storage, "delete_vs").len(), 1);
    assert!(vector_stores(&env, TENANT_A, chat).await.is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn chat_cleanup_vector_store_failure_retries_until_max_deliveries() {
    let mut opts = TestOptions::default();
    opts.cfg.cleanup_worker.max_attempts = 3;
    let env = TestEnv::new(opts).await;
    let h = ChatCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    insert_vector_store(&env, TENANT_A, chat, Some("vs_1"), "openai", OffsetDateTime::now_utc()).await;
    soft_delete_chat(&env, TENANT_A, chat).await;
    *env.storage.vs_delete_error.lock() = Some(StorageError::Http {
        status: 500,
        message: "x".into(),
    });
    let payload = serde_json::to_vec(&chat_event(TENANT_A, chat)).unwrap();
    assert!(matches!(h.handle(&msg(payload.clone(), 0)).await, MessageResult::Retry));
    assert!(matches!(h.handle(&msg(payload.clone(), 1)).await, MessageResult::Retry));
    assert!(is_reject(&h.handle(&msg(payload.clone(), 2)).await));
    assert_eq!(vector_stores(&env, TENANT_A, chat).await.len(), 1, "row kept for replay");

    // 404 counts as success.
    *env.storage.vs_delete_error.lock() = Some(StorageError::Http {
        status: 404,
        message: "x".into(),
    });
    assert!(matches!(h.handle(&msg(payload, 0)).await, MessageResult::Ok));
    assert!(vector_stores(&env, TENANT_A, chat).await.is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn chat_cleanup_drops_null_placeholder_without_provider_call() {
    let env = TestEnv::default_env().await;
    let h = ChatCleanupHandler::new(Arc::clone(&env.deps));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    insert_vector_store(&env, TENANT_A, chat, None, "openai", OffsetDateTime::now_utc()).await;
    soft_delete_chat(&env, TENANT_A, chat).await;
    let r = h.handle(&msg(serde_json::to_vec(&chat_event(TENANT_A, chat)).unwrap(), 0)).await;
    assert!(matches!(r, MessageResult::Ok));
    assert!(calls(&env.storage, "delete_vs").is_empty());
    assert!(vector_stores(&env, TENANT_A, chat).await.is_empty());
    env.shutdown().await;
}
