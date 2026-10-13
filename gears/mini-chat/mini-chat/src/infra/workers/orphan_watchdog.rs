//! Orphan turn watchdog (D "Orphan Turn Watchdog", B.9.1, D§5.7
//! `FinalizeTurn` invariant, S§10.3).
//!
//! Each scan (leader only) reads at most [`MAX_CANDIDATES`] `running`,
//! non-deleted turns whose `last_progress_at` (or `started_at` when NULL)
//! is at or before `now - orphan_watchdog.timeout_secs` (application
//! clock). Discovery is advisory: per candidate one transaction runs the
//! orphan CAS (which re-checks the whole predicate) and, only when it wins,
//! the estimated quota settlement and the usage + `turn_failed` audit
//! events. The orphan path has its own CAS but shares the billing outcome
//! derivation (`billing::derive`), the settlement (`QuotaService::settle`)
//! and the outbox enqueue with the stream finalization. It records
//! `selected_model` = the effective model and quota decision `unknown`; a
//! turn without reserve fields or requester (a retry / edit turn left
//! running before its preflight was written) is finalized without
//! settlement, with a zero usage event. A turn whose effective model is
//! missing from its policy version (or cannot be priced) is finalized too:
//! its reserve is released without a debit (`QuotaService::release_reserve`)
//! and the usage event carries `actual_credits_micro = 0`. A failed snapshot
//! lookup (plugin unavailable) leaves the turn `running` for a later scan.
//! Messages are never touched.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent,
    RequesterType, TerminalState, TurnAuditEvent, TurnAuditEventType, UsageEvent,
};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::domain::billing::{self, SettlementMethod, TurnReserve, settle_amount};
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::estimation::multipliers;
use crate::domain::ports::{OutboxPort, PendingWakes, PolicyPort};
use crate::domain::services::finalization::dedupe_key;
use crate::domain::services::quota_service::{ReleaseInput, SettleInput, period_starts_from};
use crate::domain::services::{AppServices, QuotaService};
use crate::infra::db::entity::chat_turn;
use crate::infra::db::repos::TurnRepo;
use crate::infra::db::timestamps::comparable;
use crate::infra::db::tx::with_retry;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::workers::leader::{LeaderElector, ORPHAN_WATCHDOG_ROLE};
use crate::infra::workers::run_periodic;

/// Candidates read per scan (fixed, D "Orphan Turn Watchdog").
pub const MAX_CANDIDATES: u64 = 100;

/// Error code of an orphan-finalized turn.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// Audit quota decision of an orphan turn (the preflight decision is not
/// persisted).
const UNKNOWN_DECISION: &str = "unknown";

/// Result of one scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Candidates found by the stale-progress rule.
    pub detected: u64,
    /// Candidates whose orphan CAS won and committed.
    pub finalized: u64,
}

/// Quota step of an orphan finalization.
#[derive(Debug, Clone, Copy)]
enum QuotaStep {
    /// Estimated settlement (the effective model is priced in the turn's
    /// policy version).
    Settle(SettleInput),
    /// The effective model is missing from the turn's policy version (or
    /// its multipliers are invalid): release the reserve without a debit.
    Release(ReleaseInput),
    /// No reserve fields or no requester: nothing to settle.
    Skip,
}

/// Rows / events of one orphan finalization (built before the transaction).
#[derive(Clone)]
struct OrphanPlan {
    quota: QuotaStep,
    usage: UsageEvent,
    /// `None` when the turn has no requester.
    audit: Option<MiniChatAuditEvent>,
}

/// Persisted reserve of a turn that took one.
struct PersistedReserve {
    user: Uuid,
    reserve: TurnReserve,
    policy_version: u64,
    effective_model: String,
}

