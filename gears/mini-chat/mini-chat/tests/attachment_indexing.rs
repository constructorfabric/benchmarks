//! The asynchronous indexing lifecycle of a document upload: `pending` -> `uploaded` -> `ready`
//! or `failed`, the request-phase polling (until the 25 s deadline), the background task (rounds
//! of 20 s, 10 minutes at most), provider failures and the cleanup hand-off.
//!
//! Source of truth: DESIGN 3.6 "File Upload" (sequence diagram and description), 3.3 "Upload
//! Attachment" (503 `service_unavailable` with `Retry-After: 10` and the generic detail, the row
//! gets `error_code = indexing_failed`), "Attachment status", B.9.5 (the background task refreshes
//! `updated_at`; the reaper takes what the task leaves), "Attachment cleanup state machine" and the
//! `attachment_indexing_failed` outbox event of "Attachment Deletion".
//!
//! The fixed production timings (25 s, 250 ms doubling to 2 s, rounds of 20 s up to 10 min, reads
//! doubling to 5 s) are pinned through `IndexingTimings::default()`; every other test shrinks them
//! with `AttachmentService::with_timings` so a case takes a fraction of a second.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::time::{Duration, Instant};

use attachment_support::{
    AttachmentHarness, AttachmentOptions, Cleanup, PDF, PDF_BYTES, StorageOp, StorageReply, XLSX,
    assert_problem, assert_unavailable, fast_timings, id_of, png,
};
use mini_chat::domain::attachments::IndexingTimings;
use serde_json::{Value, json};
use stream_support::contains_provider_identifier;
use time::OffsetDateTime;
use uuid::Uuid;

const PROVIDER_SECRET: &str = "provider-secret-detail-4711";

async fn harness_with(timings: IndexingTimings, cleanup: Cleanup) -> AttachmentHarness {
    AttachmentHarness::with(AttachmentOptions {
        timings,
        attachment_cleanup: cleanup,
        chat_cleanup: if cleanup == Cleanup::Real {
            Cleanup::Real
        } else {
            Cleanup::Recording
        },
        ..Default::default()
    })
    .await
}

