#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::Value;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{
    LeaderElector, NoopElector, OrphanWatchdog, UploadReaper, shutdown_workers, spawn_worker,
};
use crate::config::MiniChatConfig;
use crate::domain::background::Background;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyProvider;
use crate::domain::services::finalization_service::FinalizationService;
use crate::domain::services::quota_service::{PeriodStarts, QuotaService, ReserveRequest};
use crate::domain::time::db_ts;
use crate::infra::db::entities::{attachment, chat, chat_turn, quota_usage};
use crate::infra::outbox::{
    ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AUDIT_PAYLOAD_TYPE, MiniChatOutbox, USAGE_PAYLOAD_TYPE,
};
use crate::test_support::{
    FakeAuthz, FakePolicy, catalog_entry, ctx_for, insert_turn, seed_attachment, seed_chat,
    seed_turn, snapshot, test_file_db, test_outbox,
};

// Model `p` (premium): 1.5 / 3 credits per token (micro). Reserve: 1000
// estimated input + 2000 max output = 7500; estimated settlement with the
// floor of 100: 1500 + 300 = 1800.
const MODEL: &str = "p";
const RESERVED_CREDITS: i64 = 7500;
const ESTIMATED_CREDITS: i64 = 1800;
const ORPHAN_TIMEOUT: Duration = Duration::from_secs(300);
const STALE_AFTER: Duration = Duration::from_secs(300);

fn limits() -> mini_chat_sdk::TierLimits {
    mini_chat_sdk::TierLimits {
        limit_daily_credits_micro: 1_000_000_000,
        limit_monthly_credits_micro: 10_000_000_000,
    }
}

/// Elector that never leads.
struct Follower;

impl LeaderElector for Follower {
    fn is_leader(&self, _role: &str) -> bool {
        false
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<MiniChatOutbox>,
    rx: UnboundedReceiver<(String, Value)>,
    quota: Arc<QuotaService>,
    finalization: Arc<FinalizationService>,
    tenant: Uuid,
    user: Uuid,
}

async fn fx() -> Fx {
    let (dir, raw) = test_file_db().await;
    let (outbox, rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let policy: Arc<dyn PolicyProvider> = Arc::new(FakePolicy::with_limits(
        snapshot(vec![catalog_entry(MODEL, true)]),
        limits(),
        limits(),
    ));
    let quota = Arc::new(QuotaService::new(
        Arc::clone(&db),
        Arc::new(FakeAuthz::default()),
        Arc::clone(&policy),
        Arc::new(MiniChatConfig::default()),
    ));
    let finalization = Arc::new(FinalizationService::new(
        Arc::clone(&db),
        policy,
        Arc::clone(&quota),
        Arc::clone(&outbox),
    ));
    Fx {
        _dir: dir,
        db,
        outbox,
        rx,
        quota,
        finalization,
        tenant: Uuid::new_v4(),
        user: Uuid::new_v4(),
    }
}

impl Fx {
    fn watchdog(&self, elector: Arc<dyn LeaderElector>) -> OrphanWatchdog {
        OrphanWatchdog::new(
            Arc::clone(&self.db),
            Arc::clone(&self.finalization),
            elector,
            ORPHAN_TIMEOUT,
        )
    }

    fn reaper(&self, elector: Arc<dyn LeaderElector>) -> UploadReaper {
        UploadReaper::new(
            Arc::clone(&self.db),
            Arc::clone(&self.outbox),
            elector,
            STALE_AFTER,
        )
    }

    async fn chat(&self) -> chat::Model {
        seed_chat(
            &self.db,
            &ctx_for(self.tenant, self.user),
            None,
            db_ts(OffsetDateTime::now_utc()),
        )
        .await
    }

    /// A running turn with preflight columns and a booked reserve in the
    /// periods of `started_at`.
    async fn running_turn(
        &self,
        chat: &chat::Model,
        started_at: OffsetDateTime,
        last_progress: Option<OffsetDateTime>,
        deleted_at: Option<OffsetDateTime>,
    ) -> chat_turn::Model {
        let started_at = db_ts(started_at);
        let row = insert_turn(
            &self.db,
            chat_turn::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.tenant),
                chat_id: Set(chat.id),
                request_id: Set(Uuid::new_v4()),
                requester_type: Set("user".to_owned()),
                requester_user_id: Set(Some(self.user)),
                state: Set("running".to_owned()),
                provider_name: Set(None),
                provider_response_id: Set(None),
                assistant_message_id: Set(None),
                error_code: Set(None),
                reserve_tokens: Set(Some(3000)),
                max_output_tokens_applied: Set(Some(2000)),
                reserved_credits_micro: Set(Some(RESERVED_CREDITS)),
                policy_version_applied: Set(Some(1)),
                effective_model: Set(Some(MODEL.to_owned())),
                minimal_generation_floor_applied: Set(Some(100)),
                error_detail: Set(None),
                deleted_at: Set(deleted_at.map(db_ts)),
                replaced_by_request_id: Set(None),
                started_at: Set(started_at),
                last_progress_at: Set(last_progress.map(db_ts)),
                web_search_enabled: Set(false),
                web_search_completed_count: Set(0),
                code_interpreter_completed_count: Set(0),
                file_search_completed_count: Set(0),
                completed_at: Set(None),
                updated_at: Set(started_at),
            },
        )
        .await;
        let quota = Arc::clone(&self.quota);
        let req = ReserveRequest {
            tenant_id: self.tenant,
            user_id: self.user,
            premium: true,
            reserved_credits_micro: RESERVED_CREDITS,
            periods: PeriodStarts::at(started_at),
            limits: mini_chat_sdk::UserLimits {
                user_id: self.user,
                policy_version: 1,
                standard: limits(),
                premium: limits(),
            },
        };
        self.db
            .transaction(move |tx| Box::pin(async move { quota.reserve_in_tx(tx, &req).await }))
            .await
            .unwrap();
        row
    }

