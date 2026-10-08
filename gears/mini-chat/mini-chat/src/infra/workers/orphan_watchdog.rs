//! Orphan turn watchdog (DESIGN §4 "Orphan Turn Watchdog", B.9.1, §5.7
//! "`FinalizeTurn` Invariant", spec §13.2): fails `running` turns whose
//! `last_progress_at` (`started_at` when NULL) is older than
//! `orphan_watchdog.timeout_secs`.
//!
//! Each scan attempts at most 100 candidates (oldest `started_at` first;
//! advisory only), paging past the candidates whose finalization failed in a
//! recent scan: such a candidate is held back for a backoff that doubles per
//! consecutive failure (`scan_interval_secs` up to 64 times that), so
//! permanently failing turns cannot fill the window and starve the others. Per candidate, one transaction: orphan CAS (re-checks every predicate)
//! → estimated quota settlement (skipped when a reserve field or the requester
//! is NULL) → usage event (`aborted` / `estimated`) → audit event (`turn_failed`,
//! decision `unknown`). Outbox wakes fire after the commit. A candidate that
//! loses the CAS changes nothing. The settlement periods derive from
//! `started_at`, as the streaming path's preflight periods do.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use mini_chat_sdk::MiniChatAuditEvent;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::leader::{LeaderElector, ORPHAN_WATCHDOG_ROLE};
use crate::config::MiniChatConfig;
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::estimation::period_starts;
use crate::domain::model::{TurnState, error_codes};
use crate::domain::services::finalization::{
    SettlementInput, TurnEventFacts, TurnReserve, build_turn_audit, build_usage_event,
    compute_settlement, derive_billing,
};
use crate::domain::services::quota::{QuotaService, Settlement};
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repos::TurnRepo;
use crate::infra::db::tx::with_tx_retry;
use crate::infra::gateways::model_policy::ModelPolicyGateway;
use crate::infra::outbox::payloads::{AUDIT_PAYLOAD_TYPE, USAGE_PAYLOAD_TYPE};
use crate::infra::outbox::{OutboxEnqueuer, PendingWakes, QueueKind};

/// Candidates attempted per scan (fixed; the rest are picked up by later
/// scans). Also the page size of the candidate query.
const SCAN_LIMIT: u64 = 100;
/// Cap of the failure backoff, in scan intervals.
const MAX_BACKOFF_INTERVALS: i32 = 64;

/// A candidate whose finalization failed: held back until `retry_at`.
#[derive(Debug, Clone, Copy)]
struct FailedCandidate {
    failures: u32,
    retry_at: DateTime<Utc>,
}

/// Infrastructure of [`OrphanWatchdog`].
pub struct OrphanWatchdogDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub quota: Arc<QuotaService>,
    pub elector: Arc<dyn LeaderElector>,
}

/// Everything the orphan transaction writes, computed before it starts
/// (cloned per transaction attempt).
#[derive(Clone)]
struct OrphanPlan {
    settlement: Option<Settlement>,
    usage_event: mini_chat_sdk::UsageEvent,
    audit_event: mini_chat_sdk::TurnAuditEvent,
}

/// The orphan turn watchdog.
pub struct OrphanWatchdog {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyGateway>,
    outbox: Arc<OutboxEnqueuer>,
    quota: Arc<QuotaService>,
    elector: Arc<dyn LeaderElector>,
    /// Candidates whose finalization failed recently (in-memory, per instance).
    failed: Mutex<HashMap<Uuid, FailedCandidate>>,
}

impl OrphanWatchdog {
    #[must_use]
    pub fn new(deps: OrphanWatchdogDeps) -> Self {
        let OrphanWatchdogDeps {
            config,
            db,
            policy,
            outbox,
            quota,
            elector,
        } = deps;
        Self {
            cfg: config,
            db,
            policy,
            outbox,
            quota,
            elector,
            failed: Mutex::new(HashMap::new()),
        }
    }

