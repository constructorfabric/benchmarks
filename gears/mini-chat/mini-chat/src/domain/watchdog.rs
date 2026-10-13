//! Orphan turn watchdog (DESIGN "Turn Lifecycle, Crash Recovery and Orphan
//! Handling", "Orphan Turn Watchdog", B.9.1): a periodic scan finalizes the
//! `running` turns whose process died.
//!
//! A turn is an orphan when `COALESCE(last_progress_at, started_at) <= now -
//! orphan_watchdog.timeout_secs` (application clock). Each scan takes at most
//! [`SCAN_BATCH`] candidates; every candidate is finalized in **one**
//! transaction whose first statement is the orphan CAS that re-checks the whole
//! predicate: only the winner (`rows_affected = 1`) settles the quota by the
//! estimated formula, against the periods of `started_at` and the policy
//! snapshot of `policy_version_applied`, and enqueues the usage and audit
//! events. A loser (the stream finalized first, progress was renewed, the turn
//! was deleted) changes and emits nothing, so concurrent sweeps, or a sweep
//! racing the stream's own finalizer, settle a turn exactly once.
//!
//! A turn without reserve columns (a retry / edit turn left `running` before
//! its reserve) or without a requester is finalized and its events are
//! enqueued, but no `quota_usage` row changes (DESIGN 5.7).

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{ModelTier, SettlementMethod, TierLimits, UserLimits};
use time::OffsetDateTime;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::TxConfig;
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::leader::{LeaderElector, SingleProcessElector};
use crate::domain::ports::{
    AuditPort, ModelPolicyPort, TurnLedgerRepository, TurnLifecycleRepository, UsagePort,
};
use crate::domain::provider::TokenBreakdown;
use crate::domain::quota::{Charge, PeriodStarts, TurnReserve, derive_billing};
use crate::domain::settlement::{
    OutboxPorts, Settlement, SettlementScopes, TerminalFacts, TurnLedger, charge_for,
    enqueue_events, settle_turn,
};
use crate::domain::turns::{OrphanCandidate, TurnState};

/// `chat_turns.error_code` of an orphaned turn.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// Candidates taken by one scan (fixed, not configurable; DESIGN step 1).
pub const SCAN_BATCH: u64 = 100;

/// `quota_decision` of the audit event of an orphan: the decision of the turn
/// is not stored.
const QUOTA_DECISION_UNKNOWN: &str = "unknown";

/// Finalizes orphaned turns.
pub struct OrphanWatchdog {
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyPort>,
    config: Arc<MiniChatConfig>,
    usage: Option<Arc<dyn UsagePort>>,
    audit: Option<Arc<dyn AuditPort>>,
    elector: Arc<dyn LeaderElector>,
}

/// What one orphan finalization transaction writes (cloned per attempt).
#[derive(Clone)]
struct Finalization {
    turn_id: Uuid,
    scopes: SettlementScopes,
    ledger: TurnLedger,
    facts: TerminalFacts,
    settlement: Settlement,
    /// The turn holds a reserve to release and a debit to commit.
    settles_quota: bool,
    cutoff: OffsetDateTime,
    usage: Option<Arc<dyn UsagePort>>,
    audit: Option<Arc<dyn AuditPort>>,
}

