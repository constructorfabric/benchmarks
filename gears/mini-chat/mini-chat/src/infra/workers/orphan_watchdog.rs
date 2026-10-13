//! Orphan turn watchdog (DESIGN "Orphan Turn Watchdog", B.9.1, 5.7 "`FinalizeTurn` Invariant").
//!
//! Finalizes `running` turns whose progress went stale (the process that streamed them died)
//! as `failed` / `orphan_timeout` and settles their reserve on the estimated path.
//!
//! A turn must never stay `running` indefinitely (DESIGN 5.8), so a settlement that cannot be
//! computed degrades to releasing the reserve without a charge:
//! - the effective model is missing from the readable policy snapshot: released at once;
//! - the snapshot cannot be read: the candidate is deferred to the next scan while its last
//!   progress is younger than [`DEFER_TIMEOUTS`] x `timeout_secs`, released after that;
//! - `QuotaService::settle` fails: the finalization is retried as a release.
//!
//! Deferred rows cannot starve the scan: candidates are fetched stalest first and a row is
//! deferred only until it passes the bound above, so within that time every deferred row leaves
//! the head of the queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mini_chat_sdk::{ModelCatalogEntry, QuotaPolicyDecision};
use opentelemetry::KeyValue;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbTx};
use uuid::Uuid;

use super::leader::ROLE_ORPHAN_WATCHDOG;
use super::{LeaderElector, SCAN_BATCH, ScanGuard, run_scans};
use crate::api::state::AppServices;
use crate::domain::error::DomainError;
use crate::domain::quota::periods::period_starts;
use crate::domain::quota::{
    Bucket, QuotaMetricsFacts, QuotaService, SettleInput, SettlementMethod, derive_billing,
};
use crate::domain::stream::ToolCounts;
use crate::domain::stream::finalize::{EventFacts, EventSubject, audit_event, usage_event};
use crate::infra::db::TurnState;
use crate::infra::db::entity::chat_turns;
use crate::infra::db::repo::{quota_usage as quota_repo, turns};
use crate::infra::db::tx::write_tx_with_wakes;
use crate::infra::gateways::policy::PolicyGateway;
use crate::infra::outbox::{OutboxEnqueuer, OutboxRecord};
use crate::metrics::Metrics;

/// `reason` label of the orphan metrics.
const STALE_PROGRESS: &str = "stale_progress";
/// Quota decision of the audit event: the watchdog does not know the preflight decision.
const DECISION_UNKNOWN: &str = "unknown";
/// A candidate whose policy snapshot cannot be read is deferred while its last progress is
/// younger than this many orphan timeouts.
const DEFER_TIMEOUTS: u32 = 4;

/// How the reserve of a candidate is settled.
#[derive(Clone)]
enum Plan {
    /// Estimated settlement.
    Settle(SettleInput),
    /// Release the reserve without a charge. `premium_known` is `false` when the effective model
    /// could not be looked up: the tier is then inferred from the booked reserve.
    Release {
        input: SettleInput,
        premium_known: bool,
    },
    /// The row lacks its reserve fields or its user: no settlement.
    Skip,
}

/// Failure of the finalization transaction.
enum CommitError {
    /// `QuotaService::settle` failed; the transaction is rolled back.
    Settle(DomainError),
    Other(DomainError),
}

/// Result of looking up the effective model in the policy snapshot.
enum Lookup {
    Found(Box<ModelCatalogEntry>),
    /// The snapshot has no such model.
    Missing,
    /// The snapshot cannot be read.
    Unavailable,
}

/// The orphan watchdog; see the module docs.
pub struct OrphanWatchdog {
    db: Arc<DBProvider<DomainError>>,
    quota: Arc<QuotaService>,
    policy: Arc<dyn PolicyGateway>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<Metrics>,
    timeout: Duration,
}

impl OrphanWatchdog {
    #[must_use]
    pub fn new(services: &AppServices) -> Self {
        Self {
            db: Arc::clone(&services.db),
            quota: Arc::clone(&services.quota),
            policy: Arc::clone(&services.policy),
            outbox: Arc::clone(&services.outbox),
            metrics: Arc::clone(&services.metrics),
            timeout: Duration::from_secs(services.cfg.orphan_watchdog.timeout_secs),
        }
    }

