//! Orphan turn finalization (DESIGN section 4 "Orphan Turn Watchdog", B.9.1
//! "Orphan watchdog semantics", section 5.7 "`FinalizeTurn` Invariant",
//! ADR-0010).
//!
//! A turn left `running` by a crashed pod is finalized by the orphan
//! watchdog through its own CAS, then the steps it shares with the stream
//! finalization: the billing outcome derivation
//! ([`crate::domain::billing::derive_billing`], through the settlement),
//! the quota settlement on the estimated path
//! ([`QuotaService::settle_in_tx`]) and the outbox enqueue of the usage and
//! audit events ([`TurnOutbox`]). One transaction per turn:
//!
//! 1. the orphan CAS ([`OrphanTurnRepository::finalize_orphan_in_tx`]), the
//!    first statement (the write lock on `SQLite`): `failed` /
//!    `orphan_timeout`, re-checking `state = 'running'`, `deleted_at IS
//!    NULL` and the stale-progress predicate against `app_now - timeout`
//!    (application clock); no row means the turn is no longer
//!    orphan-finalizable and nothing else is written;
//! 2. the estimated settlement from the persisted reserve columns, with the
//!    period starts of `started_at` ([`PeriodStarts::at`]); skipped (with a
//!    warning) when a reserve column or `requester_user_id` is NULL (a
//!    retry / edit turn stopped before its preflight columns were written).
//!    A turn that can never be priced ([`PricingError::Permanent`]: the
//!    plugin does not serve `policy_version_applied`, the effective model is
//!    not in that snapshot, the credit computation fails) is still
//!    finalized: its reserve is released from bucket `total`, and from
//!    `tier:premium` when the failure carries a resolved premium tier
//!    ([`QuotaService::release_reserve_in_tx`], logged at `error`) with a
//!    zero charge, so the chat is never blocked by a `running` turn and the
//!    oldest-first scan never stalls on it. Only a transient pricing failure
//!    or a write failure rolls the transaction back;
//! 3. the usage event (`aborted` / `estimated`, `usage = null`,
//!    `selected_model` = the effective model; without settlement
//!    `actual_credits_micro = 0`, both models `""`, `policy_version_applied
//!    = 0`; after a reserve release `actual_credits_micro = 0` with the
//!    persisted effective model and policy version) and the `turn_failed` audit event with quota decision `unknown`.
//!
//! No `messages` row is created or changed. The outbox wake fires after the
//! commit.

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, ModelTier};
use time::OffsetDateTime;
use toolkit_db::Db;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use crate::domain::billing::{
    BillingDecision, ORPHAN_TIMEOUT, TerminalState, TurnAuditInput, TurnIdentity, TurnUsageInput,
    derive_billing, turn_audit_event, turn_usage_event, unknown_quota_decision,
};
use crate::domain::error::DomainError;
use crate::domain::ports::{
    FinalizedTurnRecord, OrphanCandidate, OrphanFinalization, OrphanTurnRepository, QuotaOwner,
    TurnOutbox, storage_now,
};
use crate::domain::quota::PeriodStarts;
use crate::domain::quota_service::{PricingError, QuotaService, SettlementInput};

/// Candidates fetched per scan (fixed, not configurable).
pub const ORPHAN_SCAN_LIMIT: u64 = 100;

/// How the settlement of a finalized orphan went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanSettlement {
    /// Estimated settlement committed `committed_credits_micro`.
    Estimated { committed_credits_micro: i64 },
    /// Reserve columns or `requester_user_id` were NULL: no `quota_usage`
    /// change, a zero usage event.
    Skipped,
    /// The turn can never be priced ([`PricingError::Permanent`]): the
    /// reserve `released_credits_micro` was released from bucket `total`
    /// (and `tier:premium` when the tier was resolved as premium), a zero
    /// usage event.
    ReserveReleased { released_credits_micro: i64 },
}

/// Result of one orphan finalization attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanOutcome {
    /// The CAS won and the transaction committed.
    Finalized(OrphanSettlement),
    /// The turn was no longer orphan-finalizable: nothing was written.
    Skipped,
}

/// Finalizes orphan turns (module docs).
#[derive(Clone)]
pub struct OrphanFinalizer {
    db: Db,
    turns: Arc<dyn OrphanTurnRepository>,
    outbox: Arc<dyn TurnOutbox>,
    quota: Arc<QuotaService>,
    timeout: time::Duration,
}