    async fn turn(&self, id: Uuid) -> chat_turn::Model {
        let conn = self.db.conn().unwrap();
        chat_turn::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
            .unwrap()
    }

    async fn quota_rows(&self) -> Vec<quota_usage::Model> {
        let conn = self.db.conn().unwrap();
        quota_usage::Entity::find()
            .filter(quota_usage::Column::UserId.eq(self.user))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
    }

    async fn attachment(&self, id: Uuid) -> attachment::Model {
        let conn = self.db.conn().unwrap();
        attachment::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
            .unwrap()
    }

    /// An attachment in `status` whose `updated_at` is `age` old.
    async fn upload(
        &self,
        chat: &chat::Model,
        status: &str,
        provider_file_id: Option<&str>,
        age: TimeDuration,
    ) -> attachment::Model {
        let a = seed_attachment(&self.db, chat, "document", status, None).await;
        self.set_attachment(
            a.id,
            attachment::Column::ProviderFileId,
            provider_file_id.map(str::to_owned),
        )
        .await;
        self.set_attachment(
            a.id,
            attachment::Column::UpdatedAt,
            db_ts(OffsetDateTime::now_utc() - age),
        )
        .await;
        self.attachment(a.id).await
    }

    async fn set_attachment<V>(&self, id: Uuid, col: attachment::Column, v: V)
    where
        V: Into<sea_orm::Value>,
    {
        let conn = self.db.conn().unwrap();
        attachment::Entity::update_many()
            .col_expr(col, Expr::value(v))
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    async fn delivered(&mut self, n: usize) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while out.len() < n {
            let msg = tokio::time::timeout(Duration::from_secs(20), self.rx.recv())
                .await
                .expect("outbox message delivered in time")
                .unwrap();
            out.push(msg);
        }
        self.assert_no_more().await;
        out
    }

    async fn assert_no_more(&mut self) {
        if let Ok(Some(extra)) =
            tokio::time::timeout(Duration::from_millis(1500), self.rx.recv()).await
        {
            panic!("unexpected outbox message {extra:?}");
        }
    }
}

fn one_of(msgs: &[(String, Value)], payload_type: &str) -> Value {
    let found: Vec<_> = msgs.iter().filter(|(t, _)| t == payload_type).collect();
    assert_eq!(found.len(), 1, "{payload_type} in {msgs:?}");
    found[0].1.clone()
}

// ── Orphan watchdog ──────────────────────────────────────────────────────────

