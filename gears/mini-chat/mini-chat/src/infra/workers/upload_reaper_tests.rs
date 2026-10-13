#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the abandoned-upload reaper
//! (`cpt-cf-mini-chat-algo-attachments-reap-abandoned`,
//! `cpt-cf-mini-chat-dod-attachments-upload-reaper`): leader gating, the cutoff,
//! the per-row CAS transactions, cleanup messages, metrics and the loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use uuid::Uuid;

use super::{ReaperDeps, SCAN_BATCH, UPLOAD_REAPER_LEASE, UPLOAD_REAPER_TASK, UploadReaper};
use crate::attachment_test_env::{AttEnv, AttOptions};
use crate::chat_test_env::blob;
use crate::config::UploadReaperConfig;
use crate::domain::service::attachments::model::AttachmentStatus;
use crate::domain::service::attachments::ports::{AttachmentMetrics, AttachmentRepository};
use crate::domain::service::attachments::test_support::{
    FakeRepo, Metric, RecordingAttachmentMetrics, stored, t0,
};
use crate::infra::workers::leader::{LeaderElector, LeaderRole, NoopLeaderElector};
use crate::test_doubles::{LogCapture, USER_TENANT};

/// An elector whose leadership a test switches.
struct Toggle(AtomicBool);

impl Toggle {
    fn new(leader: bool) -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(leader)))
    }

    fn set(&self, leader: bool) {
        self.0.store(leader, Ordering::SeqCst);
    }
}

impl LeaderElector for Toggle {
    fn is_leader(&self, _role: LeaderRole) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn name(&self) -> &'static str {
        "toggle"
    }
}

fn leader() -> Arc<dyn LeaderElector> {
    Arc::new(NoopLeaderElector)
}

/// `updated_at` text `secs` before the fixture clock.
fn before_now(env: &AttEnv, secs: i64) -> String {
    AttEnv::stamp(env.now() - time::Duration::seconds(secs))
}

async fn stale_uploaded(env: &AttEnv, chat: Uuid, n: u128, age: i64) -> Uuid {
    let updated = before_now(env, age);
    env.seed(chat, n, |row| {
        row.set("status", "uploaded").set("updated_at", updated)
    })
    .await
}

async fn stale_pending(env: &AttEnv, chat: Uuid, n: u128, age: i64) -> Uuid {
    let updated = before_now(env, age);
    env.seed(chat, n, |row| {
        row.set("status", "pending")
            .null("provider_file_id")
            .set("updated_at", updated)
    })
    .await
}

/// An elector that leads the roles in its set only.
struct Roles(Vec<LeaderRole>);

impl LeaderElector for Roles {
    fn is_leader(&self, role: LeaderRole) -> bool {
        self.0.contains(&role)
    }

    fn name(&self) -> &'static str {
        "roles"
    }
}

#[tokio::test]
async fn the_reaper_scans_only_while_leader_of_its_own_role_whatever_the_watchdog_role_says() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    stale_uploaded(&env, chat, 1, 400).await;

    let watchdog_only: Arc<dyn LeaderElector> = Arc::new(Roles(vec![LeaderRole::OrphanWatchdog]));
    assert_eq!(
        env.reaper(watchdog_only).tick().await,
        None,
        "leading the watchdog role is not leading the reaper role"
    );

    let reaper_only: Arc<dyn LeaderElector> = Arc::new(Roles(vec![LeaderRole::UploadReaper]));
    let report = env.reaper(reaper_only).tick().await.expect("a scan");
    assert_eq!((report.selected, report.abandoned), (1, 1));
}

#[test]
fn the_lease_and_the_task_have_their_documented_names() {
    assert_eq!(UPLOAD_REAPER_LEASE, "mini-chat-upload-reaper");
    assert_eq!(UPLOAD_REAPER_TASK, "upload-reaper");
    assert_eq!(SCAN_BATCH, 100);
}

// ================================================================================================
// the scan
// ================================================================================================

