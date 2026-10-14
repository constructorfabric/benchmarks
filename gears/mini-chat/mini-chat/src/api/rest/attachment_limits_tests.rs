//! Per-chat limits, the byte counter and the in-request failures over the router: the effective
//! per-file limit (`FILE_TOO_LARGE`, the row left `failed`), the document and storage limits
//! (429, including the re-check with the final size), provider upload failures, vector store
//! failures, the provider mismatch, indexing failures within the deadline, the best-effort
//! provider file delete, the failed row staying visible, the pending gauge and the generic 503
//! without provider text.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flows
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document` and `-upload-image`;
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits`,
//! `-provider-file-upload`, `-upload-failure`, `-upload-completion` and `-error-contract`;
//! states `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status`; definitions of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-vector-store`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;

use crate::config::RagConfig;
use crate::test_support::attachments::{
    AttachRig, AttachRigOptions, OPENAI_BACKEND, PDF, RowSeed, SMALL_MODEL, assert_created,
    assert_file_too_large, assert_no_provider_text, assert_storage_unavailable,
    fail_attachment_inserts, png, rig, rig_with, seed_documents, seed_row, seed_vector_store,
};
use crate::test_support::chats::{assert_problem, at};
use crate::test_support::provider::GatewayFailure;
use crate::test_support::storage_mock::{Answer, Route};

const MIB: usize = 1_048_576;
const MIB_I64: i64 = 1_048_576;

fn uploads_of(rig: &AttachRig) -> Vec<(String, String)> {
    rig.uploads()
}

fn error_sample(kind: &str) -> Vec<(String, String)> {
    vec![(kind.to_owned(), "error".to_owned())]
}

fn small_rag(file_kb: u32, image_kb: u32) -> AttachRigOptions {
    AttachRigOptions::new().with_rag(RagConfig {
        uploaded_file_max_size_kb: file_kb,
        uploaded_image_max_size_kb: image_kb,
        ..RagConfig::default()
    })
}

// ── the effective per-file limit ─────────────────────────────────────────────

#[tokio::test]
async fn a_file_above_the_model_limit_is_400_file_too_large_and_the_row_is_failed() {
    // inst-pfu-too-large, inst-uf-case-too-large: min(gear limit, model max_file_size_mb)
    let rig = rig().await;
    let chat = rig.chat(SMALL_MODEL).await;
    let reply = rig
        .upload(chat.id, "big.pdf", PDF, &vec![b'x'; MIB + 1])
        .await;
    assert_file_too_large(&reply);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("file_too_large"));
    assert_eq!(row.provider_file_id, None);
    assert_eq!(uploads_of(&rig), error_sample("document"));
    assert_eq!(rig.permits.available(), 10);
}

#[tokio::test]
async fn a_file_of_exactly_the_limit_is_accepted() {
    let rig = rig().await;
    let chat = rig.chat(SMALL_MODEL).await;
    let reply = rig.upload(chat.id, "edge.pdf", PDF, &vec![b'x'; MIB]).await;
    assert_created(&reply, "ready");
}

#[tokio::test]
async fn the_gear_document_limit_applies_when_it_is_lower_than_the_model_limit() {
    let rig = rig_with(small_rag(8, 5120)).await;
    let chat = rig.std_chat().await;
    let ok = rig
        .upload(chat.id, "ok.pdf", PDF, &vec![b'x'; 8 * 1024])
        .await;
    assert_created(&ok, "ready");
    let over = rig
        .upload(chat.id, "over.pdf", PDF, &vec![b'x'; 8 * 1024 + 1])
        .await;
    assert_file_too_large(&over);
}

#[tokio::test]
async fn the_gear_image_limit_applies_to_images() {
    let rig = rig_with(small_rag(25_600, 4)).await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "big.png", "image/png", &vec![0u8; 4 * 1024 + 1])
        .await;
    assert_file_too_large(&reply);
    assert_eq!(uploads_of(&rig), error_sample("image"));
}

#[tokio::test]
async fn an_oversized_body_aborts_the_provider_upload_and_leaves_no_provider_file() {
    // inst-pfu-return-too-large: no provider file id, so no delete
    let rig = rig().await;
    let chat = rig.chat(SMALL_MODEL).await;
    rig.upload(chat.id, "big.pdf", PDF, &vec![b'x'; 2 * MIB])
        .await;
    assert_eq!(
        rig.storage.completed_uploads(),
        0,
        "the whole oversized body reached the provider"
    );
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 0);
    assert_eq!(rig.storage.count_of(Route::VsCreate), 0);
}