    /// One scan at application time `now`; returns the number of turns finalized.
    ///
    /// A candidate that fails (for example a database error) is logged and left for the next
    /// scan; the others are still handled.
    ///
    /// # Errors
    /// `Internal` when the candidates cannot be read.
    pub async fn scan_once(&self, now: OffsetDateTime) -> Result<u32, DomainError> {
        self.scan(now, &ScanGuard::unrestricted()).await
    }

    /// [`Self::scan_once`] that stops between candidates when `guard` says so.
    async fn scan(&self, now: OffsetDateTime, guard: &ScanGuard) -> Result<u32, DomainError> {
        let started = Instant::now();
        let cutoff = now - self.timeout;
        let candidates = {
            let conn = self.db.conn()?;
            turns::orphan_candidates(&conn, &AccessScope::allow_all(), cutoff, SCAN_BATCH).await?
        };
        let mut finalized = 0;
        for row in &candidates {
            if !guard.proceed().await {
                break;
            }
            finalized += u32::from(self.detected(row, cutoff, now).await);
        }
        self.metrics
            .orphan_scan_duration_seconds
            .record(started.elapsed().as_secs_f64(), &[]);
        if finalized > 0 {
            tracing::info!(finalized, "orphan turns finalized");
        }
        Ok(finalized)
    }