#[tokio::test]
async fn stale_rows_fail_as_abandoned_and_only_one_with_a_file_gets_a_cleanup_message() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let pending = stale_pending(&env, chat, 1, 400).await;
    let uploaded = stale_uploaded(&env, chat, 2, 400).await;

    let report = env.reaper(leader()).scan_once().await;

    let snap_pending = env.att(pending).await;
    let snap_uploaded = env.att(uploaded).await;
    assert_eq!((report.selected, report.abandoned), (2, 2));
    for snap in [&snap_pending, &snap_uploaded] {
        assert_eq!(snap.status, "failed");
        assert_eq!(snap.error_code.as_deref(), Some("upload_abandoned"));
        assert_eq!(snap.deleted_at, None, "the row is never soft-deleted");
        assert_eq!(snap.updated_at, AttEnv::stamp(env.now()));
    }
    assert_eq!(
        snap_pending.cleanup_status, None,
        "no provider file, nothing to delete"
    );
    assert_eq!(snap_uploaded.cleanup_status.as_deref(), Some("pending"));
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0],
        json!({
            "event_type": "attachment_upload_abandoned",
            "tenant_id": USER_TENANT.to_string(),
            "chat_id": chat.to_string(),
            "attachment_id": uploaded.to_string(),
            "provider_file_id": "file-prov2",
            "vector_store_id": null,
            "storage_backend": "openai",
            "attachment_kind": "document",
            "deleted_at": AttEnv::stamp(env.now()),
            "secondary_ref": null,
        })
    );
}

#[tokio::test]
async fn the_cutoff_is_stale_after_secs_before_now_and_strict() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    assert_eq!(env.config.upload_reaper.stale_after_secs, 300);
    let stale = stale_uploaded(&env, chat, 1, 301).await;
    let at_cutoff = stale_uploaded(&env, chat, 2, 300).await;
    let fresh = stale_uploaded(&env, chat, 3, 299).await;
    let refreshed_by_background_task = stale_uploaded(&env, chat, 4, 20).await;

    let report = env.reaper(leader()).scan_once().await;

    assert_eq!((report.selected, report.abandoned), (1, 1));
    assert_eq!(env.att(stale).await.status, "failed");
    for id in [at_cutoff, fresh, refreshed_by_background_task] {
        assert_eq!(env.att(id).await.status, "uploaded", "{id}");
    }
}

#[tokio::test]
async fn rows_that_are_not_stale_uploads_are_never_touched() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let old = before_now(&env, 900);
    let ready = env
        .seed(chat, 1, |row| row.set("updated_at", old.clone()))
        .await;
    let failed = env
        .seed(chat, 2, |row| {
            row.set("status", "failed").set("updated_at", old.clone())
        })
        .await;
    let deleted = env
        .seed(chat, 3, |row| {
            row.set("status", "uploaded")
                .set("updated_at", old.clone())
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await;
    let mut owned = Vec::new();
    for (n, state) in [(4, "pending"), (5, "done"), (6, "failed")] {
        owned.push(
            env.seed(chat, n, |row| {
                row.set("status", "uploaded")
                    .set("updated_at", old.clone())
                    .set("cleanup_status", state)
            })
            .await,
        );
    }
    let stale = stale_uploaded(&env, chat, 9, 900).await;
    let untouchable: Vec<Uuid> = [ready, failed, deleted]
        .into_iter()
        .chain(owned.iter().copied())
        .collect();
    let mut before = Vec::new();
    for id in &untouchable {
        before.push(env.att(*id).await);
    }

    let report = env.reaper(leader()).scan_once().await;

    assert_eq!(
        (report.selected, report.abandoned),
        (1, 1),
        "only the stale upload is taken"
    );
    assert_eq!(env.att(stale).await.status, "failed");
    for (id, snap) in untouchable.iter().zip(before) {
        assert_eq!(env.att(*id).await, snap, "{id} is untouched");
    }
    assert_eq!(
        env.cleanup_messages().await.len(),
        1,
        "one message, for the stale upload"
    );
}

#[tokio::test]
async fn a_scan_takes_at_most_100_rows_oldest_first_and_later_scans_take_the_rest() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let mut ids = Vec::new();
    for n in 0..105_u128 {
        // Row n is (1000 - n) seconds old: row 0 is the oldest.
        ids.push(stale_uploaded(&env, chat, n + 1, 1000 - i64::try_from(n).unwrap()).await);
    }
    let reaper = env.reaper(leader());

    let first = reaper.scan_once().await;
    let after_first = env
        .stream
        .quota
        .db
        .query_i64("SELECT COUNT(*) FROM attachments WHERE status = 'failed'")
        .await;
    let newest_still_uploaded = env.att(ids[104]).await.status;
    let second = reaper.scan_once().await;

    assert_eq!((first.selected, first.abandoned), (100, 100));
    assert_eq!(after_first, 100);
    assert_eq!(
        newest_still_uploaded, "uploaded",
        "the newest rows wait for the next scan"
    );
    assert_eq!((second.selected, second.abandoned), (5, 5));
    assert_eq!(env.cleanup_messages().await.len(), 105);
}