// ── document and storage limits ──────────────────────────────────────────────

#[tokio::test]
async fn the_fifty_first_document_is_429_document_limit_with_no_row_and_no_provider_call() {
    // inst-doc-return-limit
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_documents(&rig.db, &chat, 50, 10).await;
    let reply = rig.upload_pdf(chat.id).await;
    let problem = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(
        problem["context"]["violations"][0]["subject"],
        "document_limit"
    );
    assert_eq!(rig.rows(chat.id).await.len(), 50);
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(uploads_of(&rig), error_sample("document"));
    assert_eq!(rig.permits.available(), 10);
}

#[tokio::test]
async fn the_fiftieth_document_is_accepted() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_documents(&rig.db, &chat, 49, 10).await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
}

#[tokio::test]
async fn failed_and_deleted_documents_do_not_count_toward_the_document_limit() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for _ in 0..30 {
        seed_row(&rig.db, &chat, RowSeed::document().status("failed")).await;
        seed_row(&rig.db, &chat, RowSeed::document().deleted(at(1))).await;
    }
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
}

#[tokio::test]
async fn images_never_count_toward_the_document_limit() {
    // inst-img-insert
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for _ in 0..50 {
        seed_row(&rig.db, &chat, RowSeed::image().size(10)).await;
    }
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
}

#[tokio::test]
async fn an_upload_over_100_mib_of_live_attachments_is_429_storage_limit() {
    // inst-lim-return-storage-limit; images count toward the sum
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_row(&rig.db, &chat, RowSeed::image().size(100 * MIB_I64)).await;
    let reply = rig.upload_pdf(chat.id).await;
    let problem = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(
        problem["context"]["violations"][0]["subject"],
        "storage_limit"
    );
    assert_eq!(rig.rows(chat.id).await.len(), 1);
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(uploads_of(&rig), error_sample("document"));
}

#[tokio::test]
async fn an_image_upload_is_429_storage_limit_at_the_storage_limit() {
    // inst-img-return-limit
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_row(&rig.db, &chat, RowSeed::document().size(100 * MIB_I64)).await;
    let reply = rig.upload(chat.id, "a.png", "image/png", &png(8, 8)).await;
    let problem = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(
        problem["context"]["violations"][0]["subject"],
        "storage_limit"
    );
    assert_eq!(uploads_of(&rig), error_sample("image"));
}

#[tokio::test]
async fn failed_and_deleted_rows_do_not_count_toward_the_storage_limit() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("failed").size(300 * MIB_I64),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().deleted(at(1)).size(300 * MIB_I64),
    )
    .await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
}

#[tokio::test]
async fn the_storage_recheck_with_the_final_size_fails_the_upload() {
    // inst-pfu-recheck, inst-uf-case-storage: 99 MiB + 2 MiB exceeds 100 MiB
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_row(&rig.db, &chat, RowSeed::document().size(99 * MIB_I64)).await;
    let reply = rig
        .upload(chat.id, "two.pdf", PDF, &vec![b'x'; 2 * MIB])
        .await;
    let problem = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(
        problem["context"]["violations"][0]["subject"],
        "storage_limit"
    );
    let rows = rig.rows(chat.id).await;
    let failed: Vec<_> = rows.iter().filter(|row| row.status == "failed").collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].error_code.as_deref(),
        Some("storage_limit_exceeded")
    );
    // the provider file is deleted best effort and nothing was indexed
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
    assert_eq!(rig.storage.count_of(Route::VsAddFile), 0);
    assert_eq!(uploads_of(&rig), error_sample("document"));
}

// ── provider upload failures ─────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_upload_failure_is_503_and_the_row_is_failed_upload_failed() {
    // inst-pfu-return-failed, inst-uf-case-unavailable
    for answer in [
        Answer::status(500),
        Answer::status(400),
        Answer::Fail(GatewayFailure::Unavailable),
        Answer::Fail(GatewayFailure::Timeout),
    ] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        rig.storage.set_default(Route::FilesUpload, answer);
        let reply = rig.upload_pdf(chat.id).await;
        assert_storage_unavailable(&reply);
        let row = rig.only_row(chat.id).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
        assert_eq!(row.provider_file_id, None);
        assert_eq!(
            rig.storage.count_of(Route::FilesUpload),
            1,
            "the upload was retried"
        );
        assert_eq!(
            rig.storage.count_of(Route::FilesDelete),
            0,
            "no file id, so nothing to delete"
        );
        assert_eq!(rig.storage.count_of(Route::VsCreate), 0);
        assert_eq!(uploads_of(&rig), error_sample("document"));
        assert_eq!(rig.permits.available(), 10);
    }
}