/// Uploads a document whose indexing never finishes; returns its id (status `uploaded`).
async fn stuck_upload(h: &AttachmentHarness, chat: Uuid) -> Uuid {
    h.storage.never_finishes_indexing();
    let resp = h.upload(chat, "slow.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    let body = resp.json();
    assert_eq!(body["status"], json!("uploaded"), "{body}");
    id_of(&body)
}

fn index_completes(h: &AttachmentHarness) {
    h.storage.set_default(
        StorageOp::GetVectorStoreFile,
        StorageReply::vs_file("completed"),
    );
}

fn index_fails(h: &AttachmentHarness, status: &str) {
    h.storage
        .set_default(StorageOp::GetVectorStoreFile, StorageReply::vs_file(status));
}

// ═════════════════════════════════════════════════════════════════════════════
// the production timings
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn the_default_timings_are_the_documented_ones() {
    let timings = IndexingTimings::default();
    assert_eq!(
        timings.request_deadline,
        Duration::from_secs(25),
        "25 s after the upload started"
    );
    assert_eq!(timings.first_poll_delay, Duration::from_millis(250));
    assert_eq!(
        timings.max_poll_delay,
        Duration::from_secs(2),
        "250 ms doubling to 2 s"
    );
    assert_eq!(timings.background_round, Duration::from_secs(20));
    assert_eq!(
        timings.background_max,
        Duration::from_secs(600),
        "10 minutes"
    );
    assert_eq!(
        timings.background_max_poll_delay,
        Duration::from_secs(5),
        "250 ms doubling to 5 s"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// request phase
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_in_progress_file_is_polled_until_it_is_completed() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage.adding_answers_in_progress();
    h.storage
        .index_statuses(&["in_progress", "in_progress", "completed"]);

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert_eq!(
        resp.json()["status"],
        json!("ready"),
        "ready as soon as the provider says completed"
    );

    let polls = h.storage.calls_to(StorageOp::GetVectorStoreFile);
    assert_eq!(polls.len(), 3, "polled until completed, then stopped");
    let id = id_of(&resp.json());
    let provider_file = h.provider_file_of(id).await;
    let store = h.vector_store_of(chat).await;
    for poll in &polls {
        assert_eq!(poll.method, http::Method::GET);
        assert_eq!(
            poll.path(),
            format!("/provider-alias/v1/vector_stores/{store}/files/{provider_file}")
        );
    }
    assert_eq!(h.row(id).await.status, "ready");
    h.storage
        .assert_count_stays(StorageOp::GetVectorStoreFile, 3)
        .await;
}

#[tokio::test]
async fn a_poll_answer_without_status_counts_as_in_progress() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage.adding_answers_in_progress();
    h.storage.push(
        StorageOp::GetVectorStoreFile,
        StorageReply::vs_file_without_status(),
    );
    h.storage.push(
        StorageOp::GetVectorStoreFile,
        StorageReply::vs_file_without_status(),
    );

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert_eq!(resp.json()["status"], json!("ready"));
    assert_eq!(h.storage.count(StorageOp::GetVectorStoreFile), 3);
}

#[tokio::test]
async fn transient_read_errors_keep_the_polling_going() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage.adding_answers_in_progress();
    h.storage.push(
        StorageOp::GetVectorStoreFile,
        StorageReply::error(500, PROVIDER_SECRET),
    );
    h.storage.push(
        StorageOp::GetVectorStoreFile,
        StorageReply::error(503, PROVIDER_SECRET),
    );
    h.storage
        .push(StorageOp::GetVectorStoreFile, StorageReply::Unavailable);

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert_eq!(resp.json()["status"], json!("ready"));
    assert_eq!(h.storage.count(StorageOp::GetVectorStoreFile), 4);
}

#[tokio::test]
async fn a_failed_cancelled_or_unknown_status_fails_the_upload_with_503_and_deletes_the_file() {
    for status in ["failed", "cancelled", "expired"] {
        let h = harness_with(fast_timings(), Cleanup::Recording).await;
        let chat = h.seed_chat().await;
        h.storage.adding_answers_in_progress();
        h.storage.push(
            StorageOp::GetVectorStoreFile,
            StorageReply::vs_file("in_progress"),
        );
        h.storage
            .push(StorageOp::GetVectorStoreFile, StorageReply::vs_file(status));

        let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
        let problem = assert_unavailable(&resp, "10");
        assert_eq!(
            problem["detail"],
            json!("Service temporarily unavailable"),
            "{status}"
        );
        assert!(
            !resp.raw.contains("indexing_failed"),
            "the code stays internal: {}",
            resp.raw
        );
        assert!(!contains_provider_identifier(&resp.raw), "{}", resp.raw);

        let rows = h.rows_of(chat).await;
        assert_eq!(rows.len(), 1, "{status}");
        assert_eq!(rows[0].status, "failed", "{status}");
        assert_eq!(
            rows[0].error_code.as_deref(),
            Some("indexing_failed"),
            "{status}"
        );
        let provider_file = rows[0].provider_file_id.clone().unwrap();
        let deletes = h.storage.wait_calls(StorageOp::DeleteFile, 1).await;
        assert_eq!(
            deletes[0].last_segment(),
            provider_file,
            "{status}: inline best-effort delete"
        );
        h.storage.assert_count_stays(StorageOp::DeleteFile, 1).await;
        h.attachment_log.assert_count_stays(0).await;
        assert_eq!(
            h.storage.count(StorageOp::GetVectorStoreFile),
            2,
            "polling stops at the failure"
        );
    }
}