impl std::fmt::Debug for OrphanFinalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanFinalizer")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl OrphanFinalizer {
    /// Finalizer over the gear database, the orphan turn port, the turn
    /// outbox producer and the quota service; turns without progress for
    /// `timeout_secs` (`orphan_watchdog.timeout_secs`) are orphans.
    #[must_use]
    pub fn new(
        db: Db,
        turns: Arc<dyn OrphanTurnRepository>,
        outbox: Arc<dyn TurnOutbox>,
        quota: Arc<QuotaService>,
        timeout_secs: u64,
    ) -> Self {
        let timeout = time::Duration::seconds(i64::try_from(timeout_secs).unwrap_or(i64::MAX));
        Self {
            db,
            turns,
            outbox,
            quota,
            timeout,
        }
    }

    /// The stale-progress cutoff at `now` (application clock).
    #[must_use]
    pub fn cutoff(&self, now: OffsetDateTime) -> OffsetDateTime {
        now.saturating_sub(self.timeout)
    }

    /// The orphan candidates of a scan at the current time (at most
    /// [`ORPHAN_SCAN_LIMIT`], oldest start first).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure.
    pub async fn candidates(&self) -> Result<Vec<OrphanCandidate>, DomainError> {
        self.turns
            .orphan_candidates(self.cutoff(storage_now()), ORPHAN_SCAN_LIMIT)
            .await
    }

    /// Finalizes `candidate` as an orphan when the CAS still holds at the
    /// current time (module docs).
    ///
    /// # Errors
    ///
    /// The [`DomainError`] of a failed transaction (CAS, transient pricing
    /// failure, settlement write, outbox, commit); the turn then stays
    /// `running` for the next scan. A permanent pricing failure is not an
    /// error: the turn is finalized with its reserve released.
    pub async fn finalize_orphan(
        &self,
        candidate: OrphanCandidate,
    ) -> Result<OrphanOutcome, DomainError> {
        let at = storage_now();
        let finalization = OrphanFinalization {
            turn_id: candidate.id,
            tenant_id: candidate.tenant_id,
            cutoff: self.cutoff(at),
            at,
        };
        let turns = Arc::clone(&self.turns);
        let outbox = Arc::clone(&self.outbox);
        let quota = Arc::clone(&self.quota);
        in_transaction(&self.db, move |tx| {
            Box::pin(async move {
                let unit = OrphanUnit {
                    tx,
                    turns: turns.as_ref(),
                    outbox: outbox.as_ref(),
                    quota: quota.as_ref(),
                };
                unit.execute(finalization).await
            })
        })
        .await
    }
}

/// What the events of a finalized orphan carry about its settlement.
struct Charge {
    /// Effective model (also the selected model); `""` without settlement.
    model: String,
    policy_version_applied: u64,
    billing: BillingDecision,
    committed_credits_micro: i64,
    settlement: OrphanSettlement,
}

/// One orphan finalization transaction.
struct OrphanUnit<'t, 'a> {
    tx: &'t DbTx<'a>,
    turns: &'t dyn OrphanTurnRepository,
    outbox: &'t dyn TurnOutbox,
    quota: &'t QuotaService,
}

impl OrphanUnit<'_, '_> {
    async fn execute(
        &self,
        finalization: OrphanFinalization,
    ) -> Result<(OrphanOutcome, Wake), DomainError> {
        let Some(row) = self
            .turns
            .finalize_orphan_in_tx(self.tx, finalization)
            .await?
        else {
            return Ok((OrphanOutcome::Skipped, Wake::empty()));
        };
        let identity = TurnIdentity {
            tenant_id: row.tenant_id,
            user_id: self.requester(&row).await?,
            chat_id: row.chat_id,
            turn_id: row.id,
            request_id: row.request_id,
        };
        let charge = self.settle(&row).await?;
        let at = finalization.at;
        let usage = turn_usage_event(TurnUsageInput {
            identity,
            selected_model: charge.model.clone(),
            effective_model: charge.model.clone(),
            terminal: TerminalState::Failed,
            billing: charge.billing,
            usage: None,
            actual_credits_micro: charge.committed_credits_micro,
            policy_version_applied: charge.policy_version_applied,
            web_search_calls: row.counts.web_search,
            code_interpreter_calls: row.counts.code_interpreter,
            file_search_calls: row.counts.file_search,
            timestamp: at,
        });
        let audit = AuditEvent::from(turn_audit_event(TurnAuditInput {
            identity,
            selected_model: charge.model.clone(),
            effective_model: charge.model,
            terminal: TerminalState::Failed,
            error_code: Some(ORPHAN_TIMEOUT.to_owned()),
            usage: None,
            latency_ms: latency_ms(row.started_at, at),
            web_search_calls: row.counts.web_search,
            file_search_calls: row.counts.file_search,
            quota_decision: unknown_quota_decision(),
            trace_id: None,
            timestamp: at,
        }));
        let mut wake = self.outbox.enqueue_usage_in_tx(self.tx, &usage).await?;
        match self.outbox.enqueue_audit_in_tx(self.tx, &audit).await {
            Ok(audit_wake) => wake += audit_wake,
            Err(err) => {
                wake.discard();
                return Err(err);
            }
        }
        Ok((OrphanOutcome::Finalized(charge.settlement), wake))
    }

