//! Attachment cleanup and chat cleanup handler tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use http::StatusCode;
use sea_orm::ActiveValue::Set;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use toolkit_db::secure::secure_insert;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::clock;
use crate::domain::attachments::test_support::{
    file_deletes, outbox_message, png, requests, row, rows, stop_outbox, upload, upload_doc, vector_store_rows,
};
use crate::domain::attachments::{EVENT_ATTACHMENT_DELETED, cleanup_payload};
use crate::infra::db::entities::chat_vector_store;
use crate::infra::outbox::handlers::attachment_cleanup::AttachmentCleanupHandler;
use crate::infra::outbox::handlers::chat_cleanup::ChatCleanupHandler;
use crate::infra::outbox::payloads::ChatCleanupPayload;
use crate::infra::outbox::{PAYLOAD_ATTACHMENT_CLEANUP, PAYLOAD_CHAT_CLEANUP};
use crate::testing::{TENANT_A, TestApp, ctx_a1};

/// App with a stopped pipeline, a chat and one deleted (cleanup pending) document.
async fn deleted_attachment(cfg: impl FnOnce(&mut crate::config::MiniChatConfig)) -> (TestApp, Uuid, Uuid) {
    let mut t = TestApp::with_config(cfg).await;
    stop_outbox(&mut t).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let id = upload_doc(&t, &c, chat).await;
    let (st, _, _) = t.call(&c, "DELETE", &format!("/mini-chat/v1/chats/{chat}/attachments/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let r = row(&t, id).await;
    assert_eq!(r.cleanup_status.as_deref(), Some("pending"));
    (t, chat, id)
}

async fn run_attachment_handler(t: &TestApp, id: Uuid) -> MessageResult {
    let r = row(t, id).await;
    let payload = cleanup_payload(EVENT_ATTACHMENT_DELETED, &r, clock::now());
    AttachmentCleanupHandler::new(Arc::clone(&t.app)).handle(&outbox_message(&payload, PAYLOAD_ATTACHMENT_CLEANUP, 0)).await
}

fn chat_payload(chat: Uuid) -> ChatCleanupPayload {
    ChatCleanupPayload {
        tenant_id: TENANT_A,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        reason: "chat_soft_delete".to_owned(),
        chat_deleted_at: clock::now(),
    }
}

async fn run_chat_handler(t: &TestApp, chat: Uuid, attempts: i16) -> MessageResult {
    ChatCleanupHandler::new(Arc::clone(&t.app))
        .handle(&outbox_message(&chat_payload(chat), PAYLOAD_CHAT_CLEANUP, attempts))
        .await
}

async fn delete_chat(t: &TestApp, chat: Uuid) {
    let (st, _, _) = t.call(&ctx_a1(), "DELETE", &format!("/mini-chat/v1/chats/{chat}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

// ───────────────────────────── attachment cleanup ─────────────────────────────

#[tokio::test]
async fn attachment_cleanup_deletes_the_provider_file() {
    let (t, _chat, id) = deleted_attachment(|_| {}).await;
    let file_id = row(&t, id).await.provider_file_id.unwrap();
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Ok));
    let r = row(&t, id).await;
    assert_eq!(r.cleanup_status.as_deref(), Some("done"));
    assert!(r.cleanup_updated_at.is_some());
    let deletes = requests(&t, "DELETE", "/files/");
    assert_eq!(deletes.len(), 1);
    assert!(deletes[0].uri.ends_with(&format!("/v1/files/{file_id}")));
    // Redelivery is a no-op.
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Ok));
    assert_eq!(file_deletes(&t), 1);
}

#[tokio::test]
async fn provider_404_counts_as_deleted() {
    let (t, _chat, id) = deleted_attachment(|_| {}).await;
    t.provider.delete_statuses.lock().unwrap().push_back(404);
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Ok));
    assert_eq!(row(&t, id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn failing_deletes_count_attempts_until_failed() {
    let (t, _chat, id) = deleted_attachment(|c| c.cleanup_worker.max_attempts = 3).await;
    t.provider.delete_statuses.lock().unwrap().extend([500, 400, 503]);
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Retry));
    let r = row(&t, id).await;
    assert_eq!((r.cleanup_status.as_deref(), r.cleanup_attempts), (Some("pending"), 1));
    assert!(r.last_cleanup_error.is_some());
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Retry));
    assert_eq!(row(&t, id).await.cleanup_attempts, 2);
    match run_attachment_handler(&t, id).await {
        MessageResult::Reject(reason) => assert!(reason.contains("max attempts (3)"), "{reason}"),
        other => panic!("expected reject, got {other:?}"),
    }
    let r = row(&t, id).await;
    assert_eq!((r.cleanup_status.as_deref(), r.cleanup_attempts), (Some("failed"), 3));
    assert!(r.cleanup_updated_at.is_some());
    // Terminal: no further provider calls.
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Ok));
    assert_eq!(file_deletes(&t), 3);
}