#[tokio::test]
async fn a_non_transient_read_error_fails_the_upload() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    h.storage.adding_answers_in_progress();
    h.storage
        .push(StorageOp::GetVectorStoreFile, StorageReply::not_found());

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_unavailable(&resp, "10");
    let row = &h.rows_of(chat).await[0];
    assert_eq!(
        (row.status.as_str(), row.error_code.as_deref()),
        ("failed", Some("indexing_failed"))
    );
    h.storage.wait_calls(StorageOp::DeleteFile, 1).await;
}

#[tokio::test]
async fn polling_stops_at_the_deadline_and_the_upload_returns_201_uploaded() {
    let timings = fast_timings();
    let h = harness_with(timings, Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    h.storage.never_finishes_indexing();

    let started = Instant::now();
    let resp = h.upload(chat, "slow.pdf", PDF, PDF_BYTES).await;
    let elapsed = started.elapsed();
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    let body = resp.json();
    assert_eq!(body["status"], json!("uploaded"));
    assert!(
        elapsed >= timings.request_deadline.mul_f32(0.9),
        "the upload waits for the deadline, not less: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "and answers right after it: {elapsed:?}"
    );
    assert!(body.get("error_code").is_none_or(Value::is_null));

    let id = id_of(&body);
    let row = h.row(id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.provider_file_id.is_some());
    assert_eq!(
        h.stores_of(chat).await.len(),
        1,
        "the file is already in the chat store"
    );
    assert_eq!(h.get(chat, id).await.json()["status"], json!("uploaded"));
    assert!(
        h.storage.count(StorageOp::GetVectorStoreFile) >= 2,
        "it polled until the deadline"
    );
    // no failure: nothing is deleted, nothing is enqueued
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 0);
}

#[tokio::test]
async fn a_hanging_status_read_does_not_hold_the_upload_past_its_deadline() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    h.storage.adding_answers_in_progress();
    h.storage.hold(StorageOp::GetVectorStoreFile);

    let started = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        h.upload(chat, "a.pdf", PDF, PDF_BYTES),
    )
    .await
    .expect("each status read is bounded by the same deadline");
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert_eq!(resp.json()["status"], json!("uploaded"));
    assert!(started.elapsed() < Duration::from_secs(4));
    h.storage.release();
}

#[tokio::test]
async fn images_and_spreadsheets_are_never_polled() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage.never_finishes_indexing();
    let image = h.uploaded(chat, "p.png", "image/png", &png(10, 10)).await;
    let sheet = h.uploaded(chat, "s.xlsx", XLSX, b"PK").await;
    assert_eq!(image["status"], json!("ready"));
    assert_eq!(sheet["status"], json!("ready"));
    assert_eq!(h.storage.count(StorageOp::GetVectorStoreFile), 0);
    assert_eq!(h.storage.count(StorageOp::AddVectorStoreFile), 0);
}

// ═════════════════════════════════════════════════════════════════════════════
// background task
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_background_task_makes_the_document_ready_when_indexing_completes() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    index_completes(&h);
    let row = h.wait_status(id, "ready").await;
    assert!(row.error_code.is_none());
    assert!(row.cleanup_status.is_none());
    let body = h.get(chat, id).await.json();
    assert_eq!(body["status"], json!("ready"));
    // usable now: a message may reference it
    assert_eq!(
        h.say_with(chat, "what does it say?", &[id])
            .await
            .terminal()
            .event,
        "done"
    );
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_message_can_not_reference_the_document_before_it_is_ready() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    let resp = h.reject_with(chat, "q", &[id]).await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    chat_support::assert_field_violation(&problem, "attachment", "invalid_attachment");
    h.oagw.assert_no_provider_call();
    assert!(h.links_of_chat(chat).await.is_empty());
}

#[tokio::test]
async fn every_background_round_refreshes_updated_at() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;
    let first = h.row(id).await.updated_at;

    let later = h
        .wait_row(id, "a refreshed updated_at", |row| row.updated_at > first)
        .await;
    assert_eq!(later.status, "uploaded", "still waiting");
    assert!(later.updated_at > first);
}

