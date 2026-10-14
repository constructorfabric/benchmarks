//! The failure handling after the provider file upload succeeded, and the cancel safety of the
//! upload pipeline: every error after `upload_file` returned a provider file id deletes that file
//! best effort, fails the row when it can, returns the permit, records the upload metric and
//! brings the pending gauge back; a handler future dropped after the provider upload cannot
//! abandon the pipeline.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; algorithm
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure` (`inst-uf-*`), upload
//! completion and metrics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;

use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, ANTHROPIC_CHAT_MODEL, AttachRig, FilePart, PDF, assert_created, pdf_bytes,
    png, rig, upload_request,
};
use crate::test_support::chats::a1;
use crate::test_support::provider::Gate;
use crate::test_support::storage_mock::{Answer, Route};

/// The provider file id the fake provider answers to the upload.
const FILE: &str = "file-1";

fn arm_upload(rig: &AttachRig) {
    rig.storage
        .set_default(Route::FilesUpload, Answer::ok(&json!({ "id": FILE })));
}

fn deleted_files(rig: &AttachRig) -> Vec<String> {
    rig.storage
        .calls_of(Route::FilesDelete)
        .into_iter()
        .filter_map(|call| call.resource_id)
        .collect()
}

/// The state every failed request must leave behind: the provider file deleted, the permit back,
/// one `error` sample and the pending gauge at its prior value.
fn assert_cleaned_up(rig: &AttachRig) {
    assert_eq!(deleted_files(rig), vec![FILE.to_owned()]);
    assert_eq!(
        rig.permits.available(),
        usize::from(rig.rag.max_concurrent_uploads)
    );
    assert_eq!(
        rig.uploads(),
        vec![("document".to_owned(), "error".to_owned())]
    );
    assert_eq!(rig.metrics.pending_total(), 0, "the pending gauge leaked");
}

#[tokio::test]
async fn a_mark_uploaded_error_deletes_the_provider_file_and_fails_the_row() {
    // inst-uf-has-file, inst-uf-delete-file, inst-uf-mark-failed, inst-uf-metrics
    let rig = rig().await;
    let chat = rig.std_chat().await;
    arm_upload(&rig);
    rig.faults.mark_uploaded.store(true, Ordering::SeqCst);
    let reply = rig.upload_pdf(chat.id).await;
    assert_eq!(reply.status.as_u16(), 500, "{}", reply.text);
    assert_cleaned_up(&rig);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(row.provider_file_id, None, "the id was never committed");
}

#[tokio::test]
async fn a_storage_recheck_error_deletes_the_provider_file_and_fails_the_row() {
    // inst-uf-delete-file: the row is `uploaded` and its gauge share was already released
    let rig = rig().await;
    let chat = rig.std_chat().await;
    arm_upload(&rig);
    rig.faults.storage_recheck.store(true, Ordering::SeqCst);
    let reply = rig.upload_pdf(chat.id).await;
    assert_eq!(reply.status.as_u16(), 500, "{}", reply.text);
    assert_cleaned_up(&rig);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
}

#[tokio::test]
async fn a_document_mark_ready_error_deletes_the_provider_file_and_fails_the_row() {
    // the file search path completed: the provider drops the file from its vector stores with
    // the file delete
    let rig = rig().await;
    let chat = rig.std_chat().await;
    arm_upload(&rig);
    rig.faults.mark_ready.store(true, Ordering::SeqCst);
    let reply = rig.upload_pdf(chat.id).await;
    assert_eq!(reply.status.as_u16(), 500, "{}", reply.text);
    assert_eq!(rig.storage.count_of(Route::VsAddFile), 1);
    assert_cleaned_up(&rig);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
}

#[tokio::test]
async fn an_image_mark_ready_error_deletes_the_provider_file_and_the_secondary_copy() {
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    arm_upload(&rig);
    rig.faults.mark_ready.store(true, Ordering::SeqCst);
    let reply = rig.upload(chat.id, "a.png", "image/png", &png(8, 8)).await;
    assert_eq!(reply.status.as_u16(), 500, "{}", reply.text);
    assert_eq!(deleted_files(&rig).first().map(String::as_str), Some(FILE));
    let secondary_deletes = rig
        .storage
        .calls_of(Route::FilesDelete)
        .into_iter()
        .filter(|call| call.alias == ANTHROPIC_ALIAS)
        .count();
    assert_eq!(secondary_deletes, 1, "the secondary copy was not deleted");
    assert_eq!(rig.permits.available(), 10);
    assert_eq!(
        rig.uploads(),
        vec![("image".to_owned(), "error".to_owned())]
    );
    assert_eq!(rig.metrics.pending_total(), 0);
    assert_eq!(rig.only_row(chat.id).await.status, "failed");
}

/// Wait (5 s of real time at most) until every upload permit is back.
async fn wait_for_permits(rig: &AttachRig) {
    let wait = async {
        while rig.permits.available() != usize::from(rig.rag.max_concurrent_uploads) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the pipeline never released its permit");
}

#[tokio::test]
async fn a_handler_dropped_after_the_provider_upload_still_completes_the_pipeline() {
    // the request future is dropped (gateway timeout, client disconnect) once the provider holds
    // the file and before the row write: the spawned pipeline finishes the upload
    let rig = rig().await;
    let chat = rig.std_chat().await;
    arm_upload(&rig);
    let gate = Gate::new();
    rig.faults.gate_mark_uploaded(&gate);
    let handle = rig.spawn(upload_request(
        chat.id,
        &a1(),
        &[FilePart::file("a.pdf", PDF, &pdf_bytes())],
    ));
    rig.faults.wait_for_mark_uploaded().await;
    assert_eq!(rig.storage.completed_uploads(), 1);
    handle.abort();
    gate.open();
    wait_for_permits(&rig).await;
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "ready", "the pipeline was abandoned");
    assert_eq!(row.provider_file_id.as_deref(), Some(FILE));
    assert!(deleted_files(&rig).is_empty());
    assert_eq!(
        rig.uploads(),
        vec![("document".to_owned(), "ok".to_owned())]
    );
    assert_eq!(rig.metrics.pending_total(), 0);
}

#[tokio::test]
async fn a_handler_dropped_after_the_provider_upload_still_deletes_the_file_on_a_failure() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    arm_upload(&rig);
    let gate = Gate::new();
    rig.faults.gate_mark_uploaded(&gate);
    rig.faults.mark_uploaded.store(true, Ordering::SeqCst);
    let handle = rig.spawn(upload_request(
        chat.id,
        &a1(),
        &[FilePart::file("a.pdf", PDF, &pdf_bytes())],
    ));
    rig.faults.wait_for_mark_uploaded().await;
    handle.abort();
    gate.open();
    wait_for_permits(&rig).await;
    assert_cleaned_up(&rig);
    assert_eq!(rig.only_row(chat.id).await.status, "failed");
}

#[tokio::test]
async fn a_gear_stop_fails_an_upload_whose_body_is_still_streaming() {
    // the body stream is bound to the shutdown token: the pipeline fails the row through the
    // normal failure path instead of outliving the gear
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.shutdown.cancel();
    let reply = rig.upload_pdf(chat.id).await;
    assert!(!reply.status.is_success(), "{}", reply.text);
    assert_eq!(rig.permits.available(), 10);
    assert_eq!(rig.storage.count_of(Route::FilesUpload), 0);
    assert_eq!(rig.metrics.pending_total(), 0);
}

#[tokio::test]
async fn a_successful_upload_is_unchanged_by_the_spawned_pipeline() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_eq!(rig.permits.available(), 10);
    assert_eq!(rig.metrics.pending_total(), 0);
}
