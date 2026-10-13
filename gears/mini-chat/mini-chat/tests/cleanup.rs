//! Outbox cleanup of provider files and vector stores (spec §13.1; DESIGN §3.6
//! "Cleanup on Chat Deletion", §4 "Attachment Deletion" Phase 2, B.9.2).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use mini_chat::infra::db::entities::{attachment, chat, chat_vector_store};
use mini_chat::infra::outbox::payloads::{
    AttachmentCleanupEvent, AttachmentCleanupPayload, ChatCleanupPayload, SecondaryRef,
};
use mini_chat::infra::outbox::{AttachmentCleanupHandler, ChatCleanupHandler};
use mini_chat::testing::images::png;
use mini_chat::testing::seed::{self, NewAttachment};
use mini_chat::testing::{TestApp, TestUser};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use serde_json::json;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;
const PDF: &str = "application/pdf";
const PDF_BYTES: &[u8] = b"%PDF-1.4\n% mini-chat test document\n";

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

async fn app() -> TestApp {
    app_with_max_attempts(5).await
}

async fn app_with_max_attempts(max: u32) -> TestApp {
    TestApp::builder()
        .config(move |c| c.cleanup_worker.max_attempts = max)
        .build()
        .await
}

async fn create_chat(app: &TestApp) -> Uuid {
    let r = app.call(U, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn upload(app: &TestApp, chat: Uuid, name: &str, ct: &str, data: &[u8]) -> Uuid {
    let r = app.upload(U, chat, name, ct, data).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "ready", "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn row(app: &TestApp, id: Uuid) -> attachment::Model {
    let conn = app.db.conn().unwrap();
    attachment::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("attachment row")
}

async fn vector_store_rows(app: &TestApp, chat: Uuid) -> Vec<chat_vector_store::Model> {
    let conn = app.db.conn().unwrap();
    chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

async fn seed_vector_store(app: &TestApp, chat: Uuid, vs: Option<&str>) {
    let row = chat_vector_store::Model {
        id: Uuid::new_v4(),
        tenant_id: U.tenant_id,
        chat_id: chat,
        vector_store_id: vs.map(str::to_owned),
        provider: "openai".to_owned(),
        file_count: 0,
        created_at: Utc::now(),
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<chat_vector_store::Entity>(
        row.into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
}

/// Soft-delete the chat directly (no cleanup message is enqueued).
async fn soft_delete_chat(app: &TestApp, chat: Uuid) {
    let conn = app.db.conn().unwrap();
    chat::Entity::update_many()
        .col_expr(chat::Column::DeletedAt, Expr::value(Utc::now()))
        .filter(chat::Column::Id.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// Hand the attachment to cleanup without enqueueing a message; `provider_file_id`
/// replaces the stored one.
async fn mark_pending(app: &TestApp, id: Uuid, provider_file_id: Option<&str>) {
    let conn = app.db.conn().unwrap();
    attachment::Entity::update_many()
        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
        .col_expr(
            attachment::Column::ProviderFileId,
            Expr::value(provider_file_id.map(str::to_owned)),
        )
        .filter(attachment::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// A seeded document of `chat` with the given provider file id, cleanup pending.
async fn seed_pending(app: &TestApp, chat: Uuid, provider_file_id: Option<&str>) -> Uuid {
    let id =
        seed::insert_attachment(&app.db, NewAttachment::document(chat, U.user_id, "d.pdf")).await;
    mark_pending(app, id, provider_file_id).await;
    id
}

fn attachment_payload(a: &attachment::Model) -> AttachmentCleanupPayload {
    AttachmentCleanupPayload {
        event_type: AttachmentCleanupEvent::AttachmentDeleted,
        tenant_id: a.tenant_id,
        chat_id: a.chat_id,
        attachment_id: a.id,
        provider_file_id: a.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: a.storage_backend.clone(),
        attachment_kind: a.attachment_kind.clone(),
        deleted_at: Utc::now(),
        secondary_ref: None,
    }
}

fn message<T: serde::Serialize>(payload: &T, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: serde_json::to_vec(payload).unwrap(),
        payload_type: "test".to_owned(),
        created_at: Utc::now(),
        attempts,
    }
}

fn raw_message(payload: &[u8]) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: payload.to_vec(),
        payload_type: "test".to_owned(),
        created_at: Utc::now(),
        attempts: 0,
    }
}

fn attachment_handler(app: &TestApp) -> AttachmentCleanupHandler {
    AttachmentCleanupHandler::new(Arc::clone(&app.services.cleanup))
}

fn chat_handler(app: &TestApp) -> ChatCleanupHandler {
    ChatCleanupHandler::new(Arc::clone(&app.services.cleanup))
}

fn is_ok(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Ok)
}

fn is_retry(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Retry)
}

fn is_reject(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Reject(_))
}

/// Paths of the `DELETE` requests the provider received, in order.
fn delete_paths(app: &TestApp) -> Vec<String> {
    app.provider
        .requests()
        .into_iter()
        .filter(|r| r.method == Method::DELETE)
        .map(|r| r.path)
        .collect()
}

fn deletes_of(app: &TestApp, kind: &str) -> usize {
    let needle = format!("/v1/{kind}/");
    delete_paths(app)
        .iter()
        .filter(|p| p.contains(&needle))
        .count()
}

/// Poll `cond` for up to 10 s.
async fn wait_until<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !cond().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// chat cleanup
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn chat_delete_cleans_files_then_vector_store() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let d1 = upload(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    let d2 = upload(&app, chat, "b.pdf", PDF, PDF_BYTES).await;
    let img = upload(&app, chat, "p.png", "image/png", &png(40, 30)).await;
    let files = app.provider.files();
    let stores = app.provider.vector_stores();
    assert_eq!((files.len(), stores.len()), (3, 1));
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);

    let r = app
        .call(U, Method::DELETE, &format!("{CHATS}/{chat}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);

    wait_until("vector store row removed", || async {
        vector_store_rows(&app, chat).await.is_empty()
    })
    .await;

    for f in &files {
        assert_eq!(
            delete_paths(&app)
                .iter()
                .filter(|p| p.ends_with(&format!("/v1/files/{}", f.id)))
                .count(),
            1,
            "{:?}",
            delete_paths(&app)
        );
    }
    let paths = delete_paths(&app);
    assert_eq!(paths.len(), 4, "{paths:?}");
    // The vector store goes last, after every file.
    assert!(
        paths[3].ends_with(&format!("/v1/vector_stores/{}", stores[0].id)),
        "{paths:?}"
    );
    assert!(app.provider.files().iter().all(|f| f.deleted));
    assert!(app.provider.vector_stores().iter().all(|v| v.deleted));
    for id in [d1, d2, img] {
        let a = row(&app, id).await;
        assert_eq!(a.cleanup_status.as_deref(), Some("done"), "{a:?}");
        assert!(a.cleanup_updated_at.is_some());
    }
}

#[tokio::test]
async fn file_delete_404_counts_as_success() {
    let app = app().await;
    let chat = create_chat(&app).await;
    // The provider does not know this file: DELETE answers 404.
    let id = seed_pending(&app, chat, Some("file-gone")).await;
    seed_vector_store(&app, chat, Some("vs-gone")).await;
    soft_delete_chat(&app, chat).await;

    let payload = ChatCleanupPayload::soft_delete(U.tenant_id, chat, Utc::now());
    let r = chat_handler(&app).handle(&message(&payload, 0)).await;
    assert!(is_ok(&r), "{r:?}");

    let a = row(&app, id).await;
    assert_eq!(a.cleanup_status.as_deref(), Some("done"));
    assert_eq!(a.cleanup_attempts, 0);
    assert_eq!(deletes_of(&app, "files"), 1);
    // The vector store was unknown too (404): the row is removed anyway.
    assert_eq!(deletes_of(&app, "vector_stores"), 1);
    assert!(vector_store_rows(&app, chat).await.is_empty());
}

#[tokio::test]
async fn failed_file_delete_retries_then_marks_failed_and_still_deletes_vector_store() {
    let app = app_with_max_attempts(2).await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, Some("file-broken")).await;
    seed_vector_store(&app, chat, Some("vs-1")).await;
    soft_delete_chat(&app, chat).await;
    app.provider.fail_next("/v1/files", 500);
    app.provider.fail_next("/v1/files", 400);
    let payload = ChatCleanupPayload::soft_delete(U.tenant_id, chat, Utc::now());
    let h = chat_handler(&app);

    // First delivery: the attempt is recorded, the attachment stays pending, the
    // vector store is not touched.
    let r = h.handle(&message(&payload, 0)).await;
    assert!(is_retry(&r), "{r:?}");
    let a = row(&app, id).await;
    assert_eq!(a.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(a.cleanup_attempts, 1);
    assert!(
        a.last_cleanup_error
            .as_deref()
            .is_some_and(|e| !e.is_empty())
    );
    assert_eq!(deletes_of(&app, "vector_stores"), 0);
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);

    // Second delivery reaches the per-attachment limit: failed, and the vector
    // store is deleted anyway.
    let r = h.handle(&message(&payload, 1)).await;
    assert!(is_ok(&r), "{r:?}");
    let a = row(&app, id).await;
    assert_eq!(a.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(a.cleanup_attempts, 2);
    assert!(a.last_cleanup_error.is_some());
    assert!(a.cleanup_updated_at.is_some());
    assert_eq!(deletes_of(&app, "vector_stores"), 1);
    assert!(vector_store_rows(&app, chat).await.is_empty());
}

#[tokio::test]
async fn vector_store_delete_failure_keeps_row_and_rejects_at_max_attempts() {
    let app = app_with_max_attempts(2).await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, None).await;
    seed_vector_store(&app, chat, Some("vs-1")).await;
    soft_delete_chat(&app, chat).await;
    app.provider.fail_next("/v1/vector_stores", 500);
    app.provider.fail_next("/v1/vector_stores", 503);
    let payload = ChatCleanupPayload::soft_delete(U.tenant_id, chat, Utc::now());
    let h = chat_handler(&app);

    let r = h.handle(&message(&payload, 0)).await;
    assert!(is_retry(&r), "{r:?}");
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);
    // The attachment without a provider file was finished on the way.
    assert_eq!(row(&app, id).await.cleanup_status.as_deref(), Some("done"));

    // The delivery that reaches `max_attempts` is rejected; the row is kept.
    let r = h.handle(&message(&payload, 1)).await;
    match &r {
        MessageResult::Reject(reason) => assert!(
            reason.contains("vector store delete: max attempts (2) reached"),
            "{reason}"
        ),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);
    assert_eq!(deletes_of(&app, "vector_stores"), 2);

    // A dead-letter replay retries the delete.
    let r = h.handle(&message(&payload, 0)).await;
    assert!(is_ok(&r), "{r:?}");
    assert!(vector_store_rows(&app, chat).await.is_empty());
}

#[tokio::test]
async fn chat_cleanup_for_active_chat_rejected() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, Some("file-live")).await;
    seed_vector_store(&app, chat, Some("vs-1")).await;
    let payload = ChatCleanupPayload::soft_delete(U.tenant_id, chat, Utc::now());

    let r = chat_handler(&app).handle(&message(&payload, 0)).await;
    match &r {
        MessageResult::Reject(reason) => {
            assert!(reason.contains("chat is not soft-deleted"), "{reason}");
        }
        other => panic!("expected Reject, got {other:?}"),
    }
    assert!(delete_paths(&app).is_empty());
    assert_eq!(
        row(&app, id).await.cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);
}

