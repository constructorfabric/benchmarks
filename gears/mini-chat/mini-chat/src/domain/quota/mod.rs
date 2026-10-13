//! Quota service: preflight cascade, reserve, settlement, warnings and status (DESIGN §5).
//!
//! CONTRACT (implemented by the quota work package; the public types and signatures below are
//! used by the streaming / turn services and must not change).

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UsageEvent, UsageTokens, UserLimits};
use serde::Serialize;
use time::{Date, OffsetDateTime};
use toolkit_db::DbTx;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::chat_turn;

mod arith;
mod buckets;
mod preflight;
mod settle;
pub mod status;

pub use arith::{MAX_MULT, MAX_TOKENS, next_daily_reset, next_monthly_reset};
pub use preflight::{REASON_DISABLE_PREMIUM, REASON_FORCE_STANDARD, REASON_MODEL_DISABLED, REASON_PREMIUM_EXHAUSTED};
pub use settle::dedupe_key;
pub use status::{PeriodStatus, QuotaStatus, TierStatus, quota_status};

#[cfg(test)]
mod tests;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";

/// Tool-related inputs of the preflight.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolInputs {
    /// Chat has at least one ready, non-deleted `for_file_search` attachment.
    pub has_ready_documents: bool,
    /// Chat has at least one ready, non-deleted `for_code_interpreter` attachment.
    pub has_ready_code_interpreter: bool,
    /// Request has `web_search.enabled = true`.
    pub web_search_requested: bool,
}

/// Preflight request.
#[derive(Debug, Clone)]
pub struct PreflightRequest {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// `chats.model` (may be disabled or missing from the catalog).
    pub selected_model: String,
    /// UTF-8 bytes of the current user message.
    pub message_bytes: usize,
    /// Images on the current message.
    pub image_count: u32,
    /// `input_tokens + output_tokens` of the latest non-deleted assistant message with usage.
    pub prior_context_tokens: i64,
    pub tools: ToolInputs,
    pub now: OffsetDateTime,
}

/// Tools that will be sent with the effective model.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnabledTools {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

impl QuotaDecisionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// UTC period starts computed at preflight (and from `started_at` by the watchdog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

/// Preflight decision: everything needed to book the reserve and run the turn.
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub snapshot: Arc<PolicySnapshot>,
    pub limits: UserLimits,
    pub selected_model: String,
    pub effective_model: ModelCatalogEntry,
    pub effective_tier: ModelTier,
    pub decision: QuotaDecisionKind,
    /// `premium_quota_exhausted` | `force_standard_tier` | `disable_premium_tier` | `model_disabled`
    pub downgrade_reason: Option<String>,
    pub tools: EnabledTools,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    /// `min(config floor, max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: i64,
    pub policy_version: i64,
    pub periods: PeriodStarts,
}

/// Billing outcome (DESIGN §5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    Actual,
    Estimated,
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Input of a settlement (in the CAS-winning finalization transaction).
#[derive(Debug, Clone)]
pub struct SettlementInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// The turn row as it was before the terminal CAS (reserve columns, effective model, policy version).
    pub turn: chat_turn::Model,
    pub billing_outcome: BillingOutcome,
    pub method: SettlementMethod,
    /// Provider usage for the `actual` method.
    pub usage: Option<UsageTokens>,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub periods: PeriodStarts,
}

/// Settlement result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub method: SettlementMethod,
    pub billing_outcome: BillingOutcome,
    /// Committed credits (capped at the reserve when the overshoot exceeds the tolerance).
    pub actual_credits_micro: i64,
    pub overshoot_capped: bool,
}

/// Per-tier, per-period warning entry (`done.quota_warnings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaWarning {
    /// `premium` | `total`
    pub tier: String,
    /// `daily` | `monthly`
    pub period: String,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none", with = "time::serde::rfc3339::option")]
    pub next_reset: Option<OffsetDateTime>,
}

/// UTC period starts of an instant.
#[must_use]
pub fn period_starts(now: OffsetDateTime) -> PeriodStarts {
    arith::period_starts(now)
}