#[tokio::test]
async fn watchdog_finalizes_stale_running_turn() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let now = OffsetDateTime::now_utc();
    // started in an earlier month: the settlement must hit the reserve's
    // periods (derived from started_at), not today's
    let started_at = now - TimeDuration::days(40);
    let turn = f
        .running_turn(
            &chat,
            started_at,
            Some(now - TimeDuration::minutes(10)),
            None,
        )
        .await;

    let n = f
        .watchdog(Arc::new(NoopElector))
        .scan_once(now)
        .await
        .unwrap();

    assert_eq!(n, 1);
    let row = f.turn(turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("orphan_timeout"));
    assert!(row.completed_at.is_some());
    let periods = PeriodStarts::at(started_at);
    let rows = f.quota_rows().await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    for r in rows {
        let expected_start = if r.period_type == "daily" {
            periods.daily
        } else {
            periods.monthly
        };
        assert_eq!(r.period_start, expected_start, "{r:?}");
        assert_eq!(
            (r.spent_credits_micro, r.reserved_credits_micro),
            (ESTIMATED_CREDITS, 0),
            "{r:?}"
        );
    }
    let out = f.delivered(2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["actual_credits_micro"], ESTIMATED_CREDITS);
    assert_eq!(
        one_of(&out, AUDIT_PAYLOAD_TYPE)["event_type"],
        "turn_failed"
    );

    // the next scan has nothing left to do
    assert_eq!(
        f.watchdog(Arc::new(NoopElector))
            .scan_once(now)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn watchdog_uses_started_at_when_no_progress_recorded() {
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    let old_chat = f.chat().await;
    let old = f
        .running_turn(&old_chat, now - TimeDuration::minutes(10), None, None)
        .await;
    let new_chat = f.chat().await;
    let new = f
        .running_turn(&new_chat, now - TimeDuration::minutes(1), None, None)
        .await;

    let n = f
        .watchdog(Arc::new(NoopElector))
        .scan_once(now)
        .await
        .unwrap();

    assert_eq!(n, 1);
    assert_eq!(f.turn(old.id).await.state, "failed");
    assert_eq!(f.turn(new.id).await.state, "running");
    f.delivered(2).await;
}

#[tokio::test]
async fn watchdog_skips_recent_progress() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let now = OffsetDateTime::now_utc();
    // a long-running turn that keeps making progress is not an orphan
    let turn = f
        .running_turn(
            &chat,
            now - TimeDuration::hours(1),
            Some(now - TimeDuration::minutes(1)),
            None,
        )
        .await;

    let n = f
        .watchdog(Arc::new(NoopElector))
        .scan_once(now)
        .await
        .unwrap();

    assert_eq!(n, 0);
    assert_eq!(f.turn(turn.id).await.state, "running");
    f.assert_no_more().await;
}

#[tokio::test]
async fn watchdog_skips_deleted_turn() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let now = OffsetDateTime::now_utc();
    let turn = f
        .running_turn(
            &chat,
            now - TimeDuration::hours(1),
            Some(now - TimeDuration::minutes(10)),
            Some(now - TimeDuration::minutes(5)),
        )
        .await;

    let n = f
        .watchdog(Arc::new(NoopElector))
        .scan_once(now)
        .await
        .unwrap();

    assert_eq!(n, 0);
    assert_eq!(f.turn(turn.id).await.state, "running");
    f.assert_no_more().await;
}

#[tokio::test]
async fn watchdog_does_nothing_when_not_leader() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let now = OffsetDateTime::now_utc();
    let turn = f
        .running_turn(&chat, now - TimeDuration::hours(1), None, None)
        .await;

    let n = f.watchdog(Arc::new(Follower)).scan_once(now).await.unwrap();

    assert_eq!(n, 0);
    assert_eq!(f.turn(turn.id).await.state, "running");
    f.assert_no_more().await;
}

#[tokio::test]
async fn watchdog_takes_at_most_100_turns_per_scan() {
    let f = fx().await;
    let now = OffsetDateTime::now_utc();
    // one running turn per chat (one-running-turn-per-chat index); no
    // reserve fields, so the finalization skips settlement
    for _ in 0..101 {
        let chat = f.chat().await;
        seed_turn(
            &f.db,
            &chat,
            Uuid::new_v4(),
            "running",
            db_ts(now - TimeDuration::hours(1)),
        )
        .await;
    }
    let w = f.watchdog(Arc::new(NoopElector));

    assert_eq!(w.scan_once(now).await.unwrap(), 100);
    assert_eq!(w.scan_once(now).await.unwrap(), 1);
    assert_eq!(w.scan_once(now).await.unwrap(), 0);
}

// ── Upload reaper ────────────────────────────────────────────────────────────

