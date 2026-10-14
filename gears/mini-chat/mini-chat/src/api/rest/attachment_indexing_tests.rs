//! The indexing lifecycle over the router with a mock vector store: `completed` after polling,
//! the 250 ms doubling schedule on the virtual timer, transient read errors, a missing status,
//! and the still-running index at the 25 s deadline (201 `uploaded`, the permit free, the
//! non-persisted background task with its heartbeat, its `ready` write, its one-transaction
//! failure with the cleanup message, the 10-minute timeout, and the silent stops on deletion,
//! chat deletion and gear stop). The mock gate holds the background task so the row can be
//! observed and changed meanwhile.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flow
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document`; algorithms
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait` and
//! `-background-indexing`; state `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status`;
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-indexing-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::test_support::attachments::{
    AttachRig, AttachRigOptions, FilePart, IN_REQUEST_READS, PDF, assert_created,
    assert_storage_unavailable, body_id, pdf_bytes, rig, rig_with, upload_request,
};
use crate::test_support::chats::{a1, assert_problem, blob};
use crate::test_support::provider::{Gate, GatewayFailure};
use crate::test_support::raw_exec;
use crate::test_support::storage_mock::{Answer, Route};

const MS: fn(u64) -> Duration = Duration::from_millis;

/// Script an upload whose index is still `in_progress` at the 25 s deadline; the first read of
/// the background task waits on `gate` and then answers `then`.
fn script_held_background(rig: &AttachRig, gate: &Arc<Gate>, then: Answer) {
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage.queue_n(
        Route::VsReadFile,
        &Answer::indexing("in_progress"),
        IN_REQUEST_READS,
    );
    rig.storage
        .queue(Route::VsReadFile, Answer::gated(gate, then));
}

/// Wait until the background task is parked on its first read.
async fn background_started(rig: &AttachRig) {
    rig.storage
        .wait_for(Route::VsReadFile, IN_REQUEST_READS + 1)
        .await;
}

/// Give a stopped task time to (not) write.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn uploaded_row(rig: &AttachRig, gate: &Arc<Gate>, then: Answer) -> (Uuid, Uuid) {
    script_held_background(rig, gate, then);
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    let body = assert_created(&reply, "uploaded");
    assert_eq!(body["status"], "uploaded");
    background_started(rig).await;
    (chat.id, body_id(&reply))
}

// ── within the request ───────────────────────────────────────────────────────

#[tokio::test]
async fn completed_after_polling_is_201_ready_and_the_waits_double_from_250_ms() {
    // inst-iw-initial-wait, inst-iw-backoff, inst-iw-return-ready
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("in_progress"));
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("in_progress"));
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("completed"));
    let reply = rig.upload_pdf(chat.id).await;
    assert_created(&reply, "ready");
    assert_eq!(&rig.time.sleeps()[..3], &[MS(250), MS(500), MS(1000)]);
    assert_eq!(rig.storage.count_of(Route::VsReadFile), 3);
    assert_eq!(rig.row(chat.id, body_id(&reply)).await.status, "ready");
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 0);
}

#[tokio::test]
async fn the_status_reads_target_the_vector_store_file() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("completed"));
    let reply = rig.upload_pdf(chat.id).await;
    let row = rig.row(chat.id, body_id(&reply)).await;
    let store = rig.vector_stores().await.remove(0).vector_store_id.unwrap();
    let read = rig.storage.calls_of(Route::VsReadFile).remove(0);
    assert_eq!(read.method, "GET");
    assert!(
        read.uri.ends_with(&format!(
            "/v1/vector_stores/{store}/files/{}",
            row.provider_file_id.unwrap()
        )),
        "{}",
        read.uri
    );
}

#[tokio::test]
async fn a_missing_status_counts_as_in_progress_and_polling_goes_on() {
    // inst-iw-initial-status, inst-iw-status
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::no_status());
    rig.storage.queue(Route::VsReadFile, Answer::no_status());
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("completed"));
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_eq!(rig.storage.count_of(Route::VsReadFile), 2);
}

#[tokio::test]
async fn transient_read_errors_keep_polling() {
    // inst-iw-transient: provider 5xx and gateway failures
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage.queue(Route::VsReadFile, Answer::status(500));
    rig.storage
        .queue(Route::VsReadFile, Answer::Fail(GatewayFailure::Unavailable));
    rig.storage.queue(Route::VsReadFile, Answer::status(503));
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("completed"));
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_eq!(rig.storage.count_of(Route::VsReadFile), 4);
}