/// The reserve fields of `c` when all of them (and the requester) are set.
fn persisted_reserve(c: &chat_turn::Model) -> Option<PersistedReserve> {
    Some(PersistedReserve {
        user: c.requester_user_id?,
        reserve: TurnReserve {
            reserve_tokens: c.reserve_tokens?,
            max_output_tokens_applied: i64::from(c.max_output_tokens_applied?),
            reserved_credits_micro: c.reserved_credits_micro?,
            minimal_generation_floor_applied: i64::from(c.minimal_generation_floor_applied?),
        },
        policy_version: u64::try_from(c.policy_version_applied?).ok()?,
        effective_model: c.effective_model.clone()?,
    })
}

fn count(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

/// Finalizes orphan turns.
pub struct OrphanWatchdog {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    policy: Arc<dyn PolicyPort>,
    quota: Arc<QuotaService>,
    outbox: Arc<dyn OutboxPort>,
    metrics: Arc<MiniChatMetrics>,
    timeout: time::Duration,
    interval: Duration,
    tolerance: f64,
}

impl OrphanWatchdog {
    /// Watchdog over the services' database, policy, quota and outbox, with
    /// the `orphan_watchdog` configuration.
    #[must_use]
    pub fn new(s: &AppServices) -> Self {
        let cfg = &s.config.orphan_watchdog;
        Self {
            db: Arc::clone(&s.db),
            clock: Arc::clone(&s.clock),
            policy: Arc::clone(&s.policy),
            quota: Arc::clone(&s.quota),
            outbox: Arc::clone(&s.outbox),
            metrics: Arc::clone(&s.metrics),
            timeout: time::Duration::seconds(i64::try_from(cfg.timeout_secs).unwrap_or(i64::MAX)),
            interval: Duration::from_secs(cfg.scan_interval_secs),
            tolerance: s.config.quota.overshoot_tolerance_factor,
        }
    }

    /// Scan every `orphan_watchdog.scan_interval_secs` while leader, until
    /// `cancel`.
    #[must_use = "the task is detached when the handle is dropped"]
    pub fn spawn(
        self: Arc<Self>,
        elector: Arc<dyn LeaderElector>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let interval = self.interval;
            run_periodic(ORPHAN_WATCHDOG_ROLE, interval, elector, cancel, || {
                let me = Arc::clone(&self);
                async move {
                    me.scan_once(me.clock.now()).await;
                }
            })
            .await;
        })
    }

    /// One scan at `now`. Failures are logged; the affected turns are
    /// retried by a later scan.
    pub async fn scan_once(&self, now: OffsetDateTime) -> ScanReport {
        let started = Instant::now();
        let mut report = ScanReport::default();
        let candidates = match self.candidates(now).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "orphan watchdog scan failed");
                self.metrics.orphan_scan_duration(started.elapsed());
                return report;
            }
        };
        for c in &candidates {
            report.detected += 1;
            self.metrics.orphan_detected();
            report.finalized += u64::from(self.try_finalize(c, now).await);
        }
        self.metrics.orphan_scan_duration(started.elapsed());
        if report.detected > 0 {
            info!(
                detected = report.detected,
                finalized = report.finalized,
                "orphan watchdog scan"
            );
        }
        report
    }

    /// [`Self::finalize_candidate`] with its failure logged; `true` when
    /// finalized.
    async fn try_finalize(&self, c: &chat_turn::Model, now: OffsetDateTime) -> bool {
        match self.finalize_candidate(c, now).await {
            Ok(won) => {
                if !won {
                    debug!(turn_id = %c.id, "orphan candidate no longer finalizable");
                }
                won
            }
            Err(e) => {
                warn!(turn_id = %c.id, error = %e, "orphan finalization failed; retried by a later scan");
                false
            }
        }
    }

    /// Candidates of every tenant at `now` (advisory).
    async fn candidates(&self, now: OffsetDateTime) -> Result<Vec<chat_turn::Model>, DomainError> {
        let conn = self.db.conn()?;
        let cutoff = comparable(self.db.db().backend(), now - self.timeout);
        Ok(TurnRepo
            .list_orphan_candidates(&conn, &AccessScope::allow_all(), &cutoff, MAX_CANDIDATES)
            .await?)
    }

    /// Finalize candidate `c` at `now`: orphan CAS, then settlement and
    /// events, in one transaction. `Ok(false)` when the CAS updated no row
    /// (finalized, deleted or refreshed meanwhile): nothing else is written.
    ///
    /// # Errors
    /// Policy snapshot lookup, credit computation, database or outbox
    /// failures; the turn stays `running`.
    pub async fn finalize_candidate(
        &self,
        c: &chat_turn::Model,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let plan = self.plan(c, now).await?;
        let cutoff = comparable(self.db.db().backend(), now - self.timeout);
        let scope = AccessScope::for_tenant(c.tenant_id);
        let (quota, outbox, turn_id) = (Arc::clone(&self.quota), Arc::clone(&self.outbox), c.id);
        let committed = with_retry(&self.db, move |tx| {
            let (quota, outbox, scope, cutoff, plan) = (
                Arc::clone(&quota),
                Arc::clone(&outbox),
                scope.clone(),
                cutoff.clone(),
                plan.clone(),
            );
            Box::pin(async move {
                if TurnRepo
                    .orphan_cas(tx, &scope, turn_id, &cutoff, now)
                    .await?
                    == 0
                {
                    return Ok(None);
                }
                match plan.quota {
                    QuotaStep::Settle(settle) => {
                        quota.settle(tx, &settle).await?;
                    }
                    QuotaStep::Release(mut release) => {
                        release.premium_candidate = !TurnRepo
                            .has_other_running(tx, &scope, release.user, turn_id)
                            .await?;
                        quota.release_reserve(tx, &release).await?;
                    }
                    QuotaStep::Skip => {}
                }
                let mut wakes = PendingWakes::new();
                outbox.enqueue_usage(tx, &plan.usage, &mut wakes).await?;
                if let Some(audit) = &plan.audit {
                    outbox.enqueue_audit(tx, audit, &mut wakes).await?;
                }
                Ok(Some(wakes))
            })
        })
        .await?;
        let Some(wakes) = committed else {
            return Ok(false);
        };
        wakes.fire_all();
        self.metrics.orphan_finalized();
        info!(%turn_id, chat_id = %c.chat_id, "orphan turn finalized");
        Ok(true)
    }

    /// Settlement and events of candidate `c`.
    async fn plan(
        &self,
        c: &chat_turn::Model,
        now: OffsetDateTime,
    ) -> Result<OrphanPlan, DomainError> {
        let state = TerminalState::Failed;
        let (outcome, method) = billing::derive(state, Some(ORPHAN_TIMEOUT), None);
        let tools = AuditToolCalls {
            web_search_calls: count(c.web_search_completed_count),
            file_search_calls: count(c.file_search_completed_count),
        };
        let code_interpreter_calls = count(c.code_interpreter_completed_count);

        let (quota, model, policy_version) = if let Some(r) = persisted_reserve(c) {
            let (model, version) = (r.effective_model.clone(), r.policy_version);
            let step = self
                .quota_step(c, r, method, code_interpreter_calls)
                .await?;
            (step, model, version)
        } else {
            warn!(turn_id = %c.id, "orphan turn has no reserve fields or requester; quota settlement skipped");
            (QuotaStep::Skip, String::new(), 0)
        };
        let credits = match quota {
            QuotaStep::Settle(s) => s.settlement.committed_credits_micro,
            QuotaStep::Release(_) | QuotaStep::Skip => 0,
        };

        let usage = UsageEvent {
            tenant_id: c.tenant_id,
            user_id: c.requester_user_id,
            chat_id: c.chat_id,
            turn_id: Some(c.id),
            request_id: c.request_id,
            effective_model: model.clone(),
            selected_model: model.clone(),
            terminal_state: state,
            billing_outcome: outcome,
            usage: None,
            actual_credits_micro: credits,
            settlement_method: method,
            policy_version_applied: policy_version,
            web_search_calls: tools.web_search_calls,
            code_interpreter_calls,
            file_search_calls: tools.file_search_calls,
            timestamp: now,
            requester_type: RequesterType::User,
            dedupe_key: dedupe_key(c.tenant_id, c.id, c.request_id),
            system_task_type: None,
        };
        let audit = audit_event(c, model, tools, now);
        Ok(OrphanPlan {
            quota,
            usage,
            audit,
        })
    }

    /// Quota step of a turn with a persisted reserve: the estimated
    /// settlement with the tier and multipliers of the effective model in
    /// the turn's policy version (periods of the turn's `started_at`). When
    /// that model is missing from the snapshot or cannot be priced, the
    /// turn is still finalized (a permanent condition must not keep the
    /// chat blocked): the reserve is released without a debit. Snapshot
    /// lookup failures (plugin unavailable, ...) are returned, so the turn
    /// stays `running` and a later scan retries it.
    async fn quota_step(
        &self,
        c: &chat_turn::Model,
        r: PersistedReserve,
        method: SettlementMethod,
        code_interpreter_calls: u32,
    ) -> Result<QuotaStep, DomainError> {
        let snap = self
            .policy
            .snapshot_for_version(r.user, r.policy_version)
            .await?;
        let periods = period_starts_from(c.started_at);
        let priced = snap
            .model_catalog
            .iter()
            .find(|m| m.id == r.effective_model)
            .ok_or_else(|| "model missing from the policy version".to_owned())
            .and_then(|entry| {
                let mults = multipliers(entry).map_err(|e| e.to_string())?;
                let settlement = settle_amount(method, &r.reserve, None, self.tolerance, mults)
                    .map_err(|e| e.to_string())?;
                Ok((entry.tier, settlement))
            });
        Ok(match priced {
            Ok((tier, settlement)) => QuotaStep::Settle(SettleInput {
                tenant: c.tenant_id,
                user: r.user,
                effective_tier: tier,
                periods,
                turn: r.reserve,
                method,
                settlement,
                web_search_calls: count(c.web_search_completed_count),
                code_interpreter_calls,
            }),
            Err(reason) => {
                warn!(
                    turn_id = %c.id,
                    model = %r.effective_model,
                    policy_version = r.policy_version,
                    reason,
                    "orphan turn cannot be priced; reserve released without a debit"
                );
                QuotaStep::Release(ReleaseInput {
                    tenant: c.tenant_id,
                    user: r.user,
                    periods,
                    reserved_credits_micro: r.reserve.reserved_credits_micro,
                    premium_candidate: false,
                })
            }
        })
    }
}