#[tokio::test]
async fn reaped_rows_stay_visible_as_failed_and_free_their_slots() {
    let env = AttEnv::with(AttOptions {
        rag: crate::config::RagConfig {
            max_documents_per_chat: 1,
            ..crate::config::RagConfig::default()
        },
        ..AttOptions::default()
    })
    .await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let id = stale_uploaded(&env, chat, 1, 400).await;
    let blocked = env.upload(chat, "b.pdf", "application/pdf", b"%PDF").await;

    env.reaper(leader()).scan_once().await;
    let got = env.get_attachment(chat, id).await;
    let accepted = env.upload(chat, "b.pdf", "application/pdf", b"%PDF").await;

    assert_eq!(blocked.status.as_u16(), 429, "{}", blocked.text);
    assert_eq!(got.status.as_u16(), 200, "{}", got.text);
    assert_eq!(got.json["status"], "failed");
    assert_eq!(got.json["error_code"], "upload_abandoned");
    assert_eq!(accepted.status.as_u16(), 201, "{}", accepted.text);
}

// ================================================================================================
// races and failures
// ================================================================================================

#[tokio::test]
async fn a_row_refreshed_between_the_scan_and_its_cas_is_skipped() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let refreshed = stale_uploaded(&env, chat, 1, 400).await;
    let other = stale_uploaded(&env, chat, 2, 399).await;
    let sql = format!(
        "UPDATE attachments SET updated_at = '{}' WHERE id = {}",
        AttEnv::stamp(env.now()),
        blob(refreshed)
    );
    let hook_env = env.stream.quota.db.raw.clone();
    env.repo.after("stale_uploads", move || {
        let sql = sql.clone();
        let raw = hook_env.clone();
        Box::pin(async move {
            use sea_orm::ConnectionTrait;
            raw.execute_unprepared(&sql).await.unwrap();
        })
    });

    let report = env.reaper(leader()).scan_once().await;

    assert_eq!((report.selected, report.abandoned), (2, 1));
    assert_eq!(
        env.att(refreshed).await.status,
        "uploaded",
        "the heartbeat won"
    );
    assert_eq!(env.att(other).await.status, "failed");
    assert_eq!(env.cleanup_messages().await.len(), 1);
    assert_eq!(
        env.metrics
            .count(&Metric::Abandoned(AttachmentStatus::Uploaded)),
        1
    );
}

#[tokio::test]
async fn a_failing_row_transaction_is_logged_and_the_scan_continues() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let first = stale_uploaded(&env, chat, 1, 600).await;
    let broken = stale_uploaded(&env, chat, 2, 500).await;
    let last = stale_uploaded(&env, chat, 3, 400).await;
    env.fail_updates(
        "no_reap_broken",
        &format!(
            "NEW.error_code = 'upload_abandoned' AND NEW.id = {}",
            blob(broken)
        ),
    )
    .await;
    let logs = LogCapture::start();

    let report = env.reaper(leader()).scan_once().await;

    assert_eq!((report.selected, report.abandoned), (3, 2));
    assert_eq!(env.att(first).await.status, "failed");
    assert_eq!(
        env.att(broken).await.status,
        "uploaded",
        "the next scan retries it"
    );
    assert_eq!(env.att(last).await.status, "failed");
    assert_eq!(env.cleanup_messages().await.len(), 2);
    assert!(
        !logs
            .at_containing(Level::ERROR, &broken.to_string())
            .is_empty()
            || !logs
                .at_containing(Level::WARN, &broken.to_string())
                .is_empty(),
        "the failure is logged with the attachment id: {}",
        logs.all_text()
    );
}

#[tokio::test]
async fn a_secondary_copy_of_a_reaped_row_is_warned_about_and_not_deleted() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-an").await;
    let updated = before_now(&env, 400);
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded")
                .set("updated_at", updated)
                .set("attachment_kind", "image")
                .set("secondary_file_id", "file_ant_77")
                .set("secondary_status", "uploaded")
        })
        .await;
    let logs = LogCapture::start();

    env.reaper(leader()).scan_once().await;

    assert!(
        !logs.at_containing(Level::WARN, "file_ant_77").is_empty(),
        "a warning names the secondary file id: {}",
        logs.all_text()
    );
    assert_eq!(
        env.cleanup_messages().await[0]["secondary_ref"],
        json!(null)
    );
    assert_eq!(env.att(id).await.status, "failed");
}

// ================================================================================================
// metrics
// ================================================================================================