#[tokio::test]
async fn a_non_transient_read_error_fails_at_once_with_503() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage.queue(Route::VsReadFile, Answer::status(403));
    let reply = rig.upload_pdf(chat.id).await;
    assert_storage_unavailable(&reply);
    assert_eq!(rig.storage.count_of(Route::VsReadFile), 1);
}

// ── at the deadline ──────────────────────────────────────────────────────────

#[tokio::test]
async fn an_index_still_running_at_25_seconds_is_201_uploaded() {
    // inst-doc-still-indexing, inst-doc-return-uploaded
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    let row = rig.row(chat_id, id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.provider_file_id.is_some());
    assert_eq!(row.error_code, None);
    assert_eq!(
        rig.storage.count_of(Route::VsReadFile),
        IN_REQUEST_READS + 1
    );
    gate.open();
}

#[tokio::test]
async fn the_deadline_is_25_seconds_after_the_request_start_on_the_virtual_timer() {
    let rig = rig().await;
    let gate = Gate::new();
    uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    let elapsed = rig.time.elapsed();
    assert!(elapsed < Duration::from_secs(25), "{elapsed:?}");
    assert!(elapsed > Duration::from_secs(20), "{elapsed:?}");
    gate.open();
}

#[tokio::test]
async fn the_deadline_counts_from_the_request_start_including_the_provider_upload() {
    // inst-doc-start-instant: the 25 s are spent from the start of the request, so a provider
    // upload that took 10 s leaves 15 s for the indexing wait
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.set_default(
        Route::FilesUpload,
        Answer::gated(&gate, Answer::ok(&serde_json::json!({ "id": "file-slow" }))),
    );
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage
        .set_default(Route::VsReadFile, Answer::indexing("in_progress"));
    let request = rig.spawn(upload_request(
        chat.id,
        &a1(),
        &[FilePart::file("slow.pdf", PDF, &pdf_bytes())],
    ));
    rig.storage.wait_for(Route::FilesUpload, 1).await;
    rig.time.advance(Duration::from_secs(10));
    gate.open();
    // drive the virtual time while the request polls the index
    let drive = async {
        while !request.is_finished() {
            rig.time.advance(Duration::from_millis(250));
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), drive)
        .await
        .expect("the request never returned");
    let reply = request.await.unwrap();
    assert_created(&reply, "uploaded");
    let reads_in_request = rig.storage.count_of(Route::VsReadFile);
    assert!(
        reads_in_request < IN_REQUEST_READS,
        "{reads_in_request} reads: the upload time was not charged to the deadline"
    );
    rig.shutdown.cancel();
}

#[tokio::test]
async fn the_permit_is_free_when_the_201_uploaded_returns_although_the_task_continues() {
    // inst-doc-complete-uploaded; the background task never holds a permit
    let rig = rig().await;
    let gate = Gate::new();
    uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    assert_eq!(rig.permits.available(), 10);
    assert_eq!(
        rig.uploads(),
        vec![("document".to_owned(), "ok".to_owned())]
    );
    gate.open();
}

#[tokio::test]
async fn polling_shows_uploaded_while_the_background_task_refreshes_the_row() {
    // inst-bg-heartbeat: updated_at is refreshed before the first poll of the round
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    for _ in 0..3 {
        let reply = rig.get_attachment(chat_id, id).await;
        assert_eq!(reply.json()["status"], "uploaded");
    }
    let row = rig.row(chat_id, id).await;
    assert!(
        row.updated_at > row.created_at,
        "the heartbeat never refreshed updated_at"
    );
    gate.open();
}

#[tokio::test]
async fn the_background_task_sets_ready_on_completed() {
    // inst-bg-case-ready
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    gate.open();
    rig.wait_for_status(chat_id, id, "ready").await;
    let reply = rig.get_attachment(chat_id, id).await;
    assert_eq!(reply.json()["status"], "ready");
    assert!(reply.json().get("error_code").is_none());
    assert_eq!(rig.metrics.background(), vec!["ready".to_owned()]);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 0);
}

#[tokio::test]
async fn the_background_task_fails_the_row_with_one_cleanup_message_and_no_inline_delete() {
    // inst-bg-case-failed, inst-bg-no-inline-delete
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("failed")).await;
    gate.open();
    rig.wait_for_status(chat_id, id, "failed").await;
    let row = rig.row(chat_id, id).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = rig.attachment_cleanup_rows().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].payload["event_type"],
        "attachment_indexing_failed"
    );
    assert_eq!(messages[0].payload["attachment_id"], id.to_string());
    assert_eq!(
        messages[0].payload["provider_file_id"],
        row.provider_file_id.clone().unwrap()
    );
    assert_eq!(
        rig.storage.count_of(Route::FilesDelete),
        0,
        "the cleanup handler deletes it"
    );
    assert_eq!(rig.metrics.background(), vec!["failed".to_owned()]);
    let reply = rig.get_attachment(chat_id, id).await;
    assert_eq!(reply.json()["status"], "failed");
    assert_eq!(reply.json()["error_code"], "indexing_failed");
}