    /// Counts and finalizes one candidate; a failure is logged. `true` when it was finalized.
    async fn detected(
        &self,
        row: &chat_turns::Model,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> bool {
        self.metrics
            .orphan_detected
            .add(1, &[KeyValue::new("reason", STALE_PROGRESS)]);
        match self.finalize(row, cutoff, now).await {
            Ok(finalized) => finalized,
            Err(err) => {
                tracing::warn!(turn_id = %row.id, error = %err, "orphan turn finalization failed");
                false
            }
        }
    }

    /// Scans every `interval` while `elector` says this process leads the watchdog role, until
    /// `cancel`.
    pub async fn run(
        self,
        elector: Arc<dyn LeaderElector>,
        interval: Duration,
        cancel: CancellationToken,
    ) {
        let watchdog = &self;
        run_scans(
            ROLE_ORPHAN_WATCHDOG,
            elector,
            interval,
            cancel,
            |guard| async move {
                if let Err(err) = watchdog.scan(OffsetDateTime::now_utc(), &guard).await {
                    tracing::warn!(error = %err, "orphan watchdog scan failed");
                }
            },
        )
        .await;
    }

    /// Finalizes one candidate in one transaction: the orphan CAS, the settlement and the usage
    /// and audit events. `false` when the CAS lost or the candidate was deferred.
    async fn finalize(
        &self,
        row: &chat_turns::Model,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let Some(plan) = self.plan(row, now).await else {
            return Ok(false);
        };
        let mut committed = self.commit(row, cutoff, now, &plan).await;
        if let (Err(CommitError::Settle(err)), Plan::Settle(input)) = (&committed, &plan) {
            tracing::error!(turn_id = %row.id, error = %err,
                "orphan settlement failed; finalizing with a release of the reserve");
            let release = Plan::Release {
                input: released(input),
                premium_known: true,
            };
            committed = self.commit(row, cutoff, now, &release).await;
        }
        let facts = match committed {
            Ok(Some(facts)) => facts,
            Ok(None) => return Ok(false),
            Err(CommitError::Settle(err) | CommitError::Other(err)) => return Err(err),
        };
        // Metrics only after the commit: the closure may have run several times.
        if let Some(facts) = facts {
            self.quota.record_facts(facts);
        }
        self.metrics
            .orphan_finalized
            .add(1, &[KeyValue::new("reason", STALE_PROGRESS)]);
        self.metrics
            .streams_aborted
            .add(1, &[KeyValue::new("trigger", turns::ORPHAN_TIMEOUT)]);
        Ok(true)
    }

    /// One finalization transaction. `Ok(None)`: the CAS lost; `Ok(Some(facts))`: committed, with
    /// the quota metrics of the settlement.
    async fn commit(
        &self,
        row: &chat_turns::Model,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        plan: &Plan,
    ) -> Result<Option<Option<QuotaMetricsFacts>>, CommitError> {
        let (billing, method) =
            derive_billing(TurnState::Failed, Some(turns::ORPHAN_TIMEOUT), None);
        let subject = subject_of(row, now);
        let counts = counts_of(row);
        let (quota, outbox) = (Arc::clone(&self.quota), Arc::clone(&self.outbox));
        let id = row.id;
        let scope = AccessScope::allow_all();
        let settle_failed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&settle_failed);

        // Runs again from the CAS on a retried attempt: no effects outside the transaction.
        let result = write_tx_with_wakes(&self.db, |tx, wakes| {
            let (quota, outbox, scope) = (Arc::clone(&quota), Arc::clone(&outbox), scope.clone());
            let (plan, subject, flag) = (plan.clone(), subject.clone(), Arc::clone(&flag));
            flag.store(false, Ordering::SeqCst);
            Box::pin(async move {
                if !turns::finalize_orphan(tx, &scope, id, cutoff, now).await? {
                    return Ok(None);
                }
                let input = match plan {
                    Plan::Skip => None,
                    Plan::Settle(input) => Some(input),
                    Plan::Release {
                        mut input,
                        premium_known,
                    } => {
                        if !premium_known {
                            input.is_premium = premium_reserved(tx, &input).await?;
                        }
                        Some(input)
                    }
                };
                let settled = match input {
                    Some(input) => Some(quota.settle(tx, input).await.inspect_err(|_| {
                        flag.store(true, Ordering::SeqCst);
                    })?),
                    None => None,
                };
                let facts = EventFacts {
                    state: TurnState::Failed,
                    error_code: Some(turns::ORPHAN_TIMEOUT.to_owned()),
                    billing,
                    method,
                    usage: None,
                    counts,
                    committed_credits_micro: settled.map_or(0, |s| s.committed_credits_micro),
                    now,
                };
                wakes.add(
                    outbox
                        .enqueue(tx, OutboxRecord::usage(&usage_event(&subject, &facts))?)
                        .await?,
                );
                wakes.add(
                    outbox
                        .enqueue(tx, OutboxRecord::audit(&audit_event(&subject, &facts))?)
                        .await?,
                );
                Ok(Some(settled.map(|s| s.metrics)))
            })
        })
        .await;
        result.map_err(|err| {
            if settle_failed.load(Ordering::SeqCst) {
                CommitError::Settle(err)
            } else {
                CommitError::Other(err)
            }
        })
    }

    /// How to settle `row`; `None` defers it to the next scan.
    async fn plan(&self, row: &chat_turns::Model, now: OffsetDateTime) -> Option<Plan> {
        let Some(columns) = Columns::of(row) else {
            tracing::warn!(
                turn_id = %row.id,
                "orphan turn without reserve fields or user: finalized without settlement"
            );
            return Some(Plan::Skip);
        };
        match self.lookup(row, &columns).await {
            Lookup::Found(model) => Some(Plan::Settle(columns.settle_input(row, &model))),
            Lookup::Missing => self.degrade(row, &columns, now, false),
            Lookup::Unavailable => self.degrade(row, &columns, now, true),
        }
    }

    /// The plan when the model is not at hand: defer (`None`) while the snapshot is merely
    /// unavailable and the row is within the deferral bound, release the reserve otherwise.
    fn degrade(
        &self,
        row: &chat_turns::Model,
        columns: &Columns<'_>,
        now: OffsetDateTime,
        unavailable: bool,
    ) -> Option<Plan> {
        if unavailable && self.may_defer(row, now) {
            tracing::warn!(turn_id = %row.id,
                "policy snapshot unavailable; orphan turn left for the next scan");
            return None;
        }
        tracing::error!(turn_id = %row.id, model = columns.model_id, version = columns.policy_version,
            unavailable, "no model in the policy snapshot; orphan turn finalized with a release of the reserve");
        Some(columns.release(row))
    }