#[tokio::test]
async fn each_abandoned_row_is_counted_by_its_selected_status_and_each_scan_is_timed() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    stale_pending(&env, chat, 1, 400).await;
    stale_pending(&env, chat, 2, 400).await;
    stale_uploaded(&env, chat, 3, 400).await;
    let reaper = env.reaper(leader());

    reaper.scan_once().await;
    reaper.scan_once().await;

    assert_eq!(
        env.metrics
            .count(&Metric::Abandoned(AttachmentStatus::Pending)),
        2
    );
    assert_eq!(
        env.metrics
            .count(&Metric::Abandoned(AttachmentStatus::Uploaded)),
        1
    );
    assert_eq!(
        env.metrics.count(&Metric::ReaperScan),
        2,
        "one timing per scan, also an empty one"
    );
}

// ================================================================================================
// leadership
// ================================================================================================

#[tokio::test]
async fn a_replica_that_is_not_the_leader_scans_nothing() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = stale_uploaded(&env, chat, 1, 400).await;
    let elector = Toggle::new(false);
    let reaper = env.reaper(elector.clone());

    let skipped = reaper.tick().await;
    let status_after_skip = env.att(id).await.status;
    elector.set(true);
    let scanned = reaper.tick().await;

    assert!(skipped.is_none());
    assert_eq!(status_after_skip, "uploaded");
    assert_eq!(scanned.map(|r| r.abandoned), Some(1));
    assert_eq!(env.att(id).await.status, "failed");
    assert_eq!(
        env.metrics.count(&Metric::ReaperScan),
        1,
        "the skipped tick is not a scan"
    );
}

// ================================================================================================
// the loop (no database: paused time over the in-memory store)
// ================================================================================================

fn loop_reaper(
    repo: &Arc<FakeRepo>,
    elector: Arc<dyn LeaderElector>,
    metrics: &Arc<RecordingAttachmentMetrics>,
) -> Arc<UploadReaper> {
    Arc::new(UploadReaper::new(ReaperDeps {
        repo: Arc::clone(repo) as Arc<dyn AttachmentRepository>,
        elector,
        metrics: Arc::clone(metrics) as Arc<dyn AttachmentMetrics>,
        config: UploadReaperConfig {
            enabled: true,
            scan_interval_secs: 60,
            stale_after_secs: 300,
        },
        clock: Arc::new(|| t0() + time::Duration::hours(1)),
    }))
}

async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn the_loop_scans_once_per_interval_while_leader() {
    let repo = FakeRepo::new();
    repo.put(stored(|r| {
        r.status = AttachmentStatus::Uploaded;
        r.updated_at = t0();
    }));
    let metrics = RecordingAttachmentMetrics::new();
    let stop = CancellationToken::new();
    let task = tokio::spawn(loop_reaper(&repo, leader(), &metrics).run(stop.clone()));

    settle().await;
    assert_eq!(
        repo.calls("stale_uploads"),
        0,
        "the first scan comes after one interval"
    );
    tokio::time::advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(repo.calls("stale_uploads"), 0);
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(repo.calls("stale_uploads"), 1);
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(repo.calls("stale_uploads"), 2);

    stop.cancel();
    task.await.unwrap();
    assert_eq!(
        repo.only_row().status,
        AttachmentStatus::Failed,
        "the first scan reaped the row"
    );
}

#[tokio::test(start_paused = true)]
async fn the_loop_skips_a_tick_without_leadership_and_resumes_when_it_is_gained() {
    let repo = FakeRepo::new();
    let metrics = RecordingAttachmentMetrics::new();
    let elector = Toggle::new(false);
    let stop = CancellationToken::new();
    let task = tokio::spawn(loop_reaper(&repo, elector.clone(), &metrics).run(stop.clone()));

    tokio::time::advance(Duration::from_secs(130)).await;
    settle().await;
    assert_eq!(
        repo.calls("stale_uploads"),
        0,
        "a follower never reads the table"
    );
    elector.set(true);
    tokio::time::advance(Duration::from_secs(61)).await;
    settle().await;
    assert!(repo.calls("stale_uploads") >= 1);

    stop.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_loop_ends_promptly_when_the_stop_token_is_cancelled() {
    let repo = FakeRepo::new();
    let metrics = RecordingAttachmentMetrics::new();
    let stop = CancellationToken::new();
    let task = tokio::spawn(loop_reaper(&repo, leader(), &metrics).run(stop.clone()));
    settle().await;

    tokio::time::advance(Duration::from_secs(61)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(repo.calls("stale_uploads"), 2, "two intervals, two scans");

    stop.cancel();

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the loop ends without waiting for the interval")
        .unwrap();
    assert_eq!(repo.calls("stale_uploads"), 2);
}
