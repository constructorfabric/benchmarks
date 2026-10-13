#![allow(clippy::unwrap_used, clippy::expect_used)]

//! An attachment never outlives the soft delete of its chat unseen (DESIGN "Cleanup on Chat
//! Deletion"): the insert of the row and of the vector store placeholder re-check the chat under
//! its row lock, a chat cleanup that already ran leaves nothing behind for a late upload, rows of
//! a deleted chat without a cleanup owner are taken by the chat cleanup and handled by the
//! attachment cleanup, and uploads of one chat are serialized for the per-chat limits.

mod attachments;
mod common;

use attachments::{
    ATTACHMENT_CLEANUP_QUEUE, CHAT_CLEANUP_QUEUE, PDF, Part, RowSeed, calls, calls_to, chat_rows,
    file_path, multipart_body, multipart_content_type, pdf_bytes, row, row_count, script_document,
    script_file, soft_delete_chat_sql, store_rows, upload_parts, upload_request_raw,
    vector_stores_path, wait_calls, wait_outcomes, wait_queue_handled,
};
use axum::body::Body;
use bytes::Bytes;
use common::{CHAT_RESOURCE_TYPE, assert_not_found, delete_chat, exec_sql, new_chat};
use http::StatusCode;
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

fn chat_cleanup_message(user: &TestUser, chat: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "tenant_id": user.tenant_id,
        "chat_id": chat,
        "system_request_id": Uuid::new_v4(),
        "reason": "chat_soft_delete",
        "chat_deleted_at": "2026-01-02T00:00:00Z",
    }))
    .unwrap()
}

fn attachment_cleanup_message(
    user: &TestUser,
    chat: &str,
    attachment: Uuid,
    provider_file: &str,
) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "event_type": "attachment_deleted",
        "tenant_id": user.tenant_id,
        "chat_id": chat,
        "attachment_id": attachment,
        "provider_file_id": provider_file,
        "vector_store_id": null,
        "storage_backend": "openai",
        "attachment_kind": "document",
        "deleted_at": "2026-01-02T00:00:00Z",
        "secondary_ref": null,
    }))
    .unwrap()
}

/// A multipart upload request whose body is fed from `rx`.
fn streaming_upload(chat: &str, rx: mpsc::UnboundedReceiver<Bytes>) -> http::Request<Body> {
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
    });
    let template = upload_request_raw(chat, Some(&multipart_content_type()), Vec::new());
    let (parts, _) = template.into_parts();
    http::Request::from_parts(parts, Body::from_stream(stream))
}

#[tokio::test]
async fn an_upload_whose_chat_is_deleted_and_cleaned_up_while_the_body_is_read_is_404() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let body = multipart_body(&[Part::file("a.pdf", PDF, &pdf_bytes(2048))]);
    let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
    let request = streaming_upload(&chat, rx);

    let driver = async {
        // The chat was checked before the body is read; the body is still outstanding.
        tx.send(Bytes::copy_from_slice(&body[..64])).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let deleted = delete_chat(&env, &user, &chat).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        wait_queue_handled(&env, CHAT_CLEANUP_QUEUE).await;
        tx.send(Bytes::copy_from_slice(&body[64..])).unwrap();
        drop(tx);
    };
    let (resp, ()) = tokio::join!(env.request_raw(&user, request), driver);

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert_eq!(
        row_count(&env).await,
        0,
        "no attachment row for a deleted chat"
    );
    assert!(store_rows(&env, &chat).await.is_empty());
    assert!(
        calls(&env).is_empty(),
        "no provider file or store exists for the deleted chat"
    );
}

#[tokio::test]
async fn a_chat_deleted_before_the_vector_store_placeholder_creates_no_store_and_no_orphan_file() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    // The chat is soft-deleted right after the attachment row was inserted: the upload passed
    // its own liveness check and only the placeholder insert can still see the deletion.
    exec_sql(
        &env,
        "CREATE TRIGGER delete_chat_after_insert AFTER INSERT ON attachments \
         BEGIN UPDATE chats SET deleted_at = '2026-01-02T00:00:00Z' WHERE id = NEW.chat_id; END",
    )
    .await;
    script_file(&env, "file-doc1");

    let resp = upload_parts(
        &env,
        &user,
        &chat,
        &[Part::file("a.pdf", PDF, &pdf_bytes(2048))],
    )
    .await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert!(
        store_rows(&env, &chat).await.is_empty(),
        "no placeholder for a deleted chat"
    );
    assert!(
        calls_to(&env, "POST", &vector_stores_path()).is_empty(),
        "no provider vector store is created for a deleted chat"
    );
    wait_calls(&env, "DELETE", &file_path("file-doc1"), 1).await;
    let rows = chat_rows(&env, &chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
}

#[tokio::test]
async fn the_chat_cleanup_takes_the_rows_of_a_deleted_chat_that_have_no_cleanup_owner() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let late = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-late")
        .insert(&env)
        .await;
    soft_delete_chat_sql(&env, &chat).await;
    assert!(row(&env, late).await.cleanup_status.is_none());

    env.enqueue_raw_outbox(
        CHAT_CLEANUP_QUEUE,
        user.tenant_id,
        &chat_cleanup_message(&user, &chat),
    )
    .await;

    assert_eq!(wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 1).await, vec!["ok"]);
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-late")).len(), 1);
    assert_eq!(
        row(&env, late).await.cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn the_attachment_cleanup_of_a_deleted_chat_deletes_the_file_of_a_row_without_an_owner() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let late = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-late")
        .deleted()
        .insert(&env)
        .await;
    soft_delete_chat_sql(&env, &chat).await;

    env.enqueue_raw_outbox(
        ATTACHMENT_CLEANUP_QUEUE,
        user.tenant_id,
        &attachment_cleanup_message(&user, &chat, late, "file-late"),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await,
        vec!["ok"]
    );
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-late")).len(), 1);
    assert_eq!(
        row(&env, late).await.cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn concurrent_uploads_at_the_limit_admit_exactly_one() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "rag": { "max_documents_per_chat": 1 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_document(&env, "file-a", "vs_1", true);
    script_file(&env, "file-b");
    let data = pdf_bytes(2048);
    let part = [Part::file("a.pdf", PDF, &data)];

    let (first, second) = tokio::join!(
        upload_parts(&env, &user, &chat, &part),
        upload_parts(&env, &user, &chat, &part),
    );

    let statuses = [first.status, second.status];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "the other one hits the document limit: {statuses:?}"
    );
    let live = chat_rows(&env, &chat)
        .await
        .into_iter()
        .filter(|row| row.status != "failed")
        .count();
    assert_eq!(live, 1);
}