/// Canonical credit formula with checked arithmetic (DESIGN §5.3).
///
/// # Errors
/// Returns a description when a bound is violated or the arithmetic overflows.
pub fn credits_micro(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> Result<i64, String> {
    arith::credits_micro(input_tokens, output_tokens, in_mult, out_mult)
}

/// `estimated_text_tokens` of a message for a model's budgets (DESIGN §5.5.4).
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, budgets: &mini_chat_sdk::EstimationBudgets) -> i64 {
    arith::estimate_text_tokens(utf8_bytes, budgets)
}

/// Preflight: kill switch for web search (400 `FEATURE_DISABLED`, before the cascade), cascade with
/// per-candidate reserve checks over `total` / `tier:premium` daily + monthly rows, daily web
/// search / code interpreter quota checks (429 `web_search` / `code_interpreter`), all-exhausted
/// → 429 `tokens`. Reads `quota_usage` in one transaction and writes nothing.
///
/// # Errors
/// `DomainError` as described.
pub async fn preflight(app: &AppServices, req: &PreflightRequest) -> Result<PreflightDecision, DomainError> {
    preflight::preflight(app, req).await
}

/// Books the reserve inside `tx` (creating missing bucket rows), then re-reads the rows and fails
/// with 429 `tokens` when any bucket of the decision is over its limit (the caller's transaction
/// then rolls back).
///
/// # Errors
/// `DomainError::ResourceExhausted` or DB errors.
pub async fn reserve(tx: &DbTx<'_>, tenant_id: Uuid, user_id: Uuid, decision: &PreflightDecision) -> Result<(), DomainError> {
    preflight::reserve(tx, tenant_id, user_id, decision).await
}

/// Billing outcome and settlement method of a terminal condition (DESIGN §5.8 mapping table).
/// `usage_known` = provider reported usage with a non-zero count (failed turns) / any usage
/// object (completed turns).
#[must_use]
pub fn derive_billing(state: &str, error_code: Option<&str>, usage: Option<&UsageTokens>) -> (BillingOutcome, SettlementMethod) {
    arith::derive_billing(state, error_code, usage)
}

/// Settles a turn inside the finalization transaction: releases the turn's reserve and commits
/// actual / estimated / released credits on the reserved rows (snapshot of
/// `policy_version_applied`). Skips row updates (returns zero credits) when the turn has no reserve.
///
/// # Errors
/// DB errors or a credit computation failure (finalization then fails).
pub async fn settle(app: &AppServices, tx: &DbTx<'_>, input: &SettlementInput) -> Result<Settlement, DomainError> {
    settle::settle(app, tx, input).await
}

/// Builds the usage outbox payload of a settled turn (`dedupe_key = tenant/turn/request`, simple hex).
#[must_use]
pub fn usage_event(
    input: &SettlementInput,
    settlement: &Settlement,
    selected_model: &str,
    file_search_calls: u32,
    terminal_state: &str,
    now: OffsetDateTime,
) -> UsageEvent {
    settle::usage_event(input, settlement, selected_model, file_search_calls, terminal_state, now)
}

/// Enqueues the usage event in `tx` (partitioned by tenant).
///
/// # Errors
/// Outbox errors.
pub async fn enqueue_usage(app: &AppServices, tx: &DbTx<'_>, event: &UsageEvent) -> Result<toolkit_db::outbox::Wake, DomainError> {
    settle::enqueue_usage(app, tx, event).await
}

/// Quota warnings for the user after settlement (reads rows inside `tx`).
///
/// # Errors
/// DB errors.
pub async fn quota_warnings(
    app: &AppServices,
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    user_id: Uuid,
    limits: &UserLimits,
    now: OffsetDateTime,
) -> Result<Vec<QuotaWarning>, DomainError> {
    let rows = buckets::load_rows(tx, tenant_id, user_id, period_starts(now)).await?;
    let tiers = status::compute(&rows, limits, now, app.cfg.quota.warning_threshold_pct);
    Ok(status::warnings_of(&tiers))
}
