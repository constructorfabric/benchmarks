//! Reserve write contract
//! (`cpt-cf-mini-chat-algo-quota-billing-reserve-write`,
//! `cpt-cf-mini-chat-dod-quota-billing-reserve-write`): the request and
//! result types, the canonical write order and the in-transaction re-check.

use std::time::Duration;

use mini_chat_sdk::{ModelTier, UserLimits};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::preflight::PreflightDecision;
use super::types::{BucketRow, QuotaBucket, limit_of};

/// Attempts of one reserve or finalization transaction on a lock or
/// serialization conflict (the first try included).
pub const MAX_TX_ATTEMPTS: u32 = 3;

/// Pauses between the attempts: after the first and after the second.
pub const CONFLICT_BACKOFF: [Duration; 2] = [Duration::from_millis(20), Duration::from_millis(100)];

/// The reserve flavour.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveMode {
    /// Send path: the caller writes the six values in its own `chat_turns`
    /// INSERT of the same transaction.
    Send,
    /// Retry and edit path: the six values are written once into the new turn
    /// (`state` `running` and `reserve_tokens IS NULL`).
    Fill {
        /// The new turn.
        turn_id: Uuid,
    },
}

/// What the reserve write needs.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct ReserveRequest<'a> {
    /// Tenant of the turn (tenant predicate of every statement).
    pub tenant_id: Uuid,
    /// Requesting user (owner predicate of every statement).
    pub user_id: Uuid,
    /// Send or fill.
    pub mode: ReserveMode,
    /// The preflight decision (reserve values, tier, limits, period starts).
    pub decision: &'a PreflightDecision,
    /// Current time (`quota_usage.updated_at`).
    pub now: OffsetDateTime,
}

/// The six preflight column values of `chat_turns`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedColumns {
    /// `reserve_tokens`.
    pub reserve_tokens: i64,
    /// `max_output_tokens_applied`.
    pub max_output_tokens_applied: i64,
    /// `reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// `policy_version_applied`.
    pub policy_version_applied: u64,
    /// `effective_model`.
    pub effective_model: String,
    /// `minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i64,
}

impl ReservedColumns {
    /// The six values of a preflight decision.
    #[must_use]
    pub fn from_decision(decision: &PreflightDecision) -> Self {
        Self {
            reserve_tokens: decision.reserve.reserve_tokens,
            max_output_tokens_applied: decision.reserve.max_output_tokens_applied,
            reserved_credits_micro: decision.reserve.reserved_credits_micro,
            policy_version_applied: decision.policy_version,
            effective_model: decision.effective_model.clone(),
            minimal_generation_floor_applied: decision.reserve.minimal_generation_floor_applied,
        }
    }
}

/// The buckets a turn of `tier` reserves into, per period, in write order:
/// `total`, then `tier:premium` for a premium turn only.
#[must_use]
pub fn reserve_buckets(tier: ModelTier) -> &'static [QuotaBucket] {
    match tier {
        ModelTier::Premium => &[QuotaBucket::Total, QuotaBucket::TierPremium],
        ModelTier::Standard => &[QuotaBucket::Total],
    }
}

/// The limit re-check over the re-read rows (step 5): `true` when any row has
/// `spent_credits_micro + reserved_credits_micro` greater than the limit of its
/// bucket and period (an overflowing sum counts as over the limit).
#[must_use]
pub fn reserve_exceeds_limit(rows: &[BucketRow], limits: &UserLimits) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-for-each-recheck
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-recheck-over
    rows.iter().any(|row| {
        let limit = limit_of(limits, row.bucket, row.period);
        row.spent_credits_micro
            .checked_add(row.reserved_credits_micro)
            .is_none_or(|used| used > limit)
    })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-recheck-over
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-for-each-recheck
}

#[cfg(test)]
#[path = "reserve_tests.rs"]
mod tests;