    /// The row's last progress is younger than the deferral bound.
    fn may_defer(&self, row: &chat_turns::Model, now: OffsetDateTime) -> bool {
        let last = row.last_progress_at.unwrap_or(row.started_at);
        last > now - self.timeout * DEFER_TIMEOUTS
    }

    /// The effective model of `row` in the policy version it was admitted under.
    async fn lookup(&self, row: &chat_turns::Model, columns: &Columns<'_>) -> Lookup {
        let snapshot = match self
            .policy
            .snapshot(columns.user_id, columns.policy_version)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::warn!(turn_id = %row.id, error = %err, "policy snapshot read failed");
                return Lookup::Unavailable;
            }
        };
        snapshot
            .model_catalog
            .into_iter()
            .find(|m| m.id == columns.model_id)
            .map_or(Lookup::Missing, |m| Lookup::Found(Box::new(m)))
    }
}

/// The release variant of an estimated settlement: same turn, no charge.
fn released(input: &SettleInput) -> SettleInput {
    SettleInput {
        in_mult: 0,
        out_mult: 0,
        method: SettlementMethod::Released,
        usage: None,
        ..input.clone()
    }
}

/// Whether the turn reserved on the `tier:premium` rows: those rows then hold at least the turn's
/// reserve. A heuristic for a turn whose model cannot be looked up; a release larger than a
/// row's reserve is clamped at 0 by the quota repository.
async fn premium_reserved(tx: &DbTx<'_>, input: &SettleInput) -> Result<bool, DomainError> {
    let scope = quota_repo::owner_scope(input.tenant_id, input.user_id);
    let rows = quota_repo::load_current(tx, &scope, &input.periods, false).await?;
    Ok(rows.iter().any(|r| {
        r.bucket == Bucket::Premium.as_str()
            && r.reserved_credits_micro >= input.turn_reserved_credits_micro
    }))
}

/// The columns a settlement needs, all present.
struct Columns<'a> {
    user_id: Uuid,
    reserve_tokens: i64,
    max_output_tokens: i32,
    reserved_credits_micro: i64,
    policy_version: i64,
    model_id: &'a str,
    minimal_generation_floor: i32,
}

impl<'a> Columns<'a> {
    /// `None` when any of the columns (or the user) is NULL.
    fn of(row: &'a chat_turns::Model) -> Option<Self> {
        Some(Self {
            user_id: row.requester_user_id?,
            reserve_tokens: row.reserve_tokens?,
            max_output_tokens: row.max_output_tokens_applied?,
            reserved_credits_micro: row.reserved_credits_micro?,
            policy_version: row.policy_version_applied?,
            model_id: row.effective_model.as_deref()?,
            minimal_generation_floor: row.minimal_generation_floor_applied?,
        })
    }

    /// Estimated settlement of `row`: no provider usage was ever reported.
    fn settle_input(&self, row: &chat_turns::Model, model: &ModelCatalogEntry) -> SettleInput {
        let counts = counts_of(row);
        SettleInput {
            tenant_id: row.tenant_id,
            user_id: self.user_id,
            is_premium: model.is_premium(),
            periods: period_starts(row.started_at),
            turn_reserved_credits_micro: self.reserved_credits_micro,
            reserve_tokens: self.reserve_tokens,
            max_output_tokens_applied: i64::from(self.max_output_tokens),
            minimal_generation_floor_applied: i64::from(self.minimal_generation_floor),
            in_mult: model.input_tokens_credit_multiplier_micro,
            out_mult: model.output_tokens_credit_multiplier_micro,
            method: SettlementMethod::Estimated,
            usage: None,
            web_search_calls: i64::from(counts.web_search),
            code_interpreter_calls: i64::from(counts.code_interpreter),
        }
    }