#[tokio::test]
async fn malformed_payload_is_rejected() {
    let mut t = TestApp::new().await;
    stop_outbox(&mut t).await;
    let h = AttachmentCleanupHandler::new(Arc::clone(&t.app));
    let msg = outbox_message(&serde_json::json!({"nope": true}), PAYLOAD_ATTACHMENT_CLEANUP, 0);
    assert!(matches!(h.handle(&msg).await, MessageResult::Reject(_)));
    let h = ChatCleanupHandler::new(Arc::clone(&t.app));
    assert!(matches!(h.handle(&msg).await, MessageResult::Reject(_)));
}

#[tokio::test]
async fn attachment_of_deleted_chat_is_left_to_chat_cleanup() {
    let (t, chat, id) = deleted_attachment(|_| {}).await;
    delete_chat(&t, chat).await;
    assert!(matches!(run_attachment_handler(&t, id).await, MessageResult::Ok));
    assert_eq!(file_deletes(&t), 0);
    assert_eq!(row(&t, id).await.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn attachment_cleanup_through_the_pipeline() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let id = upload_doc(&t, &c, chat).await;
    let (st, _, _) = t.call(&c, "DELETE", &format!("/mini-chat/v1/chats/{chat}/attachments/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    t.eventually("attachment cleanup done", || async { row(&t, id).await.cleanup_status.as_deref() == Some("done") }).await;
    assert_eq!(file_deletes(&t), 1);
}

// ───────────────────────────── chat cleanup ─────────────────────────────

#[tokio::test]
async fn chat_deletion_cleans_files_and_vector_store_through_the_pipeline() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let doc = upload_doc(&t, &c, chat).await;
    let (st, _, json) = upload(&t, &c, chat, "p.png", "image/png", &png(16, 16)).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    let vs_id = vector_store_rows(&t, chat).await[0].vector_store_id.clone().unwrap();
    delete_chat(&t, chat).await;
    t.eventually("chat cleanup done", || async {
        vector_store_rows(&t, chat).await.is_empty()
            && rows(&t, chat).await.iter().all(|r| r.cleanup_status.as_deref() == Some("done"))
    })
    .await;
    assert_eq!(file_deletes(&t), 2);
    let vs_deletes = requests(&t, "DELETE", "/vector_stores/");
    assert_eq!(vs_deletes.len(), 1);
    assert!(vs_deletes[0].uri.ends_with(&format!("/v1/vector_stores/{vs_id}")));
    assert!(row(&t, doc).await.deleted_at.is_none(), "chat deletion does not soft-delete attachment rows");
}

#[tokio::test]
async fn chat_cleanup_rejects_live_chats() {
    let mut t = TestApp::new().await;
    stop_outbox(&mut t).await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    match run_chat_handler(&t, chat, 0).await {
        MessageResult::Reject(r) => assert_eq!(r, "chat is not soft-deleted"),
        other => panic!("expected reject, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_cleanup_waits_for_pending_files_before_the_vector_store() {
    let mut t = TestApp::with_config(|c| c.cleanup_worker.max_attempts = 3).await;
    stop_outbox(&mut t).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let a = upload_doc(&t, &c, chat).await;
    let b = upload_doc(&t, &c, chat).await;
    delete_chat(&t, chat).await;
    // First file delete fails, second succeeds.
    t.provider.delete_statuses.lock().unwrap().extend([500, 200]);
    assert!(matches!(run_chat_handler(&t, chat, 0).await, MessageResult::Retry));
    assert!(requests(&t, "DELETE", "/vector_stores/").is_empty());
    let pending = rows(&t, chat).await.into_iter().filter(|r| r.cleanup_status.as_deref() == Some("pending")).count();
    assert_eq!(pending, 1);
    // Next delivery finishes the file, then the vector store.
    assert!(matches!(run_chat_handler(&t, chat, 1).await, MessageResult::Ok));
    assert!(rows(&t, chat).await.iter().all(|r| r.cleanup_status.as_deref() == Some("done")));
    assert_eq!(requests(&t, "DELETE", "/vector_stores/").len(), 1);
    assert!(vector_store_rows(&t, chat).await.is_empty());
    assert_eq!(row(&t, a).await.cleanup_attempts + row(&t, b).await.cleanup_attempts, 1);
}

#[tokio::test]
async fn chat_cleanup_continues_after_terminal_file_failure() {
    let mut t = TestApp::with_config(|c| c.cleanup_worker.max_attempts = 1).await;
    stop_outbox(&mut t).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let a = upload_doc(&t, &c, chat).await;
    delete_chat(&t, chat).await;
    t.provider.delete_statuses.lock().unwrap().push_back(500);
    assert!(matches!(run_chat_handler(&t, chat, 0).await, MessageResult::Ok));
    assert_eq!(row(&t, a).await.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(requests(&t, "DELETE", "/vector_stores/").len(), 1);
    assert!(vector_store_rows(&t, chat).await.is_empty());
}

#[tokio::test]
async fn vector_store_delete_failure_retries_then_rejects_at_max_attempts() {
    let mut t = TestApp::with_config(|c| c.cleanup_worker.max_attempts = 3).await;
    stop_outbox(&mut t).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    upload_doc(&t, &c, chat).await;
    delete_chat(&t, chat).await;
    t.provider.delete_statuses.lock().unwrap().extend([200, 500, 500]);
    assert!(matches!(run_chat_handler(&t, chat, 0).await, MessageResult::Retry));
    assert_eq!(vector_store_rows(&t, chat).await.len(), 1);
    match run_chat_handler(&t, chat, 2).await {
        MessageResult::Reject(r) => assert_eq!(r, "vector store delete: max attempts (3) reached"),
        other => panic!("expected reject, got {other:?}"),
    }
    assert_eq!(vector_store_rows(&t, chat).await.len(), 1, "row kept for a dead-letter replay");
    // A replay with a 404 completes.
    t.provider.delete_statuses.lock().unwrap().push_back(404);
    assert!(matches!(run_chat_handler(&t, chat, 0).await, MessageResult::Ok));
    assert!(vector_store_rows(&t, chat).await.is_empty());
}

#[tokio::test]
async fn null_placeholder_row_is_removed_without_provider_call() {
    let mut t = TestApp::new().await;
    stop_outbox(&mut t).await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let am = chat_vector_store::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat),
        vector_store_id: Set(None),
        provider: Set("openai".to_owned()),
        file_count: Set(0),
        created_at: Set(clock::now()),
    };
    {
        let conn = t.app.db.conn().unwrap();
        secure_insert::<chat_vector_store::Entity>(am, &AccessScope::for_tenant(TENANT_A), &conn).await.unwrap();
    }
    delete_chat(&t, chat).await;
    assert!(matches!(run_chat_handler(&t, chat, 0).await, MessageResult::Ok));
    assert!(vector_store_rows(&t, chat).await.is_empty());
    assert!(requests(&t, "DELETE", "/vector_stores/").is_empty());
}