#[tokio::test]
async fn reaper_fails_stale_pending() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let a = f
        .upload(&chat, "pending", None, TimeDuration::minutes(10))
        .await;

    let n = f
        .reaper(Arc::new(NoopElector))
        .scan_once(OffsetDateTime::now_utc())
        .await
        .unwrap();

    assert_eq!(n, 1);
    let row = f.attachment(a.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert!(row.updated_at > a.updated_at);
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.deleted_at, None, "the row stays visible");
    // no provider file: nothing to clean up
    f.assert_no_more().await;
}

#[tokio::test]
async fn reaper_enqueues_cleanup_when_provider_file_present() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let a = f
        .upload(
            &chat,
            "uploaded",
            Some("file-abandoned"),
            TimeDuration::minutes(10),
        )
        .await;

    let n = f
        .reaper(Arc::new(NoopElector))
        .scan_once(OffsetDateTime::now_utc())
        .await
        .unwrap();

    assert_eq!(n, 1);
    let row = f.attachment(a.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.cleanup_updated_at.is_some());
    let out = f.delivered(1).await;
    let p = one_of(&out, ATTACHMENT_CLEANUP_PAYLOAD_TYPE);
    assert_eq!(p["event_type"], "attachment_upload_abandoned");
    assert_eq!(p["tenant_id"], f.tenant.to_string());
    assert_eq!(p["chat_id"], chat.id.to_string());
    assert_eq!(p["attachment_id"], a.id.to_string());
    assert_eq!(p["provider_file_id"], "file-abandoned");
    assert_eq!(p["storage_backend"], "openai");
    assert_eq!(p["attachment_kind"], "document");
    assert_eq!(p["vector_store_id"], Value::Null);
    assert_eq!(p["secondary_ref"], Value::Null);
}

#[tokio::test]
async fn reaper_skips_rows_owned_by_chat_cleanup() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let a = f
        .upload(&chat, "uploaded", Some("file-x"), TimeDuration::minutes(10))
        .await;
    f.set_attachment(
        a.id,
        attachment::Column::CleanupStatus,
        Some("pending".to_owned()),
    )
    .await;

    let n = f
        .reaper(Arc::new(NoopElector))
        .scan_once(OffsetDateTime::now_utc())
        .await
        .unwrap();

    assert_eq!(n, 0);
    let row = f.attachment(a.id).await;
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.error_code, None);
    f.assert_no_more().await;
}

#[tokio::test]
async fn reaper_skips_recent_deleted_and_settled_rows() {
    let mut f = fx().await;
    let chat = f.chat().await;
    let recent = f
        .upload(&chat, "pending", None, TimeDuration::seconds(30))
        .await;
    let ready = f
        .upload(&chat, "ready", Some("file-r"), TimeDuration::minutes(10))
        .await;
    let deleted = f
        .upload(&chat, "pending", None, TimeDuration::minutes(10))
        .await;
    f.set_attachment(
        deleted.id,
        attachment::Column::DeletedAt,
        Some(db_ts(OffsetDateTime::now_utc())),
    )
    .await;

    let n = f
        .reaper(Arc::new(NoopElector))
        .scan_once(OffsetDateTime::now_utc())
        .await
        .unwrap();

    assert_eq!(n, 0);
    assert_eq!(f.attachment(recent.id).await.status, "pending");
    assert_eq!(f.attachment(ready.id).await.status, "ready");
    assert_eq!(f.attachment(deleted.id).await.status, "pending");
    f.assert_no_more().await;
}

#[tokio::test]
async fn reaper_does_nothing_when_not_leader() {
    let f = fx().await;
    let chat = f.chat().await;
    let a = f
        .upload(&chat, "pending", None, TimeDuration::minutes(10))
        .await;

    let n = f
        .reaper(Arc::new(Follower))
        .scan_once(OffsetDateTime::now_utc())
        .await
        .unwrap();

    assert_eq!(n, 0);
    assert_eq!(f.attachment(a.id).await.status, "pending");
}

