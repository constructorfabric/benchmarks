//! The upload reaper (DESIGN B.9.5, 3.3 "Upload Attachment" last paragraphs): abandoned `pending` /
//! `uploaded` rows become `failed` with `error_code = upload_abandoned`, the provider file recorded
//! on the row is handed to the attachment-cleanup outbox (`event_type =
//! attachment_upload_abandoned`), rows owned by chat cleanup are skipped.
//!
//! The scan is driven with `UploadReaper::sweep_at(now)` (the application clock, like the S5 orphan
//! watchdog); `run` is checked once with a real 1 s interval.

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

use std::sync::Arc;
use std::time::Duration;

use attachment_support::{
    AttRow, AttachmentHarness, AttachmentOptions, Cleanup, PDF, PDF_BYTES, StorageOp,
    assert_problem, eventually, from_db, ts,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Reaper config of most tests: the minimum `stale_after_secs`.
fn sixty() -> serde_json::Value {
    json!({"upload_reaper": {"stale_after_secs": 60}})
}

async fn recording(config: serde_json::Value) -> AttachmentHarness {
    AttachmentHarness::with(AttachmentOptions {
        config,
        attachment_cleanup: Cleanup::Recording,
        chat_cleanup: Cleanup::Recording,
        ..Default::default()
    })
    .await
}

// ═════════════════════════════════════════════════════════════════════════════
// what is taken and what is left
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_stale_pending_upload_without_a_provider_file_is_failed_without_any_message() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("pending")
                .provider_file(None)
                .touched(ts(0)),
        )
        .await;

    let now = ts(1000);
    assert_eq!(h.sweep(now).await, 1);
    let row = h.row(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(
        from_db(row.updated_at),
        now,
        "the application clock is the write time"
    );
    assert!(row.deleted_at.is_none(), "failed, not soft-deleted");
    assert!(
        row.cleanup_status.is_none(),
        "there is no provider file to clean"
    );
    h.storage.assert_no_calls();
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_stale_uploaded_row_with_a_provider_file_is_failed_and_its_file_goes_to_the_outbox() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("uploaded")
                .touched(ts(0)),
        )
        .await;
    let provider_file = h.provider_file_of(id).await;

    assert_eq!(h.sweep(ts(1000)).await, 1);
    let row = h.row(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(
        row.cleanup_status.as_deref(),
        Some("pending"),
        "set in the same transaction"
    );
    assert!(row.deleted_at.is_none());

    let payloads = h.attachment_log.wait_for(1).await;
    assert_eq!(payloads.len(), 1);
    let payload = &payloads[0];
    assert_eq!(payload["event_type"], json!("attachment_upload_abandoned"));
    assert_eq!(payload["tenant_id"], json!(common::TENANT_A.to_string()));
    assert_eq!(payload["chat_id"], json!(chat.to_string()));
    assert_eq!(payload["attachment_id"], json!(id.to_string()));
    assert_eq!(payload["provider_file_id"], json!(provider_file));
    assert_eq!(payload["storage_backend"], json!("openai"));
    assert_eq!(payload["attachment_kind"], json!("document"));
    assert!(payload["vector_store_id"].is_null());
    h.attachment_log.assert_count_stays(1).await;
}

#[tokio::test]
async fn the_abandoned_message_carries_no_secondary_ref() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    h.seed(
        AttRow::image(chat, "a.png")
            .status("uploaded")
            .secondary("file-anthropic-secondary-3")
            .touched(ts(0)),
    )
    .await;
    assert_eq!(h.sweep(ts(1000)).await, 1);
    let payload = h.attachment_log.wait_for(1).await.remove(0);
    assert!(
        payload["secondary_ref"].is_null(),
        "an Anthropic secondary copy is not deleted: {payload}"
    );
    assert_eq!(payload["attachment_kind"], json!("image"));
}

#[tokio::test]
async fn the_outbox_cleanup_deletes_the_provider_file_and_the_row_stays_failed_and_visible() {
    let h = AttachmentHarness::with(AttachmentOptions {
        config: sixty(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("uploaded")
                .touched(ts(0)),
        )
        .await;
    let provider_file = h.provider_file_of(id).await;

    assert_eq!(h.sweep(ts(1000)).await, 1);
    let row = h.wait_cleanup(id, "done").await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert!(row.deleted_at.is_none());
    let deletes = h.storage.calls_to(StorageOp::DeleteFile);
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].last_segment(), provider_file);

    // still visible through GET with the failure
    let got = h.get(chat, id).await;
    assert_eq!(got.status.as_u16(), 200, "{}", got.raw);
    let body = got.json();
    assert_eq!(body["status"], json!("failed"));
    assert_eq!(body["error_code"], json!("upload_abandoned"));
    assert!(!body.to_string().contains(&provider_file));
}