#[tokio::test]
async fn a_failed_status_in_the_background_fails_the_row_and_hands_the_file_to_the_outbox() {
    for status in ["failed", "cancelled"] {
        let h = harness_with(fast_timings(), Cleanup::Recording).await;
        let chat = h.seed_chat().await;
        let id = stuck_upload(&h, chat).await;
        let provider_file = h.provider_file_of(id).await;

        index_fails(&h, status);
        let row = h.wait_status(id, "failed").await;
        assert_eq!(
            row.error_code.as_deref(),
            Some("indexing_failed"),
            "{status}"
        );
        assert_eq!(
            row.cleanup_status.as_deref(),
            Some("pending"),
            "{status}: failed and cleanup pending in one transaction"
        );
        assert!(
            row.deleted_at.is_none(),
            "{status}: failed, not soft-deleted"
        );

        let payloads = h.attachment_log.wait_for(1).await;
        assert_eq!(
            payloads[0]["event_type"],
            json!("attachment_indexing_failed"),
            "{status}"
        );
        assert_eq!(payloads[0]["attachment_id"], json!(id.to_string()));
        assert_eq!(payloads[0]["chat_id"], json!(chat.to_string()));
        assert_eq!(payloads[0]["provider_file_id"], json!(provider_file));
        assert_eq!(payloads[0]["attachment_kind"], json!("document"));
        // there is no inline delete: only the outbox deletes the file
        h.storage.assert_count_stays(StorageOp::DeleteFile, 0).await;
        h.attachment_log.assert_count_stays(1).await;

        let body = h.get(chat, id).await.json();
        assert_eq!(body["status"], json!("failed"));
        assert_eq!(body["error_code"], json!("indexing_failed"));
        assert!(!body.to_string().contains("file-s6fake"));
    }
}

#[tokio::test]
async fn the_outbox_cleanup_deletes_the_provider_file_of_a_failed_indexing() {
    let h = harness_with(fast_timings(), Cleanup::Real).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;
    let provider_file = h.provider_file_of(id).await;

    index_fails(&h, "failed");
    h.wait_status(id, "failed").await;
    let row = h.wait_cleanup(id, "done").await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    let deletes = h.storage.calls_to(StorageOp::DeleteFile);
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].last_segment(), provider_file);
}

#[tokio::test]
async fn the_background_wait_times_out_after_the_limit() {
    let timings = IndexingTimings {
        background_max: Duration::from_millis(700),
        ..fast_timings()
    };
    let h = harness_with(timings, Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    let row = h.wait_status(id, "failed").await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let payloads = h.attachment_log.wait_for(1).await;
    assert_eq!(
        payloads[0]["event_type"],
        json!("attachment_indexing_failed")
    );
    // the task is over: no more polls, no second message
    let polls = h.storage.count(StorageOp::GetVectorStoreFile);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.storage.count(StorageOp::GetVectorStoreFile), polls);
    h.attachment_log.assert_count_stays(1).await;
}

#[tokio::test]
async fn transient_errors_in_the_background_do_not_end_the_wait() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    h.storage.set_default(
        StorageOp::GetVectorStoreFile,
        StorageReply::error(500, PROVIDER_SECRET),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.row(id).await.status,
        "uploaded",
        "a transient error is not a failure"
    );
    index_completes(&h);
    h.wait_status(id, "ready").await;
}

#[tokio::test]
async fn deleting_the_attachment_stops_the_background_task_without_changes() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let payloads = h.attachment_log.wait_for(1).await;
    assert_eq!(payloads[0]["event_type"], json!("attachment_deleted"));

    index_completes(&h);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let row = h.row(id).await;
    assert_eq!(row.status, "uploaded", "a deleted row never becomes ready");
    assert!(row.deleted_at.is_some());
    assert!(row.error_code.is_none());
    h.attachment_log.assert_count_stays(1).await;
}