    /// Run the scans every `scan_interval_secs` while this instance leads the
    /// `orphan-watchdog` role, until `cancel` fires.
    #[must_use = "the handle reports when the task ended; drop it to detach"]
    pub fn spawn(self: Arc<Self>, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                self.cfg.orphan_watchdog.scan_interval_secs,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            info!("orphan watchdog started");
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                let scan = async {
                    if !self.elector.is_leader(ORPHAN_WATCHDOG_ROLE).await {
                        debug!("orphan watchdog: not the leader; scan skipped");
                        return;
                    }
                    match self.scan_once(now_utc()).await {
                        Ok(0) => {}
                        Ok(n) => info!(finalized = n, "orphan watchdog finalized turns"),
                        Err(err) => error!(%err, "orphan watchdog scan failed"),
                    }
                };
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    () = scan => {}
                }
            }
            info!("orphan watchdog stopped");
        })
    }

    /// One scan at application time `now`; returns the number of turns this scan
    /// finalized. A candidate that cannot be finalized (database or policy
    /// failure) is logged, left `running` and held back from the next scans
    /// for a backoff; the scan pages past held-back candidates.
    ///
    /// # Errors
    /// The candidate query failed.
    pub async fn scan_once(&self, now: DateTime<Utc>) -> DomainResult<u32> {
        let timeout = i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(i64::MAX);
        let cutoff = now - chrono::Duration::seconds(timeout);
        let mut finalized = 0_u32;
        let mut attempted = 0_u64;
        let mut after = None;
        let mut seen = HashSet::new();
        let reached_end = 'scan: loop {
            let page = {
                let conn = self.db.conn()?;
                TurnRepo::stale_running(&conn, cutoff, after, SCAN_LIMIT).await?
            };
            for turn in &page {
                seen.insert(turn.id);
                if self.held_back(turn.id, now) {
                    continue;
                }
                if attempted == SCAN_LIMIT {
                    break 'scan false;
                }
                attempted += 1;
                if self.attempt(turn, cutoff, now).await {
                    finalized += 1;
                }
            }
            if u64::try_from(page.len()).unwrap_or(u64::MAX) < SCAN_LIMIT {
                break true;
            }
            after = page.last().map(|t| (t.started_at, t.id));
        };
        if reached_end {
            // Forget the failures of turns that are no longer candidates.
            self.failed_map().retain(|id, _| seen.contains(id));
        }
        Ok(finalized)
    }

    /// Try to finalize one candidate; `true` when this attempt finalized it. A
    /// failure is logged and recorded for the backoff.
    async fn attempt(
        &self,
        turn: &chat_turn::Model,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        // mini_chat_orphan_detected_total{reason="stale_progress"}
        match self.finalize(turn, cutoff, now).await {
            Ok(true) => {
                // mini_chat_orphan_finalized_total{reason="stale_progress"}
                self.forget_failure(turn.id);
                true
            }
            Ok(false) => {
                debug!(turn_id = %turn.id, "orphan candidate no longer finalizable; skipped");
                self.forget_failure(turn.id);
                false
            }
            Err(err) => {
                let retry_at = self.record_failure(turn.id, now);
                error!(turn_id = %turn.id, %err, %retry_at, "orphan finalization failed; turn left running");
                false
            }
        }
    }

    fn failed_map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, FailedCandidate>> {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `id` failed recently and its backoff has not elapsed at `now`.
    fn held_back(&self, id: Uuid, now: DateTime<Utc>) -> bool {
        self.failed_map().get(&id).is_some_and(|f| now < f.retry_at)
    }

    fn forget_failure(&self, id: Uuid) {
        self.failed_map().remove(&id);
    }

    /// Record a failed attempt on `id` at `now`; returns when it is retried.
    fn record_failure(&self, id: Uuid, now: DateTime<Utc>) -> DateTime<Utc> {
        let interval =
            i64::try_from(self.cfg.orphan_watchdog.scan_interval_secs).unwrap_or(i64::MAX);
        let mut failed = self.failed_map();
        let entry = failed.entry(id).or_insert(FailedCandidate {
            failures: 0,
            retry_at: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        let intervals = 2_i32
            .saturating_pow(entry.failures.saturating_sub(1))
            .min(MAX_BACKOFF_INTERVALS);
        let backoff = chrono::Duration::try_seconds(interval.saturating_mul(i64::from(intervals)))
            .unwrap_or(chrono::Duration::MAX);
        entry.retry_at = now
            .checked_add_signed(backoff)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        entry.retry_at
    }

    /// Finalize one candidate; `false` when it lost the orphan CAS.
    async fn finalize(
        &self,
        turn: &chat_turn::Model,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let plan = self.plan(turn, now).await?;
        let turn_id = turn.id;
        let tenant_id = turn.tenant_id;
        let quota = Arc::clone(&self.quota);
        let outbox = Arc::clone(&self.outbox);
        let wakes: Option<PendingWakes> =
            with_tx_retry(&self.db, "orphan finalization", move |tx| {
                let (plan, quota, outbox) = (plan.clone(), Arc::clone(&quota), Arc::clone(&outbox));
                Box::pin(async move {
                    if !TurnRepo::cas_orphan(tx, turn_id, cutoff, now).await? {
                        return Ok::<_, DomainError>(None);
                    }
                    if let Some(settlement) = &plan.settlement {
                        quota.settle_in_tx(tx, settlement).await?;
                    }
                    let mut wakes = PendingWakes::new();
                    wakes.push(
                        outbox
                            .enqueue_json(
                                tx,
                                QueueKind::Usage,
                                tenant_id,
                                USAGE_PAYLOAD_TYPE,
                                &plan.usage_event,
                            )
                            .await?,
                    );
                    wakes.push(
                        outbox
                            .enqueue_json(
                                tx,
                                QueueKind::Audit,
                                tenant_id,
                                AUDIT_PAYLOAD_TYPE,
                                &MiniChatAuditEvent::Turn(plan.audit_event),
                            )
                            .await?,
                    );
                    Ok(Some(wakes))
                })
            })
            .await?;
        match wakes {
            Some(wakes) => {
                wakes.fire();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Settlement and events of the orphan finalization of `turn` (the policy
    /// snapshot is fetched here, outside the transaction).
    async fn plan(&self, turn: &chat_turn::Model, now: DateTime<Utc>) -> DomainResult<OrphanPlan> {
        let (outcome, method) =
            derive_billing(TurnState::Failed, Some(error_codes::ORPHAN_TIMEOUT), None);
        let web_search = i64::from(turn.web_search_completed_count);
        let code_interpreter = i64::from(turn.code_interpreter_completed_count);

        let settlement = if let (Some(reserve), Some(user_id)) =
            (TurnReserve::of(turn), turn.requester_user_id)
        {
            let snapshot = self
                .policy
                .snapshot(user_id, reserve.policy_version_applied)
                .await?;
            let entry = snapshot.find(&reserve.effective_model).ok_or_else(|| {
                DomainError::internal(format!(
                    "effective model `{}` missing from policy snapshot {}",
                    reserve.effective_model, reserve.policy_version_applied
                ))
            })?;
            // Same periods as the preflight of the turn, not the current ones.
            let (daily_start, monthly_start) = period_starts(turn.started_at);
            Some(compute_settlement(&SettlementInput {
                tenant_id: turn.tenant_id,
                user_id,
                daily_start,
                monthly_start,
                reserve: &reserve,
                entry,
                method,
                usage: None,
                web_search_calls: web_search,
                code_interpreter_calls: code_interpreter,
                overshoot_tolerance_factor: self.cfg.quota.overshoot_tolerance_factor,
            })?)
        } else {
            warn!(
                turn_id = %turn.id,
                "orphan turn has no reserve fields or requester; skipping quota settlement"
            );
            None
        };

        // The selected model is not stored on the turn: report the effective one.
        let effective_model = turn.effective_model.clone().unwrap_or_default();
        let facts = TurnEventFacts {
            turn,
            user_id: turn.requester_user_id,
            selected_model: &effective_model,
            state: TurnState::Failed,
            error_code: Some(error_codes::ORPHAN_TIMEOUT),
            usage: None,
            web_search_calls: u32::try_from(turn.web_search_completed_count).unwrap_or(0),
            code_interpreter_calls: u32::try_from(turn.code_interpreter_completed_count)
                .unwrap_or(0),
            file_search_calls: u32::try_from(turn.file_search_completed_count).unwrap_or(0),
            now,
        };
        let credits = settlement.as_ref().map_or(0, |s| s.committed_credits_micro);
        let latency_ms = (now - turn.started_at).num_milliseconds().max(0);
        Ok(OrphanPlan {
            settlement,
            usage_event: build_usage_event(&facts, outcome, method, credits),
            audit_event: build_turn_audit(&facts, None, latency_ms),
        })
    }
}