#[tokio::test]
async fn images_are_reaped_like_documents() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::image(chat, "a.png")
                .status("pending")
                .touched(ts(0)),
        )
        .await;
    assert_eq!(h.sweep(ts(1000)).await, 1);
    assert_eq!(h.row(id).await.status, "failed");
}

#[tokio::test]
async fn only_stale_pending_and_uploaded_rows_of_live_unmarked_attachments_are_taken() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let ready = h
        .seed(AttRow::document(chat, "ready.pdf").touched(ts(0)))
        .await;
    let failed = h
        .seed(
            AttRow::document(chat, "failed.pdf")
                .status("failed")
                .error("upload_failed")
                .touched(ts(0)),
        )
        .await;
    let deleted = h
        .seed(
            AttRow::document(chat, "deleted.pdf")
                .status("uploaded")
                .deleted()
                .touched(ts(0)),
        )
        .await;
    let marked: Vec<Uuid> = {
        let mut ids = Vec::new();
        for status in ["pending", "done", "failed"] {
            ids.push(
                h.seed(
                    AttRow::document(chat, "marked.pdf")
                        .status("uploaded")
                        .cleanup(status, 0)
                        .touched(ts(0)),
                )
                .await,
            );
        }
        ids
    };
    let fresh = h
        .seed(
            AttRow::document(chat, "fresh.pdf")
                .status("uploaded")
                .touched(ts(990)),
        )
        .await;

    assert_eq!(h.sweep(ts(1000)).await, 0);
    assert_eq!(h.row(ready).await.status, "ready");
    assert_eq!(
        h.row(failed).await.error_code.as_deref(),
        Some("upload_failed")
    );
    assert_eq!(h.row(deleted).await.status, "uploaded");
    for id in marked {
        let row = h.row(id).await;
        assert_eq!(
            row.status, "uploaded",
            "chat cleanup owns the file of a row with a cleanup status"
        );
    }
    assert_eq!(h.row(fresh).await.status, "uploaded");
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn the_staleness_boundary_is_strictly_older_than_stale_after_secs() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let at_cutoff = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("pending")
                .provider_file(None)
                .touched(ts(940)),
        )
        .await;
    let older = h
        .seed(
            AttRow::document(chat, "b.pdf")
                .status("pending")
                .provider_file(None)
                .touched(ts(939)),
        )
        .await;

    // now = ts(1000): cutoff = ts(940); `updated_at < cutoff`
    assert_eq!(h.sweep(ts(1000)).await, 1);
    assert_eq!(h.row(at_cutoff).await.status, "pending");
    assert_eq!(h.row(older).await.status, "failed");
}

#[tokio::test]
async fn the_default_stale_after_is_300_seconds() {
    let h = recording(json!({})).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("pending")
                .provider_file(None)
                .touched(ts(0)),
        )
        .await;
    assert_eq!(h.sweep(ts(300)).await, 0, "exactly 300 s old is not stale");
    assert_eq!(h.sweep(ts(301)).await, 1);
    assert_eq!(h.row(id).await.status, "failed");
}

#[tokio::test]
async fn staleness_follows_updated_at_not_created_at() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let mut row = AttRow::document(chat, "a.pdf")
        .status("uploaded")
        .touched(ts(0));
    row.updated_at = ts(990); // refreshed by the background indexing task
    let id = h.seed(row).await;
    assert_eq!(h.sweep(ts(1000)).await, 0);
    assert_eq!(h.row(id).await.status, "uploaded");
}

#[tokio::test]
async fn a_row_is_taken_once() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    h.seed(
        AttRow::document(chat, "a.pdf")
            .status("uploaded")
            .touched(ts(0)),
    )
    .await;
    assert_eq!(h.sweep(ts(1000)).await, 1);
    assert_eq!(h.sweep(ts(2000)).await, 0, "the row is failed now");
    h.attachment_log.assert_count_stays(1).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// batching and concurrency
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_scan_takes_at_most_100_rows_oldest_first() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let mut ids = Vec::new();
    for n in 0..105_i64 {
        ids.push(
            h.seed(
                AttRow::document(chat, "a.pdf")
                    .status("pending")
                    .provider_file(None)
                    .touched(ts(n)),
            )
            .await,
        );
    }

    assert_eq!(h.sweep(ts(10_000)).await, 100);
    let remaining: Vec<_> = {
        let mut out = Vec::new();
        for (n, id) in ids.iter().enumerate() {
            if h.row(*id).await.status == "pending" {
                out.push(n);
            }
        }
        out
    };
    assert_eq!(
        remaining,
        vec![100, 101, 102, 103, 104],
        "the newest five wait for the next scan"
    );
    assert_eq!(h.sweep(ts(10_000)).await, 5);
    assert_eq!(h.sweep(ts(10_000)).await, 0);
}