#[tokio::test]
async fn the_pending_gauge_returns_to_its_prior_value_on_every_path() {
    // inst-met-pending: +1 at the insert, -1 where the row leaves pending
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_eq!(rig.metrics.pending_total(), 0, "after a 201");
    rig.storage
        .set_default(Route::FilesUpload, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    assert_eq!(rig.metrics.pending_total(), 0, "after an upload failure");
    rig.storage
        .set_default(Route::FilesUpload, Answer::ok(&json!({ "id": "file-ok" })));
    let small = rig.chat(SMALL_MODEL).await;
    rig.upload(small.id, "big.pdf", PDF, &vec![b'x'; MIB + 1])
        .await;
    assert_eq!(rig.metrics.pending_total(), 0, "after file_too_large");
}

#[tokio::test]
async fn the_pending_gauge_is_incremented_at_the_insert_and_decremented_when_the_row_leaves_pending()
 {
    use crate::test_support::attach_fakes::MetricEvent;
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let deltas: Vec<i64> = rig
        .metrics
        .events()
        .into_iter()
        .filter_map(|event| match event {
            MetricEvent::Pending(delta) => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec![1, -1]);
}

#[tokio::test]
async fn a_failure_after_the_row_left_pending_decrements_the_gauge_only_once() {
    // inst-uf-mark-failed: a row that already left pending at inst-pfu-set-uploaded is not
    // decremented again
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsCreate, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    assert_eq!(rig.metrics.pending_total(), 0);
}

#[tokio::test]
async fn a_database_failure_at_the_insert_is_500_internal_and_releases_the_permit() {
    // inst-doc-permit-rule: a database failure is mapped to 500 internal
    let rig = rig().await;
    let chat = rig.std_chat().await;
    fail_attachment_inserts(&rig.db).await;
    let reply = rig.upload_pdf(chat.id).await;
    let problem = assert_problem(&reply, 500, "internal");
    assert!(
        !problem.to_string().contains("forced attachment"),
        "{problem}"
    );
    assert_eq!(uploads_of(&rig), error_sample("document"));
    assert_eq!(rig.permits.available(), 10);
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn the_failed_row_stays_visible_through_get_with_its_error_code() {
    // inst-uf-visible
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::FilesUpload, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    let id = rig.only_row(chat.id).await.id;
    let reply = rig.get_attachment(chat.id, id).await;
    assert_eq!(reply.status.as_u16(), 200, "{}", reply.text);
    let body = reply.json();
    assert_eq!(body["status"], "failed");
    assert_eq!(body["error_code"], "upload_failed");
}

#[tokio::test]
async fn a_failed_row_counts_toward_neither_limit_so_the_client_can_retry() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage.queue(Route::FilesUpload, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    // the retry is a new attachment with a new provider file id
    let rows = rig.rows(chat.id).await;
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].id, rows[1].id);
}

// ── vector store failures ────────────────────────────────────────────────────

#[tokio::test]
async fn a_vector_store_create_failure_is_503_and_the_row_failed_vector_store_failed() {
    // inst-vs-create-failed, inst-doc-failed
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsCreate, Answer::status(500));
    let reply = rig.upload_pdf(chat.id).await;
    assert_storage_unavailable(&reply);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    // the placeholder was deleted and the provider file removed best effort
    assert!(rig.vector_stores().await.is_empty());
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
    assert_eq!(rig.storage.count_of(Route::VsAddFile), 0);
    assert_eq!(
        rig.storage.count_of(Route::VsCreate),
        1,
        "the create was retried"
    );
    assert_eq!(uploads_of(&rig), error_sample("document"));
}