// ── Scan loop and shutdown ───────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn spawn_worker_scans_every_interval_until_cancelled() {
    let calls = Arc::new(AtomicU32::new(0));
    let cancel = CancellationToken::new();
    let mut set = JoinSet::new();
    let c = Arc::clone(&calls);
    spawn_worker(
        &mut set,
        "test",
        Duration::from_secs(10),
        cancel.clone(),
        move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
        },
    );

    // first scan at start, then one per interval
    tokio::time::sleep(Duration::from_secs(25)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), set.join_next())
        .await
        .expect("worker stops on cancel")
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn spawn_worker_keeps_scanning_after_errors() {
    let calls = Arc::new(AtomicU32::new(0));
    let cancel = CancellationToken::new();
    let mut set = JoinSet::new();
    let c = Arc::clone(&calls);
    spawn_worker(
        &mut set,
        "test",
        Duration::from_secs(1),
        cancel.clone(),
        move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(DomainError::Internal("db down".to_owned()))
            }
        },
    );

    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    cancel.cancel();
    set.join_next().await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_stays_within_budget_when_tasks_hang() {
    // Carried from task 15: the lifecycle stop_timeout is 30 s and
    // outbox.stop() must still run after the worker and background waits.
    let mut set: JoinSet<()> = JoinSet::new();
    set.spawn(std::future::pending());
    let background = Background::default();
    background.tracker.spawn(std::future::pending::<()>());
    let budget = Duration::from_secs(25);

    let started = tokio::time::Instant::now();
    shutdown_workers(&mut set, &background, budget).await;

    assert!(
        started.elapsed() <= budget + Duration::from_millis(100),
        "shutdown took {:?}",
        started.elapsed()
    );
    let aborted = set.join_next().await.unwrap().unwrap_err();
    assert!(aborted.is_cancelled(), "hung workers are aborted");
    assert!(background.cancel.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn shutdown_returns_as_soon_as_everything_stopped() {
    let cancel = CancellationToken::new();
    let mut set: JoinSet<()> = JoinSet::new();
    let c = cancel.clone();
    set.spawn(async move { c.cancelled().await });
    cancel.cancel();
    let background = Background::default();
    let bg = background.cancel.clone();
    background
        .tracker
        .spawn(async move { bg.cancelled().await });

    let started = tokio::time::Instant::now();
    shutdown_workers(&mut set, &background, Duration::from_secs(25)).await;

    assert!(started.elapsed() < Duration::from_secs(1));
}

// ── Kubernetes Lease elector (feature `k8s`) ─────────────────────────────────

#[cfg(feature = "k8s")]
mod lease {
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use k8s_openapi::jiff::{SignedDuration, Timestamp};

    use super::super::ROLE_UPLOAD_REAPER;
    use super::super::leader::k8s::{LeaseAction, decide, lease_name};

    const ME: &str = "pod-a";

    fn lease(holder: Option<&str>, renewed_secs_ago: Option<i64>, now: Timestamp) -> Lease {
        Lease {
            spec: Some(LeaseSpec {
                holder_identity: holder.map(str::to_owned),
                lease_duration_seconds: Some(15),
                renew_time: renewed_secs_ago.map(|s| MicroTime(now - SignedDuration::from_secs(s))),
                ..LeaseSpec::default()
            }),
            ..Lease::default()
        }
    }

    #[test]
    fn missing_lease_is_created() {
        assert_eq!(decide(None, ME, Timestamp::now()), LeaseAction::Create);
    }

    #[test]
    fn own_lease_is_renewed() {
        let now = Timestamp::now();
        // even when this pod's renewals lapsed, it renews its own lease
        assert_eq!(
            decide(Some(&lease(Some(ME), Some(60), now)), ME, now),
            LeaseAction::Renew
        );
    }

    #[test]
    fn live_foreign_lease_is_followed() {
        let now = Timestamp::now();
        assert_eq!(
            decide(Some(&lease(Some("pod-b"), Some(5), now)), ME, now),
            LeaseAction::Follow
        );
    }

    #[test]
    fn expired_foreign_lease_is_acquired() {
        let now = Timestamp::now();
        assert_eq!(
            decide(Some(&lease(Some("pod-b"), Some(16), now)), ME, now),
            LeaseAction::Acquire
        );
        // a holder that never renewed holds nothing
        assert_eq!(
            decide(Some(&lease(Some("pod-b"), None, now)), ME, now),
            LeaseAction::Acquire
        );
    }

    #[test]
    fn released_lease_is_acquired() {
        let now = Timestamp::now();
        assert_eq!(
            decide(Some(&lease(None, Some(1), now)), ME, now),
            LeaseAction::Acquire
        );
        assert_eq!(
            decide(Some(&lease(Some(""), Some(1), now)), ME, now),
            LeaseAction::Acquire
        );
        assert_eq!(
            decide(Some(&Lease::default()), ME, now),
            LeaseAction::Acquire
        );
    }

    #[test]
    fn lease_names_use_the_hardcoded_prefix() {
        assert_eq!(lease_name(ROLE_UPLOAD_REAPER), "mini-chat-upload-reaper");
    }
}