#[tokio::test]
async fn concurrent_scans_take_every_row_exactly_once() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    for n in 0..6_i64 {
        h.seed(
            AttRow::document(chat, "a.pdf")
                .status("uploaded")
                .touched(ts(n)),
        )
        .await;
    }
    let now = ts(10_000);
    let (a, b, c) = tokio::join!(h.sweep(now), h.sweep(now), h.sweep(now));
    assert_eq!(
        a + b + c,
        6,
        "a lost conditional update takes nothing: {a} {b} {c}"
    );
    for row in h.rows_of(chat).await {
        assert_eq!(row.status, "failed");
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    }
    let payloads = h.attachment_log.wait_for(6).await;
    assert_eq!(payloads.len(), 6);
    h.attachment_log.assert_count_stays(6).await;
}

#[tokio::test]
async fn a_failing_enqueue_rolls_that_row_back_and_does_not_block_the_others() {
    let h = AttachmentHarness::with(AttachmentOptions {
        config: sixty(),
        attachment_cleanup: Cleanup::Missing,
        chat_cleanup: Cleanup::Recording,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let with_file = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("uploaded")
                .touched(ts(0)),
        )
        .await;
    let without_file = h
        .seed(
            AttRow::document(chat, "b.pdf")
                .status("pending")
                .provider_file(None)
                .touched(ts(1)),
        )
        .await;

    h.reaper.sweep_at(ts(1000)).await.ok();
    let row = h.row(with_file).await;
    assert_eq!(
        row.status, "uploaded",
        "the transaction rolled back: the file is not forgotten"
    );
    assert!(row.cleanup_status.is_none() && row.error_code.is_none());
    assert_eq!(
        h.row(without_file).await.status,
        "failed",
        "the row that needs no message is reaped"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// effects on the chat
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_reaped_row_no_longer_counts_toward_the_document_limit() {
    let h = recording(
        json!({"rag": {"max_documents_per_chat": 1}, "upload_reaper": {"stale_after_secs": 60}}),
    )
    .await;
    let chat = h.seed_chat().await;
    h.seed(
        AttRow::document(chat, "stuck.pdf")
            .status("uploaded")
            .touched(ts(0)),
    )
    .await;

    let blocked = h.upload(chat, "new.pdf", PDF, PDF_BYTES).await;
    assert_problem(&blocked, 429, "resource_exhausted");
    assert_eq!(h.sweep(ts(1000)).await, 1);
    let resp = h.upload(chat, "new.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
}

#[tokio::test]
async fn a_reaped_attachment_can_not_be_referenced_by_a_message_and_can_be_deleted() {
    let h = recording(sixty()).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "stuck.pdf")
                .status("uploaded")
                .touched(ts(0)),
        )
        .await;
    assert_eq!(h.sweep(ts(1000)).await, 1);

    let resp = h.reject_with(chat, "q", &[id]).await;
    assert_problem(&resp, 400, "invalid_argument");
    h.oagw.assert_no_provider_call();
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
}

// ═════════════════════════════════════════════════════════════════════════════
// the periodic task
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn run_scans_every_scan_interval_until_cancelled() {
    let h = recording(json!({})).await;
    let chat = h.seed_chat().await;
    let stale = h
        .seed(
            AttRow::document(chat, "stale.pdf")
                .status("uploaded")
                .touched(ts(0)),
        )
        .await;
    let reaper =
        h.reaper_with(json!({"upload_reaper": {"scan_interval_secs": 1, "stale_after_secs": 60}}));

    let cancel = CancellationToken::new();
    let task = tokio::spawn(Arc::clone(&reaper).run(cancel.child_token()));
    // a first scan within one interval; the application clock is the wall clock (2025 rows are stale)
    let row = h
        .wait_row(stale, "the periodic scan", |row| row.status == "failed")
        .await;
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert!(OffsetDateTime::now_utc() - from_db(row.updated_at) < time::Duration::minutes(5));

    // rows that become stale later are picked up by a later scan
    let late = h
        .seed(
            AttRow::document(chat, "late.pdf")
                .status("pending")
                .provider_file(None)
                .touched(ts(5)),
        )
        .await;
    h.wait_row(late, "the next scan", |row| row.status == "failed")
        .await;

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run returns after the cancellation")
        .expect("the task does not panic");
}

#[tokio::test]
async fn run_returns_at_once_when_the_reaper_is_disabled() {
    let h = recording(json!({})).await;
    let chat = h.seed_chat().await;
    let stale = h
        .seed(
            AttRow::document(chat, "stale.pdf")
                .status("uploaded")
                .touched(ts(0)),
        )
        .await;
    let reaper = h.reaper_with(json!({"upload_reaper": {"enabled": false, "scan_interval_secs": 1, "stale_after_secs": 60}}));

    let cancel = CancellationToken::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        Arc::clone(&reaper).run(cancel.child_token()),
    )
    .await
    .expect("a disabled reaper does not run");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(h.row(stale).await.status, "uploaded", "nothing was scanned");
    eventually("still nothing", || async {
        h.row(stale).await.status == "uploaded"
    })
    .await;
}
