//! The upload reaper over in-memory `SQLite`, the real outbox and a virtual clock: the leader
//! gate, the cutoff on the application clock, the `pending` and `uploaded` rows only (not
//! deleted, no cleanup status), the guarded failure with `upload_abandoned`, the cleanup message
//! and `cleanup_status` `pending` only for a row with a provider file id, the 100-row batch with
//! the oldest `updated_at` first and the chronological comparison of timestamps whose fractional
//! seconds differ in length, the skipped rows whose guard lost, a failing row that a later scan
//! retries, the metrics after the commit, the visibility of the reaped row through the get flow
//! and the worker loop on the virtual timer.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper` (`inst-rp-*`),
//! `-cleanup-enqueue` (`inst-enq-reaper-secondary`, `inst-enq-reaper-warn`), `-metrics`
//! (`inst-met-reaper`), `-configuration` (`inst-cfg-workers`), state
//! `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status` (`inst-cs-pending`);
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-reaper`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::attachments::reaper::REAPER_BATCH;
use crate::infra::outbox::partition_for;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::infra::upload_reaper::{ReaperReport, ReaperSettings};
use crate::test_support::attachments::{
    AttachRig, AttachRigOptions, FailingReaperStore, RacingReaperStore, RowSeed, ToggleElector,
    blob, reaper_defaults, rig, rig_with, seed_row,
};
use crate::test_support::chats::{at, fail_outbox_inserts, seed_chat};
use crate::test_support::quota::{STANDARD, default_now};
use crate::test_support::{TENANT_A, TENANT_B};

const STALE: i64 = 300;

/// The instant `seconds` before the virtual now.
fn ago(seconds: i64) -> OffsetDateTime {
    default_now() - time::Duration::seconds(seconds)
}

fn stale(seed: RowSeed) -> RowSeed {
    seed.updated(ago(STALE + 100))
}

fn uploaded() -> RowSeed {
    RowSeed::document().status("uploaded")
}

fn pending() -> RowSeed {
    RowSeed::document().status("pending").no_provider_file()
}

async fn scan(rig: &AttachRig) -> ReaperReport {
    rig.reaper().scan_once().await.unwrap()
}

async fn status_of(rig: &AttachRig, chat: &ChatRecord, id: Uuid) -> String {
    rig.row(chat.id, id).await.status
}

// ── the leader gate and the switch ───────────────────────────────────────────

#[tokio::test]
async fn a_non_leader_scans_nothing() {
    // inst-rp-leader: the scan is skipped without selecting, reaping or recording
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let reaper = rig.reaper_with(reaper_defaults(), ToggleElector::follower(), None);
    assert_eq!(reaper.scan_once().await.unwrap(), ReaperReport::default());
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
    assert!(rig.attachment_cleanup_rows().await.is_empty());
    assert!(
        rig.metrics.reaper_scans().is_empty(),
        "no scan, no duration"
    );
    assert!(rig.metrics.abandoned().is_empty());
}

#[tokio::test]
async fn gaining_leadership_makes_the_next_scan_work() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let elector = ToggleElector::follower();
    let reaper = rig.reaper_with(reaper_defaults(), elector.clone(), None);
    reaper.scan_once().await.unwrap();
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
    elector.0.store(true, std::sync::atomic::Ordering::SeqCst);
    let report = reaper.scan_once().await.unwrap();
    assert!(report.scanned);
    assert_eq!(status_of(&rig, &chat, id).await, "failed");
}

#[tokio::test]
async fn a_disabled_reaper_returns_without_scanning() {
    // inst-rp-disabled, inst-rp-return-disabled
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let settings = ReaperSettings {
        enabled: false,
        ..reaper_defaults()
    };
    let reaper = rig.reaper_with(settings, ToggleElector::leader(), None);
    let finished =
        tokio::time::timeout(Duration::from_secs(5), reaper.run(CancellationToken::new())).await;
    assert!(finished.is_ok(), "the loop returns at once when disabled");
    assert!(rig.time.sleeps().is_empty(), "it never waited for a scan");
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
}

// ── the selection ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_stale_uploaded_row_fails_with_upload_abandoned() {
    // inst-rp-cutoff, inst-rp-select, inst-rp-cas, inst-rp-each
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let report = scan(&rig).await;
    assert_eq!(
        report,
        ReaperReport {
            scanned: true,
            candidates: 1,
            reaped: 1,
            skipped: 0,
            failed: 0,
        }
    );
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(row.updated_at, default_now(), "the application clock");
    assert_eq!(row.deleted_at, None, "the reaped row is not soft-deleted");
}

#[tokio::test]
async fn a_stale_pending_row_fails_with_upload_abandoned() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(pending())).await;
    assert_eq!(scan(&rig).await.reaped, 1);
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
}

#[tokio::test]
async fn a_fresh_row_is_left_alone() {
    // `updated_at` newer than the cutoff
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let fresh_uploaded = seed_row(&rig.db, &chat, uploaded().updated(ago(STALE - 1))).await;
    let fresh_pending = seed_row(&rig.db, &chat, pending().updated(ago(1))).await;
    let report = scan(&rig).await;
    assert_eq!((report.candidates, report.reaped), (0, 0));
    assert_eq!(status_of(&rig, &chat, fresh_uploaded).await, "uploaded");
    assert_eq!(status_of(&rig, &chat, fresh_pending).await, "pending");
}

#[tokio::test]
async fn the_cutoff_is_strictly_before_now_minus_stale_after() {
    // inst-rp-cutoff: "updated_at before the cutoff"
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let exact = seed_row(&rig.db, &chat, uploaded().updated(ago(STALE))).await;
    let older = seed_row(
        &rig.db,
        &chat,
        uploaded().updated(ago(STALE) - time::Duration::microseconds(1)),
    )
    .await;
    scan(&rig).await;
    assert_eq!(status_of(&rig, &chat, exact).await, "uploaded");
    assert_eq!(status_of(&rig, &chat, older).await, "failed");
}

#[tokio::test]
async fn the_threshold_is_stale_after_secs_and_follows_the_application_clock() {
    // inst-rp-cutoff: now minus `upload_reaper.stale_after_secs` on the application clock
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, uploaded().updated(default_now())).await;
    assert_eq!(scan(&rig).await.reaped, 0);
    rig.time.advance(Duration::from_secs(301));
    assert_eq!(scan(&rig).await.reaped, 1);
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.updated_at, default_now() + time::Duration::seconds(301));
}

#[tokio::test]
async fn a_configured_threshold_replaces_the_default() {
    // inst-rp-cutoff, inst-cfg-workers: `upload_reaper.stale_after_secs`
    let configured = ReaperSettings {
        stale_after: Duration::from_secs(1_000),
        ..reaper_defaults()
    };
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, uploaded().updated(ago(500))).await;
    let reaper = rig.reaper_with(configured, ToggleElector::leader(), None);
    assert_eq!(reaper.scan_once().await.unwrap().reaped, 0);
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
}

#[tokio::test]
async fn only_pending_and_uploaded_rows_are_candidates() {
    // inst-rp-select: `ready` and `failed` rows are never reaped
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let ready = seed_row(&rig.db, &chat, stale(RowSeed::document().status("ready"))).await;
    let failed = seed_row(
        &rig.db,
        &chat,
        stale(
            RowSeed::document()
                .status("failed")
                .error("indexing_failed"),
        ),
    )
    .await;
    let report = scan(&rig).await;
    assert_eq!((report.candidates, report.reaped), (0, 0));
    assert_eq!(status_of(&rig, &chat, ready).await, "ready");
    let row = rig.row(chat.id, failed).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
}

#[tokio::test]
async fn a_soft_deleted_row_is_never_reaped() {
    // inst-rp-select: `deleted_at IS NULL`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded().deleted(ago(10)))).await;
    assert_eq!(scan(&rig).await.candidates, 0);
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
}

#[tokio::test]
async fn a_row_with_a_cleanup_status_is_never_reaped() {
    // inst-rp-select: `cleanup_status IS NULL` (claimed by chat cleanup or the indexing failure)
    for status in ["pending", "done", "failed"] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let id = seed_row(
            &rig.db,
            &chat,
            stale(uploaded().cleanup(match status {
                "pending" => "pending",
                "done" => "done",
                _ => "failed",
            })),
        )
        .await;
        assert_eq!(scan(&rig).await.candidates, 0, "{status}");
        assert_eq!(status_of(&rig, &chat, id).await, "uploaded", "{status}");
        assert!(rig.attachment_cleanup_rows().await.is_empty(), "{status}");
    }
}

#[tokio::test]
async fn every_tenant_is_scanned() {
    // the reaper is a system scan; every row is handled inside its own tenant
    let rig = rig().await;
    let chat_a = rig.std_chat().await;
    let who_b = crate::test_support::chats::b1();
    let chat_b = seed_chat(&rig.db, &who_b, Uuid::new_v4(), Some("b"), STANDARD, at(0)).await;
    let a = seed_row(&rig.db, &chat_a, stale(uploaded())).await;
    let b = seed_row(
        &rig.db,
        &chat_b,
        stale(uploaded().tenant(TENANT_B).uploader(who_b.subject_id())),
    )
    .await;
    assert_eq!(scan(&rig).await.reaped, 2);
    assert_eq!(status_of(&rig, &chat_a, a).await, "failed");
    assert_eq!(status_of(&rig, &chat_b, b).await, "failed");
    let partitions: Vec<i64> = rig
        .attachment_cleanup_rows()
        .await
        .iter()
        .map(|row| row.partition)
        .collect();
    assert_eq!(partitions.len(), 2);
    let expected = |tenant: Uuid| i64::from(partition_for(tenant, rig.outbox.num_partitions));
    assert!(partitions.contains(&expected(TENANT_A)));
    assert!(partitions.contains(&expected(TENANT_B)));
}

// ── the cleanup message ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_row_with_a_provider_file_gets_cleanup_pending_and_one_message() {
    // inst-rp-enqueue, inst-cs-pending, inst-enq-payload
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let seed = stale(uploaded().file_id("file-abandoned"));
    let id = seed_row(&rig.db, &chat, seed).await;
    scan(&rig).await;
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_updated_at, Some(default_now()));
    assert_eq!(row.cleanup_attempts, 0);
    let messages = rig.attachment_cleanup_rows().await;
    assert_eq!(messages.len(), 1);
    let payload = &messages[0].payload;
    assert_eq!(payload["event_type"], "attachment_upload_abandoned");
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["chat_id"], chat.id.to_string());
    assert_eq!(payload["attachment_id"], id.to_string());
    assert_eq!(payload["provider_file_id"], "file-abandoned");
    assert!(payload["vector_store_id"].is_null(), "{payload}");
    assert_eq!(payload["storage_backend"], "azure_openai");
    assert_eq!(payload["attachment_kind"], "document");
    assert!(payload["secondary_ref"].is_null(), "{payload}");
    assert_eq!(messages[0].queue, rig.outbox.cleanup_queue_name);
    assert_eq!(
        messages[0].partition,
        i64::from(partition_for(TENANT_A, rig.outbox.num_partitions))
    );
}

#[tokio::test]
async fn a_pending_row_has_no_provider_file_so_no_message_and_no_cleanup_status() {
    // inst-rp-enqueue: only a row with a `provider_file_id`; inst-rp-limits: a provider-side
    // orphan of a dropped request is not deleted (P2 reconciliation)
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(pending())).await;
    assert_eq!(scan(&rig).await.reaped, 1);
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.cleanup_updated_at, None);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
    assert_eq!(
        rig.storage.call_count(),
        0,
        "the reaper makes no provider call"
    );
}

#[tokio::test]
#[traced_test]
async fn the_secondary_copy_is_not_in_the_message_and_a_warning_names_it() {
    // inst-enq-reaper-secondary, inst-enq-reaper-warn: DESIGN B.9.5 limitation
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        stale(
            RowSeed::image()
                .status("uploaded")
                .secondary("file_ant_orphan", "uploaded"),
        ),
    )
    .await;
    scan(&rig).await;
    let payload = rig.attachment_cleanup_rows().await.remove(0).payload;
    assert!(payload["secondary_ref"].is_null(), "{payload}");
    assert_eq!(
        rig.row(chat.id, id).await.cleanup_status.as_deref(),
        Some("pending")
    );
    logs_assert(|lines: &[&str]| {
        if lines
            .iter()
            .any(|line| line.contains("WARN") && line.contains("file_ant_orphan"))
        {
            Ok(())
        } else {
            Err(format!("no WARN line names the secondary file: {lines:#?}"))
        }
    });
}

#[tokio::test]
async fn the_reaped_row_stays_visible_through_the_get_flow() {
    // inst-rp-limits: not soft-deleted, `failed` with `upload_abandoned`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    scan(&rig).await;
    let reply = rig.get_attachment(chat.id, id).await;
    assert_eq!(reply.status.as_u16(), 200, "{}", reply.text);
    let body = reply.json();
    assert_eq!(body["status"], "failed");
    assert_eq!(body["error_code"], "upload_abandoned");
}

// ── the batch and the order ──────────────────────────────────────────────────

#[tokio::test]
async fn one_scan_handles_at_most_one_hundred_rows() {
    // inst-rp-select: at most 100 rows (fixed); a later scan takes the rest
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for n in 0..130 {
        seed_row(&rig.db, &chat, uploaded().updated(ago(1_000 + n))).await;
    }
    let first = scan(&rig).await;
    assert_eq!((first.candidates, first.reaped), (100, 100));
    assert_eq!(REAPER_BATCH, 100);
    let second = scan(&rig).await;
    assert_eq!((second.candidates, second.reaped), (30, 30));
    let third = scan(&rig).await;
    assert_eq!(third.candidates, 0);
}

#[tokio::test]
async fn the_oldest_rows_are_reaped_first() {
    // inst-rp-select: oldest `updated_at` first
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let mut ids = Vec::new();
    for n in 0..101 {
        // the rows are inserted newest first, so the insert order is not the age order
        ids.push((
            n,
            seed_row(&rig.db, &chat, uploaded().updated(ago(5_000 - n))).await,
        ));
    }
    scan(&rig).await;
    let newest = ids.iter().max_by_key(|(n, _)| *n).unwrap().1;
    for (n, id) in &ids {
        let expected = if *n == 100 { "uploaded" } else { "failed" };
        assert_eq!(
            status_of(&rig, &chat, *id).await,
            expected,
            "row {n} ({id})"
        );
    }
    assert_eq!(status_of(&rig, &chat, newest).await, "uploaded");
}

#[tokio::test]
async fn timestamps_with_fractional_seconds_of_different_lengths_order_chronologically() {
    // inst-rp-select: "the comparison and order use the chronological timestamp key on SQLite,
    // never raw text comparison": `...:20.12Z` is older than `...:20.123Z` although its text
    // sorts after
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for n in 0..99 {
        seed_row(&rig.db, &chat, uploaded().updated(ago(2_000 - n))).await;
    }
    let base = ago(400);
    let older = seed_row(
        &rig.db,
        &chat,
        uploaded().updated(base + time::Duration::milliseconds(120)),
    )
    .await;
    let newer = seed_row(
        &rig.db,
        &chat,
        uploaded().updated(base + time::Duration::milliseconds(123)),
    )
    .await;
    let report = scan(&rig).await;
    assert_eq!(report.reaped, 100);
    assert_eq!(
        status_of(&rig, &chat, older).await,
        "failed",
        "the chronologically older row"
    );
    assert_eq!(
        status_of(&rig, &chat, newer).await,
        "uploaded",
        "the 101st row waits"
    );
}

#[tokio::test]
async fn the_cutoff_compares_chronologically_when_the_row_has_fractional_seconds() {
    // the cutoff text has no fraction (`...:25:00Z`) and a row 0.5 s after it has one; raw text
    // comparison puts `...:25:00.5Z` before `...:25:00Z`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let cutoff = ago(STALE);
    let after = seed_row(
        &rig.db,
        &chat,
        uploaded().updated(cutoff + time::Duration::milliseconds(500)),
    )
    .await;
    let before = seed_row(
        &rig.db,
        &chat,
        uploaded().updated(cutoff - time::Duration::milliseconds(500)),
    )
    .await;
    scan(&rig).await;
    assert_eq!(
        status_of(&rig, &chat, after).await,
        "uploaded",
        "0.5 s newer than the cutoff"
    );
    assert_eq!(
        status_of(&rig, &chat, before).await,
        "failed",
        "0.5 s older than the cutoff"
    );
}

// ── the guarded update ───────────────────────────────────────────────────────

/// A raw SQL statement built for the id of the racing row.
type Racer = Box<dyn Fn(Uuid) -> String>;

fn racing(rig: &AttachRig, statements: Vec<String>) -> Arc<RacingReaperStore> {
    Arc::new(RacingReaperStore {
        inner: rig.quota.db.provider.clone(),
        raw: rig.db.raw.clone(),
        after_select: Mutex::new(statements),
    })
}

#[tokio::test]
async fn a_row_that_changed_after_the_selection_is_skipped() {
    // inst-rp-cas, inst-rp-skip: the upload finished, or the row was deleted or claimed by chat
    // cleanup, or its `updated_at` was refreshed
    let cases: Vec<(&str, Racer)> = vec![
        (
            "finished",
            Box::new(|id| {
                format!(
                    "UPDATE attachments SET status = 'ready' WHERE id = {}",
                    blob(id)
                )
            }),
        ),
        (
            "deleted",
            Box::new(|id| {
                format!(
                    "UPDATE attachments SET deleted_at = '2026-02-28T15:29:00Z' WHERE id = {}",
                    blob(id)
                )
            }),
        ),
        (
            "claimed",
            Box::new(|id| {
                format!(
                    "UPDATE attachments SET cleanup_status = 'pending' WHERE id = {}",
                    blob(id)
                )
            }),
        ),
        (
            "refreshed",
            Box::new(|id| {
                format!(
                    "UPDATE attachments SET updated_at = '2026-02-28T15:29:59Z' WHERE id = {}",
                    blob(id)
                )
            }),
        ),
    ];
    for (name, statement) in cases {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
        let store = racing(&rig, vec![statement(id)]);
        let reaper = rig.reaper_with(reaper_defaults(), ToggleElector::leader(), Some(store));
        let report = reaper.scan_once().await.unwrap();
        assert_eq!(
            (
                report.candidates,
                report.reaped,
                report.skipped,
                report.failed
            ),
            (1, 0, 1, 0),
            "{name}"
        );
        let row = rig.row(chat.id, id).await;
        assert_ne!(
            row.error_code.as_deref(),
            Some("upload_abandoned"),
            "{name}"
        );
        assert!(
            rig.attachment_cleanup_rows().await.is_empty(),
            "{name}: no message"
        );
        assert!(rig.metrics.abandoned().is_empty(), "{name}: no metric");
        assert_eq!(rig.metrics.pending_total(), 0, "{name}");
    }
}

#[tokio::test]
async fn a_skipped_row_does_not_stop_the_scan() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let racy = seed_row(&rig.db, &chat, stale(uploaded()).updated(ago(900))).await;
    let calm = seed_row(&rig.db, &chat, stale(uploaded()).updated(ago(800))).await;
    let store = racing(
        &rig,
        vec![format!(
            "UPDATE attachments SET status = 'ready' WHERE id = {}",
            blob(racy)
        )],
    );
    let reaper = rig.reaper_with(reaper_defaults(), ToggleElector::leader(), Some(store));
    let report = reaper.scan_once().await.unwrap();
    assert_eq!((report.reaped, report.skipped), (1, 1));
    assert_eq!(status_of(&rig, &chat, racy).await, "ready");
    assert_eq!(status_of(&rig, &chat, calm).await, "failed");
}

// ── failures ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_failing_row_is_logged_and_the_next_row_is_handled_then_a_later_scan_retries_it() {
    // inst-rp-row-failed: the status update and the enqueue are one transaction; a failing enqueue
    // rolls both back, the other row (no provider file, no enqueue) is still reaped
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let with_file = seed_row(&rig.db, &chat, uploaded().updated(ago(900))).await;
    let without_file = seed_row(&rig.db, &chat, pending().updated(ago(800))).await;
    fail_outbox_inserts(&rig.db).await;
    let report = scan(&rig).await;
    assert_eq!(
        (report.candidates, report.reaped, report.failed),
        (2, 1, 1),
        "{report:?}"
    );
    let untouched = rig.row(chat.id, with_file).await;
    assert_eq!(untouched.status, "uploaded", "rolled back");
    assert_eq!(untouched.cleanup_status, None);
    assert_eq!(untouched.error_code, None);
    assert_eq!(status_of(&rig, &chat, without_file).await, "failed");

    crate::test_support::raw_exec(&rig.db.raw, "DROP TRIGGER fail_outbox_insert")
        .await
        .unwrap();
    let retry = scan(&rig).await;
    assert_eq!((retry.candidates, retry.reaped), (1, 1));
    assert_eq!(status_of(&rig, &chat, with_file).await, "failed");
    assert_eq!(rig.attachment_cleanup_rows().await.len(), 1);
}

#[tokio::test]
async fn a_failing_selection_fails_the_scan_without_reaping() {
    // the scan itself fails; no row is touched and no duration is recorded for a failed scan
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let reaper = rig.reaper_with(
        reaper_defaults(),
        ToggleElector::leader(),
        Some(Arc::new(FailingReaperStore)),
    );
    assert!(reaper.scan_once().await.is_err());
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
}

// ── the metrics ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_abandoned_metric_carries_the_previous_status() {
    // inst-rp-commit, inst-met-reaper: `mini_chat_attachment_upload_abandoned{from_status}`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_row(&rig.db, &chat, stale(pending())).await;
    seed_row(&rig.db, &chat, stale(uploaded())).await;
    seed_row(&rig.db, &chat, stale(uploaded())).await;
    scan(&rig).await;
    let mut seen = rig.metrics.abandoned();
    seen.sort();
    assert_eq!(seen, vec!["pending", "uploaded", "uploaded"]);
}

#[tokio::test]
async fn the_pending_gauge_is_decremented_only_for_a_row_that_was_pending() {
    // inst-rp-commit: "when the previous status was `pending`, decrement `mini_chat_attachments_pending`
    // because the row left `pending`"
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_row(&rig.db, &chat, stale(uploaded())).await;
    scan(&rig).await;
    assert_eq!(
        rig.metrics.pending_total(),
        0,
        "an uploaded row already left pending"
    );
    seed_row(&rig.db, &chat, stale(pending())).await;
    scan(&rig).await;
    assert_eq!(rig.metrics.pending_total(), -1);
}

#[tokio::test]
async fn the_scan_duration_is_recorded_once_per_scan_even_without_candidates() {
    // inst-rp-duration
    let rig = rig().await;
    scan(&rig).await;
    scan(&rig).await;
    assert_eq!(rig.metrics.reaper_scans().len(), 2);
}

// ── the worker loop ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_loop_scans_every_scan_interval_of_the_timer() {
    // inst-rp-loop: one scan every `upload_reaper.scan_interval_secs`
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let settings = ReaperSettings {
        scan_interval: Duration::from_secs(7),
        ..reaper_defaults()
    };
    let reaper = rig.reaper_with(settings, ToggleElector::leader(), None);
    let token = CancellationToken::new();
    let handle = tokio::spawn(reaper.run(token.clone()));
    rig.time.wait_for_parked(1).await;
    assert_eq!(rig.time.sleeps(), vec![Duration::from_secs(7)]);
    assert_eq!(
        status_of(&rig, &chat, id).await,
        "uploaded",
        "no scan before the interval"
    );
    rig.time.advance(Duration::from_secs(7));
    wait_for_status(&rig, &chat, id, "failed").await;
    rig.time.wait_for_parked(1).await;
    assert_eq!(
        rig.time.sleeps(),
        vec![Duration::from_secs(7), Duration::from_secs(7)]
    );
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the loop stops with the token")
        .unwrap();
}

#[tokio::test]
async fn the_loop_returns_when_the_gear_stops_without_another_scan() {
    // inst-rp-return: the gear cancellation token fired
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, stale(uploaded())).await;
    let reaper = rig.reaper();
    let token = CancellationToken::new();
    let handle = tokio::spawn(reaper.run(token.clone()));
    rig.time.wait_for_parked(1).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the loop stops with the token")
        .unwrap();
    assert_eq!(status_of(&rig, &chat, id).await, "uploaded");
    assert!(rig.metrics.reaper_scans().is_empty());
}

#[tokio::test]
async fn a_cancelled_token_ends_the_loop_before_the_first_wait() {
    let rig = rig().await;
    let token = CancellationToken::new();
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), rig.reaper().run(token))
        .await
        .expect("the loop returns at once");
    assert!(rig.metrics.reaper_scans().is_empty());
}

async fn wait_for_status(rig: &AttachRig, chat: &ChatRecord, id: Uuid, status: &str) {
    rig.wait_for_status(chat.id, id, status).await;
}

// ── the configuration ────────────────────────────────────────────────────────

#[test]
fn the_settings_read_the_upload_reaper_section() {
    // inst-cfg-workers: `enabled` (true), `scan_interval_secs` (60), `stale_after_secs` (300)
    let base = json!({ "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" } });
    let defaults = ReaperSettings::from_config(&MiniChatConfig::from_value(base.clone()).unwrap());
    assert_eq!(defaults, reaper_defaults());
    let mut custom = base;
    custom["upload_reaper"] = json!({
        "enabled": false, "scan_interval_secs": 5, "stale_after_secs": 120
    });
    let settings = ReaperSettings::from_config(&MiniChatConfig::from_value(custom).unwrap());
    assert_eq!(
        settings,
        ReaperSettings {
            enabled: false,
            scan_interval: Duration::from_secs(5),
            stale_after: Duration::from_secs(120),
        }
    );
}

#[test]
fn the_fixed_batch_is_not_configurable() {
    // inst-cfg-fixed: 100 rows per reaper scan
    assert_eq!(REAPER_BATCH, 100);
    let result = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "upload_reaper": { "batch_size": 5 }
    }));
    // unknown keys of the worker sections are accepted and ignored: the batch stays fixed
    assert_eq!(
        ReaperSettings::from_config(&result.unwrap()),
        reaper_defaults()
    );
}