    /// The requesting user, or the chat owner for a row without one.
    async fn requester(&self, row: &FinalizedTurnRecord) -> Result<Uuid, DomainError> {
        if let Some(user_id) = row.requester_user_id {
            return Ok(user_id);
        }
        let owner = self
            .turns
            .chat_owner_in_tx(self.tx, row.tenant_id, row.chat_id)
            .await?;
        Ok(owner.unwrap_or_else(|| {
            tracing::warn!(
                target: "mini_chat.orphan",
                turn_id = %row.id,
                "orphan turn without requester or chat owner; events carry the nil user"
            );
            Uuid::nil()
        }))
    }

    /// The estimated settlement of `row`, or the zero charge when its
    /// reserve columns are incomplete.
    async fn settle(&self, row: &FinalizedTurnRecord) -> Result<Charge, DomainError> {
        let Some(input) = settlement_input(row) else {
            tracing::warn!(
                target: "mini_chat.orphan",
                turn_id = %row.id,
                "orphan turn without reserve fields or requester; settlement skipped"
            );
            return Ok(Charge {
                model: String::new(),
                policy_version_applied: 0,
                billing: derive_billing(TerminalState::Failed, Some(ORPHAN_TIMEOUT), None),
                committed_credits_micro: 0,
                settlement: OrphanSettlement::Skipped,
            });
        };
        let priced = match self.quota.price_settlement(&input).await {
            Ok(priced) => priced,
            Err(PricingError::Transient(err)) => return Err(err),
            Err(PricingError::Permanent { error, tier }) => {
                return self.release(row, input, tier, &error).await;
            }
        };
        let settled = self
            .quota
            .apply_settlement_in_tx(self.tx, &input, priced)
            .await?;
        Ok(Charge {
            model: input.effective_model,
            policy_version_applied: input.policy_version_applied,
            billing: settled.billing,
            committed_credits_micro: settled.committed_credits_micro,
            settlement: OrphanSettlement::Estimated {
                committed_credits_micro: settled.committed_credits_micro,
            },
        })
    }

    /// The zero charge of a turn that can never be priced: its reserve is
    /// released and the finalization commits.
    async fn release(
        &self,
        row: &FinalizedTurnRecord,
        input: SettlementInput,
        tier: Option<ModelTier>,
        cause: &DomainError,
    ) -> Result<Charge, DomainError> {
        tracing::error!(
            target: "mini_chat.orphan",
            turn_id = %row.id,
            effective_model = %input.effective_model,
            policy_version_applied = input.policy_version_applied,
            error = %cause,
            "orphan turn cannot be priced; reserve released without a charge"
        );
        self.quota
            .release_reserve_in_tx(self.tx, &input, tier)
            .await?;
        Ok(Charge {
            model: input.effective_model,
            policy_version_applied: input.policy_version_applied,
            billing: derive_billing(TerminalState::Failed, Some(ORPHAN_TIMEOUT), None),
            committed_credits_micro: 0,
            settlement: OrphanSettlement::ReserveReleased {
                released_credits_micro: input.reserved_credits_micro,
            },
        })
    }
}

/// The settlement of an orphan row from its persisted preflight columns;
/// `None` when any of them or `requester_user_id` is NULL (or out of range).
#[must_use]
pub fn settlement_input(row: &FinalizedTurnRecord) -> Option<SettlementInput> {
    Some(SettlementInput {
        owner: QuotaOwner {
            tenant_id: row.tenant_id,
            user_id: row.requester_user_id?,
        },
        effective_model: row.effective_model.clone()?,
        policy_version_applied: row
            .policy_version_applied
            .and_then(|version| u64::try_from(version).ok())?,
        reserve_tokens: row.reserve_tokens?,
        max_output_tokens_applied: i64::from(row.max_output_tokens_applied?),
        reserved_credits_micro: row.reserved_credits_micro?,
        minimal_generation_floor_applied: i64::from(row.minimal_generation_floor_applied?),
        periods: PeriodStarts::at(row.started_at),
        terminal: TerminalState::Failed,
        error_code: Some(ORPHAN_TIMEOUT.to_owned()),
        usage: None,
        web_search_calls: row.counts.web_search,
        code_interpreter_calls: row.counts.code_interpreter,
    })
}

fn latency_ms(started_at: OffsetDateTime, now: OffsetDateTime) -> u64 {
    u64::try_from((now - started_at).whole_milliseconds()).unwrap_or(0)
}

#[cfg(test)]
#[path = "orphan_tests.rs"]
pub(crate) mod tests;
