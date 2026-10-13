//! Ports of the quota service: the repository the infrastructure implements
//! and the metrics sink.

use async_trait::async_trait;
use mini_chat_sdk::ModelTier;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::finalize::{FinalizeRequest, FinalizeResult};
use super::periods::PeriodKeys;
use super::reserve::{ReserveRequest, ReservedColumns};
use super::types::{BucketRow, QuotaPeriod};
use crate::domain::error::DomainError;

/// What the preflight reads for one request.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreflightRead {
    /// Tenant of the chat and of the buckets.
    pub tenant_id: Uuid,
    /// Requesting user (owner of the buckets).
    pub user_id: Uuid,
    /// The already authorized chat.
    pub chat_id: Uuid,
    /// The period starts to read.
    pub keys: PeriodKeys,
}

/// Result of the preflight read.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRows {
    /// `input_tokens + output_tokens` of the chat's latest non-deleted
    /// assistant message with non-zero counts; 0 when none exists.
    pub prior_context_tokens: i64,
    /// The existing rows of buckets `total` and `tier:premium` for both
    /// period starts, in the canonical order (daily before monthly, then
    /// `total` before `tier:premium`); a missing row is absent.
    pub buckets: Vec<BucketRow>,
}

/// Persistence of the quota service.
///
/// The two operations that take a caller transaction are inherent methods of
/// the infrastructure implementation (`reserve_with` and `finalize_turn_with`
/// of `SeaQuotaRepository`): their closure runs the caller's extra steps in the
/// same transaction. The methods below run their own transaction.
#[async_trait]
pub trait QuotaRepository: Send + Sync {
    /// Preflight read: the prior context tokens and the four bucket rows in one
    /// read transaction that never writes.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn read_preflight(&self, read: &PreflightRead) -> Result<PreflightRows, DomainError>;

    /// The `total` and `tier:premium` rows of `scope` (tenant and owner
    /// predicates) for both period starts, in the canonical order.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn read_buckets(
        &self,
        scope: &AccessScope,
        keys: PeriodKeys,
    ) -> Result<Vec<BucketRow>, DomainError>;

    /// Reserve write and limit re-check
    /// (`cpt-cf-mini-chat-algo-quota-billing-reserve-write`) in its own
    /// transaction, with the bounded conflict retry.
    ///
    /// # Errors
    ///
    /// `QuotaExceeded(Tokens)` when a bucket is over its limit at the
    /// re-check (nothing is written); [`DomainError::Internal`] when a fill
    /// updates no `chat_turns` row; [`DomainError::Database`] for a database
    /// failure or when the conflict retry budget runs out.
    async fn reserve(&self, request: &ReserveRequest<'_>) -> Result<ReservedColumns, DomainError>;

    /// CAS-guarded finalization
    /// (`cpt-cf-mini-chat-algo-quota-billing-finalize-turn`) in its own
    /// transaction, with the bounded conflict retry.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`] / [`DomainError::Internal`]; the transaction is
    /// rolled back and the turn stays `running`.
    async fn finalize_turn(&self, request: &FinalizeRequest)
    -> Result<FinalizeResult, DomainError>;
}

/// Value of the `decision` label of `mini_chat_quota_preflight`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightLabel {
    /// The selected model runs.
    Allow,
    /// Another model runs.
    Downgrade,
    /// No tier is available.
    Reject,
}

impl PreflightLabel {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
            Self::Reject => "reject",
        }
    }
}

/// The billing group of the cost metrics
/// (`cpt-cf-mini-chat-algo-quota-billing-record-metrics`). Only the labels
/// `decision`, `model`, `tier`, `period` exist.
pub trait QuotaMetrics: Send + Sync {
    /// `mini_chat_quota_preflight{decision,model,tier}`.
    fn record_preflight(&self, decision: PreflightLabel, model: &str, tier: ModelTier);
    /// `mini_chat_quota_estimated_tokens`.
    fn record_estimated_tokens(&self, tokens: i64);
    /// `mini_chat_quota_reserve{period}`.
    fn record_reserve(&self, period: QuotaPeriod);
    /// `mini_chat_quota_commit{period}` (actual settlements only).
    fn record_commit(&self, period: QuotaPeriod);
    /// `mini_chat_quota_actual_tokens` (actual settlements only).
    fn record_actual_tokens(&self, tokens: i64);
    /// `mini_chat_quota_overshoot{period}`.
    fn record_overshoot(&self, period: QuotaPeriod);
    /// `mini_chat_code_interpreter_calls{model}` (actual settlements only).
    fn record_code_interpreter_calls(&self, model: &str, calls: u64);
}