impl OrphanWatchdog {
    /// Creates the watchdog. The policy port supplies the snapshot of the
    /// version a turn was planned under. Without [`Self::with_outbox`] nothing
    /// is enqueued. It runs as the leader of a single process until
    /// [`Self::with_elector`] says otherwise.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static>(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            db,
            policy,
            config,
            usage: None,
            audit: None,
            elector: Arc::new(SingleProcessElector),
        }
    }

    /// Wires the usage and audit outbox ports of the finalization.
    #[must_use]
    pub fn with_outbox<U: UsagePort + 'static, A: AuditPort + 'static>(
        mut self,
        usage: Arc<U>,
        audit: Arc<A>,
    ) -> Self {
        self.usage = Some(usage);
        self.audit = Some(audit);
        self
    }

    /// Replaces the single-process elector.
    #[must_use]
    pub fn with_elector(mut self, elector: Arc<dyn LeaderElector>) -> Self {
        self.elector = elector;
        self
    }

    /// The periodic task: returns at once when `orphan_watchdog.enabled` is
    /// false, otherwise scans every `scan_interval_secs` (the first scan right
    /// at the start) with the application clock until `cancel` is cancelled.
    /// A failing scan is logged and the loop goes on; a scan that started is
    /// completed (it is bounded by [`SCAN_BATCH`]).
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let settings = &self.config.orphan_watchdog;
        if !settings.enabled {
            tracing::info!("orphan watchdog is disabled");
            return;
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(settings.scan_interval_secs));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
            self.scan().await;
        }
    }

    /// One periodic scan: skipped while this process is not the leader, a
    /// failure is logged.
    async fn scan(&self) {
        if !self.elector.is_leader().await {
            return;
        }
        match self.sweep_at(now_utc()).await {
            Ok(0) => {}
            Ok(finalized) => tracing::info!(finalized, "orphan watchdog finalized turns"),
            Err(error) => tracing::warn!(%error, "orphan watchdog scan failed"),
        }
    }

    /// One scan at the application time `now`; returns the number of turns this
    /// call finalized (CAS wins). A turn that cannot be finalized now (the
    /// policy snapshot of its version is unavailable, the transaction failed)
    /// is logged and left `running` for the next scan.
    ///
    /// # Errors
    /// `Internal` when the candidates cannot be read.
    pub async fn sweep_at(&self, now: OffsetDateTime) -> Result<usize, DomainError> {
        let timeout = i64::try_from(self.config.orphan_watchdog.timeout_secs).unwrap_or(i64::MAX);
        let Some(cutoff) = now.checked_sub(time::Duration::seconds(timeout)) else {
            return Ok(0);
        };
        // The scan crosses tenants: it is a system task, not a request. Every
        // candidate is then settled with the scope of its own tenant.
        let candidates = self
            .db
            .conn()?
            .stale_running_turns(&AccessScope::allow_all(), cutoff, SCAN_BATCH)
            .await?;
        let mut finalized = 0;
        for candidate in candidates {
            match self.finalize_orphan(&candidate, now, cutoff).await {
                Ok(true) => finalized += 1,
                Ok(false) => {
                    tracing::debug!(turn_id = %candidate.id, "orphan candidate is no longer finalizable");
                }
                Err(error) => {
                    tracing::warn!(turn_id = %candidate.id, %error,
                        "orphan turn was not finalized; the next scan retries it");
                }
            }
        }
        Ok(finalized)
    }

    async fn finalize_orphan(
        &self,
        candidate: &OrphanCandidate,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let finalization = self.prepare(candidate, now, cutoff).await?;
        let wake = self
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let finalization = finalization.clone();
                Box::pin(async move { finalize_tx(tx, &finalization).await })
            })
            .await?;
        Ok(match wake {
            Some(wake) => {
                // The rows are durable now: wake the outbox sequencers.
                wake.fire();
                true
            }
            None => false,
        })
    }

    /// The pure part of the finalization, computed before the transaction: the
    /// ledger from the persisted row and the snapshot of its policy version,
    /// the estimated charge.
    async fn prepare(
        &self,
        candidate: &OrphanCandidate,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) -> Result<Finalization, DomainError> {
        let tenant = AccessScope::for_tenant(candidate.tenant_id);
        let facts = TerminalFacts {
            state: TurnState::Failed,
            error_code: Some(ORPHAN_TIMEOUT.to_owned()),
            usage: None,
            breakdown: TokenBreakdown::default(),
            latency_ms: u64::try_from((now - candidate.started_at).whole_milliseconds())
                .unwrap_or_default(),
            now,
        };
        let user_id = candidate.requester_user_id.unwrap_or_else(Uuid::nil);
        let scopes = SettlementScopes {
            turn: tenant.clone(),
            quota: tenant.ensure_owner(user_id),
        };
        let (ledger, settlement, settles_quota) = if let Some(reserved) = reserved(candidate) {
            let (ledger, settlement) = self.settle(candidate, &reserved, &facts).await?;
            (ledger, settlement, true)
        } else {
            tracing::warn!(turn_id = %candidate.id,
                "orphan turn has no reserve or requester: nothing to settle, events only");
            let (ledger, settlement) = unreserved(candidate, user_id, &facts);
            (ledger, settlement, false)
        };
        Ok(Finalization {
            turn_id: candidate.id,
            scopes,
            ledger,
            facts,
            settlement,
            settles_quota,
            cutoff,
            usage: self.usage.clone(),
            audit: self.audit.clone(),
        })
    }

    /// The ledger of a turn that holds a reserve and its estimated charge.
    async fn settle(
        &self,
        candidate: &OrphanCandidate,
        reserved: &Reserved,
        facts: &TerminalFacts,
    ) -> Result<(TurnLedger, Settlement), DomainError> {
        // The multipliers of the version the turn was planned under, never the
        // current policy.
        let snapshot = self
            .policy
            .snapshot_at(reserved.user_id, reserved.policy_version)
            .await?;
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == reserved.effective_model)
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "model of turn {} is not in policy version {}",
                    candidate.id, reserved.policy_version
                ))
            })?;
        let ledger = TurnLedger {
            tenant_id: candidate.tenant_id,
            user_id: reserved.user_id,
            chat_id: candidate.chat_id,
            turn_id: candidate.id,
            request_id: candidate.request_id,
            // `chat_turns` does not store the selected model.
            selected_model: reserved.effective_model.clone(),
            effective_model: reserved.effective_model.clone(),
            tier: entry.tier,
            in_mult: entry.input_tokens_credit_multiplier_micro,
            out_mult: entry.output_tokens_credit_multiplier_micro,
            policy_version: reserved.policy_version,
            reserve: reserved.reserve,
            starts: PeriodStarts::at(candidate.started_at),
            quota_decision: QUOTA_DECISION_UNKNOWN.to_owned(),
            limits: no_limits(reserved.user_id, reserved.policy_version),
        };
        let settlement = charge_for(&ledger, facts, self.config.quota.overshoot_tolerance_factor)
            .map_err(|error| {
            DomainError::internal(format!(
                "settlement of turn {} failed: {error}",
                candidate.id
            ))
        })?;
        Ok((ledger, settlement))
    }
}