#[tokio::test]
async fn chat_cleanup_malformed_payload_rejected() {
    let app = app().await;
    let r = chat_handler(&app).handle(&raw_message(b"{not json")).await;
    assert!(is_reject(&r), "{r:?}");
    let r = chat_handler(&app).handle(&raw_message(b"{}")).await;
    assert!(is_reject(&r), "{r:?}");
}

#[tokio::test]
async fn chat_cleanup_only_touches_pending_attachments_of_the_chat() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let other = create_chat(&app).await;
    // Done, failed and not-handed-over rows of the chat, and a pending row of
    // another chat, are left alone.
    let done = seed_pending(&app, chat, Some("file-done")).await;
    let failed = seed_pending(&app, chat, Some("file-failed")).await;
    let active = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "active.pdf"),
    )
    .await;
    let foreign = seed_pending(&app, other, Some("file-foreign")).await;
    for (id, status) in [(done, "done"), (failed, "failed")] {
        let conn = app.db.conn().unwrap();
        attachment::Entity::update_many()
            .col_expr(attachment::Column::CleanupStatus, Expr::value(status))
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }
    soft_delete_chat(&app, chat).await;

    let payload = ChatCleanupPayload::soft_delete(U.tenant_id, chat, Utc::now());
    let r = chat_handler(&app).handle(&message(&payload, 0)).await;
    assert!(is_ok(&r), "{r:?}");

    assert!(delete_paths(&app).is_empty(), "{:?}", delete_paths(&app));
    assert_eq!(
        row(&app, done).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(
        row(&app, failed).await.cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(row(&app, active).await.cleanup_status, None);
    assert_eq!(
        row(&app, foreign).await.cleanup_status.as_deref(),
        Some("pending")
    );
}

// ---------------------------------------------------------------------------------------------
// attachment cleanup
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn attachment_cleanup_deletes_provider_file_and_marks_done() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = upload(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    mark_pending(&app, id, row(&app, id).await.provider_file_id.as_deref()).await;
    let a = row(&app, id).await;
    let file_id = a.provider_file_id.clone().unwrap();

    let r = attachment_handler(&app)
        .handle(&message(&attachment_payload(&a), 0))
        .await;
    assert!(is_ok(&r), "{r:?}");

    let paths = delete_paths(&app);
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(
        paths[0].ends_with(&format!("/v1/files/{file_id}")),
        "{paths:?}"
    );
    // The handler makes no Vector Stores API call.
    assert_eq!(deletes_of(&app, "vector_stores"), 0);
    assert!(app.provider.files().iter().all(|f| f.deleted));
    let a = row(&app, id).await;
    assert_eq!(a.cleanup_status.as_deref(), Some("done"));
    assert!(a.cleanup_updated_at.is_some());
    assert_eq!(a.cleanup_attempts, 0);
}

#[tokio::test]
async fn attachment_cleanup_skips_when_chat_deleted() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, Some("file-1")).await;
    soft_delete_chat(&app, chat).await;
    let a = row(&app, id).await;

    let r = attachment_handler(&app)
        .handle(&message(&attachment_payload(&a), 0))
        .await;
    assert!(is_ok(&r), "{r:?}");
    assert!(delete_paths(&app).is_empty());
    // The chat cleanup owns the row.
    assert_eq!(
        row(&app, id).await.cleanup_status.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn attachment_cleanup_without_provider_file_marks_done() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, None).await;
    let a = row(&app, id).await;
    assert!(a.provider_file_id.is_none());

    let r = attachment_handler(&app)
        .handle(&message(&attachment_payload(&a), 0))
        .await;
    assert!(is_ok(&r), "{r:?}");
    assert!(delete_paths(&app).is_empty());
    assert_eq!(row(&app, id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn malformed_payload_rejected() {
    let app = app().await;
    let h = attachment_handler(&app);
    for bytes in [&b"not json"[..], b"{}", br#"{"event_type":"bogus"}"#] {
        let r = h.handle(&raw_message(bytes)).await;
        assert!(is_reject(&r), "{r:?}");
    }
    assert!(delete_paths(&app).is_empty());
}

#[tokio::test]
async fn attachment_cleanup_failure_retries_then_marks_failed_and_rejects() {
    let app = app_with_max_attempts(2).await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, Some("file-broken")).await;
    let a = row(&app, id).await;
    app.provider.fail_next("/v1/files", 500);
    app.provider.fail_next("/v1/files", 403);
    let h = attachment_handler(&app);

    let r = h.handle(&message(&attachment_payload(&a), 0)).await;
    assert!(is_retry(&r), "{r:?}");
    let a1 = row(&app, id).await;
    assert_eq!(a1.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(a1.cleanup_attempts, 1);
    assert!(
        a1.last_cleanup_error
            .as_deref()
            .is_some_and(|e| !e.is_empty())
    );
    assert!(a1.cleanup_updated_at.is_some());

    let r = h.handle(&message(&attachment_payload(&a), 1)).await;
    assert!(is_reject(&r), "{r:?}");
    let a2 = row(&app, id).await;
    assert_eq!(a2.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(a2.cleanup_attempts, 2);
    assert!(a2.last_cleanup_error.is_some());
}

#[tokio::test]
async fn attachment_cleanup_with_secondary_ref_still_completes() {
    // No anthropic_messages provider is configured (and the kind is not
    // `anthropic`): the secondary delete is skipped with a log line and does
    // not block the primary outcome.
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, Some("file-1")).await;
    let a = row(&app, id).await;
    let mut payload = attachment_payload(&a);
    payload.secondary_ref = Some(SecondaryRef {
        file_id: "file-secondary".to_owned(),
        provider_kind: "anthropic_messages".to_owned(),
        upstream_alias: "api.anthropic.com".to_owned(),
    });

    let r = attachment_handler(&app).handle(&message(&payload, 0)).await;
    assert!(is_ok(&r), "{r:?}");
    assert_eq!(deletes_of(&app, "files"), 1);
    assert_eq!(row(&app, id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn attachment_cleanup_of_done_row_is_a_no_op() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = seed_pending(&app, chat, Some("file-1")).await;
    let a = row(&app, id).await;
    let h = attachment_handler(&app);
    assert!(is_ok(&h.handle(&message(&attachment_payload(&a), 0)).await));
    assert_eq!(deletes_of(&app, "files"), 1);

    // Redelivery after the ack was lost.
    assert!(is_ok(&h.handle(&message(&attachment_payload(&a), 1)).await));
    assert_eq!(deletes_of(&app, "files"), 1);
    assert_eq!(row(&app, id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn deleted_attachment_is_cleaned_through_the_outbox() {
    let app = app().await;
    let chat = create_chat(&app).await;
    let id = upload(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    let r = app
        .call(
            U,
            Method::DELETE,
            &format!("{CHATS}/{chat}/attachments/{id}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);

    wait_until("attachment cleanup done", || async {
        row(&app, id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert!(app.provider.files().iter().all(|f| f.deleted));
    // The chat's vector store stays until the chat is deleted.
    assert_eq!(deletes_of(&app, "vector_stores"), 0);
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);
}