/// `turn_failed` audit event of an orphan turn (`None` without requester):
/// `selected_model` = the effective model, quota decision `unknown`.
fn audit_event(
    c: &chat_turn::Model,
    model: String,
    tools: AuditToolCalls,
    now: OffsetDateTime,
) -> Option<MiniChatAuditEvent> {
    let Some(user) = c.requester_user_id else {
        warn!(turn_id = %c.id, "orphan turn has no requester; audit event skipped");
        return None;
    };
    Some(MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: TurnAuditEventType::TurnFailed,
        tenant_id: c.tenant_id,
        user_id: user,
        chat_id: c.chat_id,
        turn_id: c.id,
        request_id: c.request_id,
        selected_model: model.clone(),
        effective_model: model,
        terminal_state: TerminalState::Failed,
        error_code: Some(ORPHAN_TIMEOUT.to_owned()),
        usage: None,
        latency_ms: AuditLatency {
            ttft_ms: None,
            total_ms: u64::try_from((now - c.started_at).whole_milliseconds()).unwrap_or(0),
        },
        tool_calls: tools,
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: UNKNOWN_DECISION.to_owned(),
                downgrade_from: None,
                downgrade_reason: None,
            },
            license: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        quota_scope: None,
        trace_id: None,
        timestamp: now,
    }))
}