    /// Release of the reserve of `row` for a model that cannot be looked up (tier unknown).
    fn release(&self, row: &chat_turns::Model) -> Plan {
        // The multipliers and the tier are placeholders: a release charges nothing and the tier
        // is inferred in the transaction.
        let model_free = SettleInput {
            tenant_id: row.tenant_id,
            user_id: self.user_id,
            is_premium: false,
            periods: period_starts(row.started_at),
            turn_reserved_credits_micro: self.reserved_credits_micro,
            reserve_tokens: self.reserve_tokens,
            max_output_tokens_applied: i64::from(self.max_output_tokens),
            minimal_generation_floor_applied: i64::from(self.minimal_generation_floor),
            in_mult: 0,
            out_mult: 0,
            method: SettlementMethod::Released,
            usage: None,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        };
        Plan::Release {
            input: model_free,
            premium_known: false,
        }
    }
}

/// Completed tool calls recorded on the row.
fn counts_of(row: &chat_turns::Model) -> ToolCounts {
    let count = |n: i32| u32::try_from(n).unwrap_or(0);
    ToolCounts {
        web_search: count(row.web_search_completed_count),
        code_interpreter: count(row.code_interpreter_completed_count),
        file_search: count(row.file_search_completed_count),
    }
}

/// The turn as the usage and audit events describe it. The selected model is not persisted on
/// the turn: both models are the effective one, empty when the reserve fields are NULL.
fn subject_of(row: &chat_turns::Model, now: OffsetDateTime) -> EventSubject {
    let model = row.effective_model.clone().unwrap_or_default();
    EventSubject {
        tenant_id: row.tenant_id,
        user_id: row.requester_user_id,
        chat_id: row.chat_id,
        turn_id: row.id,
        request_id: row.request_id,
        effective_model: model.clone(),
        selected_model: model,
        policy_version_applied: row.policy_version_applied.unwrap_or(0),
        quota: QuotaPolicyDecision {
            decision: DECISION_UNKNOWN.to_owned(),
            downgrade_from: None,
            downgrade_reason: None,
        },
        total_ms: u64::try_from((now - row.started_at).whole_milliseconds()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use mini_chat_sdk::credits_micro_checked;
    use serde_json::json;
    use time::Duration;
    use uuid::Uuid;

    use super::*;
    use crate::domain::quota::periods::period_starts;
    use crate::domain::quota::{Bucket, Period};
    use crate::infra::db::ts::db_now;
    use crate::test_support::app::{TestApp, ctx};
    use crate::test_support::catalog::test_catalog;
    use crate::test_support::stream::{
        AUDIT_QUEUE, USAGE_QUEUE, answer, create_chat, quota_rows, script_provider, seed_spent,
        stream_uri, turn_of,
    };
    use crate::test_support::workers::{
        SeedReserve, SeedTurn, seed_premium_reserve, seed_running_turn,
    };

    /// Credit multiplier of the test catalog's premium model (input and output).
    const PREMIUM_MULT: i64 = 3_000_000;
    const EST_INPUT: i64 = 100;
    const MAX_OUTPUT: i64 = 1_000;
    const FLOOR: i32 = 50;

    fn reserve() -> SeedReserve {
        SeedReserve {
            reserve_tokens: EST_INPUT + MAX_OUTPUT,
            max_output_tokens_applied: i32::try_from(MAX_OUTPUT).unwrap(),
            reserved_credits_micro: credits_micro_checked(
                EST_INPUT,
                MAX_OUTPUT,
                PREMIUM_MULT,
                PREMIUM_MULT,
            )
            .unwrap(),
            policy_version_applied: 1,
            effective_model: "gpt-premium",
            minimal_generation_floor_applied: FLOOR,
        }
    }

    fn settled_credits() -> i64 {
        credits_micro_checked(EST_INPUT, i64::from(FLOOR), PREMIUM_MULT, PREMIUM_MULT).unwrap()
    }

    struct Caller {
        tenant: Uuid,
        user: Uuid,
    }

    fn new_user() -> Caller {
        Caller {
            tenant: Uuid::new_v4(),
            user: Uuid::new_v4(),
        }
    }

    async fn wait_for_payloads(app: &TestApp, queue: &str, n: usize) {
        TestApp::wait_until(&format!("{n} payload(s) on {queue}"), || async {
            app.outbox_payloads(queue).len() >= n
        })
        .await;
    }

    /// A chat of `u` with a `running` turn (reserve columns and quota reserve) last seen
    /// `age_secs` ago; returns `(chat, turn id, request id)`.
    async fn stale_turn(app: &TestApp, u: &Caller, age_secs: i64) -> (Uuid, Uuid, Uuid) {
        stale_turn_with(app, u, age_secs, reserve()).await
    }

    /// [`stale_turn`] with the given reserve columns.
    async fn stale_turn_with(
        app: &TestApp,
        u: &Caller,
        age_secs: i64,
        reserve: SeedReserve,
    ) -> (Uuid, Uuid, Uuid) {
        let chat = create_chat(app, &ctx(u.tenant, u.user), None).await;
        let at = db_now() - Duration::seconds(age_secs);
        let credits = reserve.reserved_credits_micro;
        let (turn, request) = seed_running_turn(
            &app.db,
            &SeedTurn::reserved(u.tenant, chat, u.user, reserve, at),
        )
        .await;
        seed_premium_reserve(&app.db, u.tenant, u.user, at, credits).await;
        (chat, turn, request)
    }

    /// Every quota row of the user has its reserve released and nothing charged.
    async fn assert_released_without_charge(app: &TestApp, u: &Caller) {
        let rows = quota_rows(app, u.tenant, u.user).await;
        assert_eq!(rows.len(), 4, "{rows:?}");
        for row in &rows {
            assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
            assert_eq!(row.spent_credits_micro, 0, "{row:?}");
            assert_eq!(row.calls, 1, "{row:?}");
        }
    }

    /// The usage event of the turn carries no charge; returns it.
    async fn released_usage_event(app: &TestApp) -> serde_json::Value {
        wait_for_payloads(app, USAGE_QUEUE, 1).await;
        wait_for_payloads(app, AUDIT_QUEUE, 1).await;
        let usage = app.outbox_payloads(USAGE_QUEUE).remove(0);
        assert_eq!(usage["billing_outcome"], "aborted");
        assert_eq!(usage["actual_credits_micro"], 0);
        assert_eq!(usage["usage"], json!(null));
        assert_eq!(
            app.outbox_payloads(AUDIT_QUEUE)[0]["event_type"],
            "turn_failed"
        );
        usage
    }

    #[tokio::test]
    async fn watchdog_model_missing_finalizes_without_settlement() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let retired = SeedReserve {
            effective_model: "retired-model",
            ..reserve()
        };
        let (chat, _, request_id) = stale_turn_with(&app, &u, 400, retired).await;

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            1
        );

        let turn = turn_of(&app, chat, request_id).await;
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
        assert_released_without_charge(&app, &u).await;
        let usage = released_usage_event(&app).await;
        assert_eq!(usage["effective_model"], "retired-model");
        assert_eq!(usage["policy_version_applied"], 1);
    }

    #[tokio::test]
    async fn watchdog_snapshot_failure_past_the_bound_releases_the_reserve() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        // The default timeout is 300 s: the deferral bound is 4 x 300 s.
        let (chat, _, request_id) = stale_turn(&app, &u, 1_300).await;
        app.usage.fail_snapshots(true);

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            1
        );

        assert_eq!(turn_of(&app, chat, request_id).await.state, "failed");
        assert_released_without_charge(&app, &u).await;
        released_usage_event(&app).await;
    }

    #[tokio::test]
    async fn watchdog_settlement_error_falls_back_to_a_release() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let (chat, _, request_id) = stale_turn(&app, &u, 400).await;
        // A multiplier above the credits bound makes the estimated settlement fail.
        let mut catalog = test_catalog();
        for model in &mut catalog {
            model.input_tokens_credit_multiplier_micro = 20_000_000_000;
        }
        app.usage.set_catalog(catalog);

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            1
        );

        assert_eq!(turn_of(&app, chat, request_id).await.state, "failed");
        assert_released_without_charge(&app, &u).await;
        released_usage_event(&app).await;
    }

    #[tokio::test]
    async fn watchdog_finalizes_seeded_stale_turn() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let (chat, turn_id, request_id) = stale_turn(&app, &u, 400).await;
        let watchdog = OrphanWatchdog::new(&app.services);
        let now = db_now();

        assert_eq!(watchdog.scan_once(now).await.unwrap(), 1);

        let turn = turn_of(&app, chat, request_id).await;
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
        assert!(turn.completed_at.is_some());
        let starts = period_starts(turn.started_at);
        let rows = quota_rows(&app, u.tenant, u.user).await;
        assert_eq!(rows.len(), 4, "{rows:?}");
        for row in &rows {
            let period = if row.period_type == Period::Daily.as_str() {
                Period::Daily
            } else {
                Period::Monthly
            };
            assert_eq!(row.period_start, starts.start(period));
            assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
            assert_eq!(row.spent_credits_micro, settled_credits(), "{row:?}");
            assert_eq!(row.calls, 1, "{row:?}");
            assert!(row.bucket == Bucket::Total.as_str() || row.bucket == Bucket::Premium.as_str());
        }

        wait_for_payloads(&app, USAGE_QUEUE, 1).await;
        wait_for_payloads(&app, AUDIT_QUEUE, 1).await;
        let usage = &app.outbox_payloads(USAGE_QUEUE)[0];
        assert_eq!(usage["terminal_state"], "failed");
        assert_eq!(usage["billing_outcome"], "aborted");
        assert_eq!(usage["settlement_method"], "estimated");
        assert_eq!(usage["usage"], json!(null));
        assert_eq!(usage["actual_credits_micro"], settled_credits());
        assert_eq!(usage["effective_model"], "gpt-premium");
        assert_eq!(usage["selected_model"], "gpt-premium");
        assert_eq!(usage["policy_version_applied"], 1);
        assert_eq!(usage["user_id"], json!(u.user));
        assert_eq!(
            usage["dedupe_key"],
            format!(
                "{}/{}/{}",
                u.tenant.simple(),
                turn_id.simple(),
                request_id.simple()
            )
        );
        let audit = &app.outbox_payloads(AUDIT_QUEUE)[0];
        assert_eq!(audit["event_type"], "turn_failed");
        assert_eq!(audit["error_code"], "orphan_timeout");
        assert_eq!(audit["selected_model"], "gpt-premium");
        assert_eq!(audit["policy_decisions"]["quota"]["decision"], "unknown");

        // A second scan finds nothing and changes nothing.
        assert_eq!(watchdog.scan_once(db_now()).await.unwrap(), 0);
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert_eq!(app.outbox_payloads(USAGE_QUEUE).len(), 1);
        assert_eq!(app.outbox_payloads(AUDIT_QUEUE).len(), 1);
        assert_eq!(quota_rows(&app, u.tenant, u.user).await, rows);
    }

    #[tokio::test]
    async fn watchdog_uses_started_at_when_progress_null() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let chat = create_chat(&app, &ctx(u.tenant, u.user), None).await;
        let at = db_now() - Duration::seconds(400);
        let mut seed = SeedTurn::reserved(u.tenant, chat, u.user, reserve(), at);
        seed.last_progress_at = None;
        let (_, request_id) = seed_running_turn(&app.db, &seed).await;
        seed_premium_reserve(
            &app.db,
            u.tenant,
            u.user,
            at,
            reserve().reserved_credits_micro,
        )
        .await;

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            1
        );

        let turn = turn_of(&app, chat, request_id).await;
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    }

    #[tokio::test]
    async fn watchdog_skips_recent_progress() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let chat = create_chat(&app, &ctx(u.tenant, u.user), None).await;
        let started = db_now() - Duration::seconds(3_000);
        let mut seed = SeedTurn::reserved(u.tenant, chat, u.user, reserve(), started);
        seed.last_progress_at = Some(db_now() - Duration::seconds(10));
        let (_, request_id) = seed_running_turn(&app.db, &seed).await;
        seed_premium_reserve(
            &app.db,
            u.tenant,
            u.user,
            started,
            reserve().reserved_credits_micro,
        )
        .await;
        let before = quota_rows(&app, u.tenant, u.user).await;

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            0
        );

        let turn = turn_of(&app, chat, request_id).await;
        assert_eq!(turn.state, "running");
        assert!(turn.completed_at.is_none());
        assert_eq!(quota_rows(&app, u.tenant, u.user).await, before);
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert!(app.outbox_payloads(USAGE_QUEUE).is_empty());
    }

    #[tokio::test]
    async fn watchdog_null_reserve_skips_settlement() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let chat = create_chat(&app, &ctx(u.tenant, u.user), None).await;
        let at = db_now() - Duration::seconds(400);
        let mut seed = SeedTurn::reserved(u.tenant, chat, u.user, reserve(), at);
        seed.reserve = None;
        seed.web_search_completed_count = 2;
        let (_, request_id) = seed_running_turn(&app.db, &seed).await;
        seed_spent(&app, u.tenant, u.user, Period::Daily, Bucket::Total, 7).await;
        let before = quota_rows(&app, u.tenant, u.user).await;

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            1
        );

        let turn = turn_of(&app, chat, request_id).await;
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
        assert_eq!(quota_rows(&app, u.tenant, u.user).await, before);
        wait_for_payloads(&app, USAGE_QUEUE, 1).await;
        wait_for_payloads(&app, AUDIT_QUEUE, 1).await;
        let usage = &app.outbox_payloads(USAGE_QUEUE)[0];
        assert_eq!(usage["billing_outcome"], "aborted");
        assert_eq!(usage["settlement_method"], "estimated");
        assert_eq!(usage["actual_credits_micro"], 0);
        assert_eq!(usage["usage"], json!(null));
        assert_eq!(usage["effective_model"], "");
        assert_eq!(usage["selected_model"], "");
        assert_eq!(usage["policy_version_applied"], 0);
        assert_eq!(usage["web_search_calls"], 2);
        assert_eq!(
            app.outbox_payloads(AUDIT_QUEUE)[0]["event_type"],
            "turn_failed"
        );
    }

    #[tokio::test]
    async fn watchdog_leaves_the_turn_running_while_the_policy_snapshot_is_unavailable() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let (chat, _, request_id) = stale_turn(&app, &u, 400).await;
        let before = quota_rows(&app, u.tenant, u.user).await;
        app.usage.fail_snapshots(true);
        let watchdog = OrphanWatchdog::new(&app.services);

        assert_eq!(watchdog.scan_once(db_now()).await.unwrap(), 0);
        assert_eq!(turn_of(&app, chat, request_id).await.state, "running");
        assert_eq!(quota_rows(&app, u.tenant, u.user).await, before);

        app.usage.fail_snapshots(false);
        assert_eq!(watchdog.scan_once(db_now()).await.unwrap(), 1);
        assert_eq!(turn_of(&app, chat, request_id).await.state, "failed");
    }

    #[tokio::test]
    async fn watchdog_unblocks_chat() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let who = ctx(u.tenant, u.user);
        let (chat, _, request_id) = stale_turn(&app, &u, 400).await;
        script_provider(&app, answer(&["ok"], 5, 2));
        let blocked = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "hi"}))
            .await
            .expect_err("a running turn blocks the chat");
        assert_eq!(blocked.status, 409, "{}", blocked.json);

        assert_eq!(
            OrphanWatchdog::new(&app.services)
                .scan_once(db_now())
                .await
                .unwrap(),
            1
        );

        let frames = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "hi"}))
            .await
            .expect("send after the orphan finalization");
        assert_eq!(frames.last().unwrap().event, "done");
        let status = app
            .call(
                "GET",
                &format!(
                    "{}/{chat}/turns/{request_id}",
                    crate::test_support::stream::CHATS
                ),
                &who,
                None,
            )
            .await;
        assert_eq!(status.status, 200, "{}", status.json);
        assert_eq!(status.json["state"], "error");
        assert_eq!(status.json["error_code"], "orphan_timeout");
    }
}