#[tokio::test]
async fn deleting_the_chat_stops_the_background_task_and_the_row_never_becomes_ready() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);
    assert_eq!(
        h.row(id).await.cleanup_status.as_deref(),
        Some("pending"),
        "set by the chat delete"
    );

    index_completes(&h);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let row = h.row(id).await;
    assert_eq!(
        row.status, "uploaded",
        "the chat cleanup owns the file; the row is not made ready"
    );
    assert!(row.error_code.is_none());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(
        h.attachment_log.received().is_empty(),
        "no attachment cleanup message of the task: {:?}",
        h.attachment_log.received()
    );
}

#[tokio::test]
async fn a_row_the_reaper_already_failed_is_left_alone_by_the_task() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    // far in the future: the row looks abandoned
    let swept = h
        .sweep(OffsetDateTime::now_utc() + time::Duration::hours(2))
        .await;
    assert_eq!(swept, 1);
    assert_eq!(
        h.row(id).await.error_code.as_deref(),
        Some("upload_abandoned")
    );

    index_completes(&h);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let row = h.row(id).await;
    assert_eq!(
        row.status, "failed",
        "the task never overwrites a terminal row"
    );
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
}

#[tokio::test]
async fn stopping_the_gear_cancels_the_wait_and_leaves_the_row_uploaded() {
    let timings = IndexingTimings {
        background_max: Duration::from_millis(600),
        ..fast_timings()
    };
    let h = harness_with(timings, Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let id = stuck_upload(&h, chat).await;

    h.attachments.stop().await;
    let polls = h.storage.count(StorageOp::GetVectorStoreFile);
    // past the 10 minute limit of the (shrunk) task: nothing was decided
    tokio::time::sleep(Duration::from_millis(900)).await;
    let row = h.row(id).await;
    assert_eq!(row.status, "uploaded", "left for the upload reaper");
    assert!(row.error_code.is_none() && row.cleanup_status.is_none());
    assert_eq!(
        h.storage.count(StorageOp::GetVectorStoreFile),
        polls,
        "no more polling"
    );
    h.attachment_log.assert_count_stays(0).await;

    // the reaper finishes it
    assert_eq!(
        h.sweep(OffsetDateTime::now_utc() + time::Duration::hours(2))
            .await,
        1
    );
    assert_eq!(
        h.row(id).await.error_code.as_deref(),
        Some("upload_abandoned")
    );
}

#[tokio::test]
async fn two_uploads_are_indexed_independently() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    let first = stuck_upload(&h, chat).await;
    // the second one finishes inside its request: its add answers `completed`, while the status
    // reads of the first keep answering `in_progress` (clearing the read default too would let
    // the first background poll - at most 40 ms away - win the race against the second upload)
    h.storage.clear_default(StorageOp::AddVectorStoreFile);
    let second = h.doc(chat, "fast.pdf").await;

    assert_eq!(h.row(second).await.status, "ready");
    assert_eq!(
        h.row(first).await.status,
        "uploaded",
        "the first is still being indexed"
    );
    index_completes(&h);
    h.wait_status(first, "ready").await;
}

// ═════════════════════════════════════════════════════════════════════════════
// the request-phase failure leaves a consistent chat
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_failed_document_is_not_usable_and_can_be_deleted() {
    let h = harness_with(fast_timings(), Cleanup::Recording).await;
    let chat = h.seed_chat().await;
    h.storage.adding_answers_in_progress();
    h.storage.push(
        StorageOp::GetVectorStoreFile,
        StorageReply::vs_file("failed"),
    );
    assert_eq!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        503
    );
    let id = h.rows_of(chat).await[0].id;

    let resp = h.reject_with(chat, "q", &[id]).await;
    assert_problem(&resp, 400, "invalid_argument");
    // no file_search tool for a chat whose only document failed
    h.storage.clear_default(StorageOp::AddVectorStoreFile);
    let sent = h.say(chat, "hello").await;
    assert_eq!(sent.terminal().event, "done");
    assert!(h.oagw.last_request().tool("file_search").is_none());

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
}