#[tokio::test]
async fn the_deleted_provider_file_is_the_one_that_was_uploaded() {
    // inst-uf-delete-file: DELETE /files/{provider_file_id}
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage.queue(
        Route::FilesUpload,
        Answer::ok(&json!({ "id": "file-the-one" })),
    );
    rig.storage
        .set_default(Route::VsCreate, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    let delete = rig.storage.calls_of(Route::FilesDelete).remove(0);
    assert_eq!(delete.method, "DELETE");
    assert!(
        delete.uri.ends_with("/v1/files/file-the-one"),
        "{}",
        delete.uri
    );
}

#[tokio::test]
async fn a_provider_mismatch_is_409_and_the_row_failed_with_vector_store_failed() {
    // inst-vs-mismatch, inst-uf-case-mismatch: stored as vector_store_failed
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_vector_store(
        &rig.db,
        &chat,
        Some("vs_other"),
        "some_other_backend",
        at(1),
    )
    .await;
    let reply = rig.upload_pdf(chat.id).await;
    let problem = assert_problem(&reply, 409, "already_exists");
    assert_eq!(problem["context"]["resource_name"], "provider_mismatch");
    assert_no_provider_text(&reply.text);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
    assert_eq!(rig.storage.count_of(Route::VsCreate), 0);
    assert_eq!(rig.storage.count_of(Route::VsAddFile), 0);
    assert_eq!(uploads_of(&rig), error_sample("document"));
}

#[tokio::test]
async fn a_bound_store_of_the_same_backend_is_reused_without_a_create() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.db, &chat, Some("vs_seeded"), OPENAI_BACKEND, at(1)).await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_eq!(rig.storage.count_of(Route::VsCreate), 0);
    let add = rig.storage.calls_of(Route::VsAddFile).remove(0);
    assert!(
        add.uri.contains("/vector_stores/vs_seeded/files"),
        "{}",
        add.uri
    );
}

#[tokio::test]
async fn a_stale_placeholder_is_reclaimed_by_the_next_upload() {
    // inst-vs-stale, inst-vs-reclaim: a placeholder older than 120 s
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let old = rig.clock.now() - time::Duration::seconds(121);
    let stale = seed_vector_store(&rig.db, &chat, None, OPENAI_BACKEND, old).await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let stores = rig.vector_stores().await;
    assert_eq!(stores.len(), 1);
    assert_ne!(stores[0].id, stale, "the stale placeholder was kept");
    assert!(stores[0].vector_store_id.is_some());
    assert_eq!(rig.storage.count_of(Route::VsCreate), 1);
}

// ── indexing failures within the deadline ────────────────────────────────────

#[tokio::test]
async fn a_failed_add_is_503_with_the_row_failed_indexing_failed() {
    // inst-iw-return-add-failed
    for answer in [
        Answer::status(500),
        Answer::status(400),
        Answer::Fail(GatewayFailure::Unavailable),
    ] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        rig.storage.set_default(Route::VsAddFile, answer);
        let reply = rig.upload_pdf(chat.id).await;
        assert_storage_unavailable(&reply);
        let row = rig.only_row(chat.id).await;
        assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
        assert_eq!(
            rig.storage.count_of(Route::VsAddFile),
            1,
            "the add was retried"
        );
        assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
        assert_eq!(uploads_of(&rig), error_sample("document"));
    }
}

#[tokio::test]
async fn failed_cancelled_unknown_and_4xx_statuses_fail_the_indexing_with_503() {
    // inst-iw-return-failed, inst-iw-read-failed: one provider file delete, no error_code in
    // the response body
    let cases = vec![
        Answer::indexing("failed"),
        Answer::indexing("cancelled"),
        Answer::indexing("expired"),
        Answer::status(404),
    ];
    for answer in cases {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        rig.storage
            .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
        rig.storage
            .queue(Route::VsReadFile, Answer::indexing("in_progress"));
        rig.storage.queue(Route::VsReadFile, answer);
        let reply = rig.upload_pdf(chat.id).await;
        assert_storage_unavailable(&reply);
        let row = rig.only_row(chat.id).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
        assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
        assert!(
            rig.vector_stores().await[0].vector_store_id.is_some(),
            "the store stays"
        );
    }
}

#[tokio::test]
async fn a_failing_best_effort_delete_does_not_change_the_response_and_is_not_retried() {
    // inst-uf-delete-file: fire and forget, not retried; the outcome is only logged
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::status(500));
    rig.storage
        .set_default(Route::FilesDelete, Answer::status(500));
    let reply = rig.upload_pdf(chat.id).await;
    assert_storage_unavailable(&reply);
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
}

#[tokio::test]
async fn no_failure_response_carries_provider_text_or_identifiers() {
    // inst-err-no-provider-text: every body of a provider failure is generic
    for route in [Route::FilesUpload, Route::VsCreate, Route::VsAddFile] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        rig.storage.set_default(route, Answer::status(500));
        let reply = rig.upload_pdf(chat.id).await;
        assert_storage_unavailable(&reply);
        assert!(!reply.text.contains("provider-secret-error-text"));
    }
}