#[tokio::test]
async fn a_cancelled_status_in_the_background_fails_the_row_too() {
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("cancelled")).await;
    gate.open();
    rig.wait_for_status(chat_id, id, "failed").await;
    assert_eq!(rig.attachment_cleanup_rows().await.len(), 1);
}

#[tokio::test]
async fn ten_minutes_of_in_progress_in_the_background_is_a_timeout_failure() {
    // inst-bg-timeout: failed with indexing_failed, cleanup pending, one message
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage
        .set_default(Route::VsReadFile, Answer::indexing("in_progress"));
    let reply = rig.upload_pdf(chat.id).await;
    let id = body_id(&reply);
    assert_created(&reply, "uploaded");
    rig.wait_for_status(chat.id, id, "failed").await;
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(rig.attachment_cleanup_rows().await.len(), 1);
    assert_eq!(rig.metrics.background(), vec!["timeout".to_owned()]);
    assert!(
        rig.time.elapsed() >= Duration::from_secs(600),
        "{:?}",
        rig.time.elapsed()
    );
}

// ── silent stops ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_deleted_attachment_never_becomes_ready() {
    // inst-bg-row-gone: stop without changes; the row stays as deleted
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    raw_exec(
        &rig.db.raw,
        &format!(
            "UPDATE attachments SET deleted_at = '2026-03-01 00:00:00+00:00', \
             cleanup_status = 'pending' WHERE id = {}",
            blob(id)
        ),
    )
    .await
    .unwrap();
    gate.open();
    settle().await;
    let row = rig.row(chat_id, id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.deleted_at.is_some());
    assert!(rig.metrics.background().is_empty());
}

#[tokio::test]
async fn a_row_of_a_deleted_chat_stays_uploaded_and_enqueues_nothing() {
    // chat deletion marks the attachments with cleanup_status; the task stops without changes
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("failed")).await;
    raw_exec(
        &rig.db.raw,
        &format!(
            "UPDATE attachments SET cleanup_status = 'pending' WHERE id = {}",
            blob(id)
        ),
    )
    .await
    .unwrap();
    gate.open();
    settle().await;
    let row = rig.row(chat_id, id).await;
    assert_eq!(
        row.status, "uploaded",
        "a row of a deleted chat never changes status"
    );
    assert_eq!(row.error_code, None);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
}

#[tokio::test]
async fn a_gear_stop_ends_the_task_and_the_row_stays_uploaded_for_the_reaper() {
    // inst-bg-gear-stop
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, id) = uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    rig.shutdown.cancel();
    // the task observes the stop while it waits on the provider; only then does the provider
    // answer, so the stop cannot race the `completed` answer
    settle().await;
    gate.open();
    settle().await;
    assert_eq!(rig.row(chat_id, id).await.status, "uploaded");
    assert!(rig.metrics.background().is_empty());
}

#[tokio::test]
async fn the_201_uploaded_is_a_success_for_the_upload_metric() {
    // result ok for each 201 including uploaded
    let rig = rig().await;
    let gate = Gate::new();
    uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    let recorded = rig.metrics.upload_bytes();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "document");
    gate.open();
}

#[tokio::test]
async fn a_second_upload_to_the_same_chat_while_the_first_is_indexing_reuses_the_store() {
    let rig = rig().await;
    let gate = Gate::new();
    let (chat_id, _) = uploaded_row(&rig, &gate, Answer::indexing("completed")).await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("completed"));
    let second = rig
        .upload(chat_id, "second.pdf", "application/pdf", b"%PDF second")
        .await;
    assert_created(&second, "ready");
    assert_eq!(rig.storage.count_of(Route::VsCreate), 1);
    gate.open();
}

#[tokio::test]
async fn an_error_response_of_the_deadline_path_is_never_a_201() {
    // a failed status read at the first poll is still a 503, not an uploaded row
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage
        .queue(Route::VsReadFile, Answer::indexing("failed"));
    let reply = rig.upload_pdf(chat.id).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(rig.only_row(chat.id).await.status, "failed");
}