/// The persisted reserve of a turn, when it has all of it.
struct Reserved {
    user_id: Uuid,
    effective_model: String,
    policy_version: u64,
    reserve: TurnReserve,
}

fn reserved(candidate: &OrphanCandidate) -> Option<Reserved> {
    Some(Reserved {
        user_id: candidate.requester_user_id?,
        effective_model: candidate.effective_model.clone()?,
        policy_version: candidate.policy_version_applied?,
        reserve: TurnReserve {
            reserve_tokens: candidate.reserve_tokens?,
            max_output_tokens_applied: candidate.max_output_tokens_applied?,
            reserved_credits_micro: candidate.reserved_credits_micro?,
            minimal_generation_floor_applied: candidate.minimal_generation_floor_applied?,
        },
    })
}

/// The ledger and the zero charge of a turn without a reserve: the events
/// carry empty models and policy version 0.
fn unreserved(
    candidate: &OrphanCandidate,
    user_id: Uuid,
    facts: &TerminalFacts,
) -> (TurnLedger, Settlement) {
    let ledger = TurnLedger {
        tenant_id: candidate.tenant_id,
        user_id,
        chat_id: candidate.chat_id,
        turn_id: candidate.id,
        request_id: candidate.request_id,
        selected_model: candidate.effective_model.clone().unwrap_or_default(),
        effective_model: candidate.effective_model.clone().unwrap_or_default(),
        tier: ModelTier::Standard,
        in_mult: 0,
        out_mult: 0,
        policy_version: candidate.policy_version_applied.unwrap_or(0),
        reserve: TurnReserve {
            reserve_tokens: 0,
            max_output_tokens_applied: 0,
            reserved_credits_micro: 0,
            minimal_generation_floor_applied: 0,
        },
        starts: PeriodStarts::at(candidate.started_at),
        quota_decision: QUOTA_DECISION_UNKNOWN.to_owned(),
        limits: no_limits(user_id, 0),
    };
    let settlement = Settlement {
        billing: derive_billing(facts.state, facts.error_code.as_deref(), facts.usage),
        charge: Charge {
            credits_micro: 0,
            method: SettlementMethod::Estimated,
            input_tokens: 0,
            output_tokens: 0,
            breakdown: TokenBreakdown::NONE,
        },
    };
    (ledger, settlement)
}

/// The limits feed the post-settlement warnings only, which the watchdog does
/// not ask for.
fn no_limits(user_id: Uuid, policy_version: u64) -> UserLimits {
    let none = TierLimits {
        limit_daily_credits_micro: 0,
        limit_monthly_credits_micro: 0,
    };
    UserLimits {
        user_id,
        policy_version,
        standard: none.clone(),
        premium: none,
    }
}

/// The orphan finalization transaction: the CAS first; only its winner settles
/// and enqueues. `None` when the CAS was lost.
async fn finalize_tx(
    tx: &DbTx<'_>,
    job: &Finalization,
) -> Result<Option<toolkit_db::outbox::Wake>, DomainError> {
    if !tx
        .finalize_orphan_turn(&job.scopes.turn, job.turn_id, job.cutoff, job.facts.now)
        .await?
    {
        return Ok(None);
    }
    let ports = OutboxPorts {
        usage: job.usage.as_deref(),
        audit: job.audit.as_deref(),
    };
    let wake = if job.settles_quota {
        settle_turn(
            tx,
            &job.scopes,
            &job.ledger,
            &job.facts,
            &job.settlement,
            None,
            ports,
        )
        .await?
        .wake
    } else {
        let tools = tx.tool_counts(&job.scopes.turn, job.turn_id).await?;
        enqueue_events(tx, &job.ledger, &job.facts, &job.settlement, &tools, ports).await?
    };
    Ok(Some(wake))
}
