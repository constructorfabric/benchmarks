//! Quota service (DESIGN §3.2, §5): preflight cascade premium -> standard,
//! reserve with re-check, settlement, warnings and status.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UsageTokens, UserLimits};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use super::clock;
use super::credits::credits_micro;
use super::error::{DomainError, QuotaScope};
use super::estimation::estimate_text_tokens;
use super::models::QuotaWarning;
use crate::config::QuotaConfig;
use crate::infra::db::entities::quota_usage;
use crate::infra::db::repo::quota::{self as repo, BucketDelta, BucketKey, bucket, period};

/// Inputs of the preflight that do not depend on the candidate model.
#[allow(clippy::struct_excessive_bools)] // independent request flags
#[derive(Debug, Clone)]
pub struct PreflightRequest<'a> {
    pub selected_model: &'a str,
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub image_count: u32,
    pub has_ready_documents: bool,
    pub has_ready_code_interpreter: bool,
    pub web_search_requested: bool,
}

/// Tool gates for the effective model.
#[allow(clippy::struct_excessive_bools)] // one independent gate per tool
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolGates {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Preflight decision (effective model, reserve and tool gates).
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub effective: ModelCatalogEntry,
    pub selected_model: String,
    pub downgrade: bool,
    pub downgrade_reason: Option<&'static str>,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub policy_version: u64,
    pub daily_start: NaiveDate,
    pub monthly_start: NaiveDate,
    pub gates: ToolGates,
}

impl PreflightDecision {
    #[must_use]
    pub fn is_premium(&self) -> bool {
        self.effective.tier == ModelTier::Premium
    }

    /// `allow` / `downgrade`.
    #[must_use]
    pub const fn decision_str(&self) -> &'static str {
        if self.downgrade { "downgrade" } else { "allow" }
    }
}

/// Bucket usage snapshot of one `(bucket, period)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Usage of the four buckets for the current periods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageView {
    pub total_daily: BucketUsage,
    pub total_monthly: BucketUsage,
    pub premium_daily: BucketUsage,
    pub premium_monthly: BucketUsage,
}

impl UsageView {
    #[must_use]
    pub fn from_rows(rows: &[quota_usage::Model], daily: NaiveDate, monthly: NaiveDate) -> Self {
        let mut v = Self::default();
        for r in rows {
            let u = BucketUsage {
                spent: r.spent_credits_micro,
                reserved: r.reserved_credits_micro,
                web_search_calls: i64::from(r.web_search_calls),
                code_interpreter_calls: i64::from(r.code_interpreter_calls),
            };
            match (r.bucket.as_str(), r.period_type.as_str()) {
                (bucket::TOTAL, period::DAILY) if r.period_start == daily => v.total_daily = u,
                (bucket::TOTAL, period::MONTHLY) if r.period_start == monthly => v.total_monthly = u,
                (bucket::PREMIUM, period::DAILY) if r.period_start == daily => v.premium_daily = u,
                (bucket::PREMIUM, period::MONTHLY) if r.period_start == monthly => v.premium_monthly = u,
                _ => {}
            }
        }
        v
    }
}

/// Candidate reserve of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub gates: ToolGates,
}

/// Computes the reserve a candidate would book (DESIGN §5.4.1).
#[must_use]
pub fn candidate_reserve(
    m: &ModelCatalogEntry,
    req: &PreflightRequest<'_>,
    snapshot: &PolicySnapshot,
    max_output_cap: u32,
) -> CandidateReserve {
    let ks = &snapshot.kill_switches;
    let ts = &m.general_config.tool_support;
    let gates = ToolGates {
        file_search: req.has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: req.web_search_requested && ts.web_search,
        code_interpreter: req.has_ready_code_interpreter && ts.code_interpreter && !ks.disable_code_interpreter,
    };
    let b = &m.estimation_budgets;
    let mut est = estimate_text_tokens_bytes(req.message_bytes, b) + req.prior_context_tokens.max(0);
    est += i64::from(req.image_count) * i64::from(b.image_token_budget);
    if gates.file_search {
        est += i64::from(b.tool_surcharge_tokens);
    }
    if gates.web_search {
        est += i64::from(b.web_search_surcharge_tokens);
    }
    if gates.code_interpreter {
        est += i64::from(b.code_interpreter_surcharge_tokens);
    }
    let max_out = i64::from(m.max_output_tokens.min(max_output_cap));
    let credits = credits_micro(
        est,
        max_out,
        m.input_tokens_credit_multiplier_micro,
        m.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(model = %m.id, error = %e, "candidate reserve cannot be computed");
        i64::MAX
    });
    CandidateReserve {
        estimated_input_tokens: est,
        max_output_tokens_applied: max_out,
        reserved_credits_micro: credits,
        gates,
    }
}

fn estimate_text_tokens_bytes(bytes: usize, b: &mini_chat_sdk::EstimationBudgets) -> i64 {
    super::estimation::estimate_bytes_tokens(bytes, b)
}

fn fits(u: BucketUsage, this: i64, limit: i64) -> bool {
    u.spent
        .saturating_add(u.reserved)
        .saturating_add(this)
        <= limit
}

/// Whether a tier can take a reserve of `this` credits in both periods.
#[must_use]
pub fn tier_available(tier: ModelTier, usage: &UsageView, limits: &UserLimits, this: i64) -> bool {
    let total_ok = fits(usage.total_daily, this, limits.standard.limit_daily_credits_micro)
        && fits(usage.total_monthly, this, limits.standard.limit_monthly_credits_micro);
    match tier {
        ModelTier::Standard => total_ok,
        ModelTier::Premium => {
            total_ok
                && fits(usage.premium_daily, this, limits.premium.limit_daily_credits_micro)
                && fits(usage.premium_monthly, this, limits.premium.limit_monthly_credits_micro)
        }
    }
}

/// Result of the cascade.
#[derive(Debug, Clone)]
pub struct CascadeResult {
    pub effective: ModelCatalogEntry,
    pub reserve: CandidateReserve,
    pub downgrade: bool,
    pub reason: Option<&'static str>,
}

fn candidate_for<'a>(snapshot: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snapshot.model_catalog.iter().filter(|m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Deterministic downgrade cascade (DESIGN "Downgrade Decision Flow").
#[must_use]
pub fn run_cascade(
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    usage: &UsageView,
    req: &PreflightRequest<'_>,
    max_output_cap: u32,
) -> Option<CascadeResult> {
    let (start, mut reason) = match snapshot.find(req.selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled")),
        None => (ModelTier::Premium, Some("model_disabled")),
    };
    let cascade: &[ModelTier] = match start {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    let ks = &snapshot.kill_switches;
    for tier in cascade {
        if *tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
            if reason.is_none() {
                reason = Some(if ks.force_standard_tier {
                    "force_standard_tier"
                } else {
                    "disable_premium_tier"
                });
            }
            continue;
        }
        let Some(candidate) = candidate_for(snapshot, *tier, req.selected_model) else {
            continue;
        };
        let reserve = candidate_reserve(candidate, req, snapshot, max_output_cap);
        if tier_available(*tier, usage, limits, reserve.reserved_credits_micro) {
            let downgrade = candidate.id != req.selected_model || reason.is_some();
            return Some(CascadeResult {
                effective: candidate.clone(),
                reserve,
                downgrade,
                reason: if downgrade { reason } else { None },
            });
        }
        if *tier == ModelTier::Premium && reason.is_none() {
            reason = Some("premium_quota_exhausted");
        }
    }
    None
}

/// Settlement method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    Actual(UsageTokens),
    Estimated,
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Actual(_) => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Persisted reserve of a turn (from `chat_turns`).
#[derive(Debug, Clone)]
pub struct TurnReserve {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub premium: bool,
    pub in_mult: i64,
    pub out_mult: i64,
    pub daily_start: NaiveDate,
    pub monthly_start: NaiveDate,
}

/// Settlement result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementResult {
    pub committed_credits_micro: i64,
    pub overshoot_capped: bool,
}

/// Committed credits for a settlement method (pure).
///
/// # Errors
/// Credit computation failure.
pub fn committed_credits(
    r: &TurnReserve,
    method: SettlementMethod,
    tolerance: f64,
) -> Result<SettlementResult, DomainError> {
    match method {
        SettlementMethod::Released => Ok(SettlementResult {
            committed_credits_micro: 0,
            overshoot_capped: false,
        }),
        SettlementMethod::Estimated => {
            let est_in = (r.reserve_tokens - r.max_output_tokens_applied).max(0);
            let c = credits_micro(est_in, r.minimal_generation_floor_applied, r.in_mult, r.out_mult)
                .map_err(|e| DomainError::Internal(format!("settlement: {e}")))?;
            Ok(SettlementResult {
                committed_credits_micro: c,
                overshoot_capped: false,
            })
        }
        SettlementMethod::Actual(u) => {
            let actual = credits_micro(u.input_tokens, u.output_tokens, r.in_mult, r.out_mult)
                .map_err(|e| DomainError::Internal(format!("settlement: {e}")))?;
            let actual_tokens = u.input_tokens + u.output_tokens;
            #[allow(clippy::cast_precision_loss)]
            let capped = actual_tokens > r.reserve_tokens
                && r.reserve_tokens > 0
                && (actual_tokens as f64) / (r.reserve_tokens as f64) > tolerance;
            Ok(SettlementResult {
                committed_credits_micro: if capped { r.reserved_credits_micro } else { actual },
                overshoot_capped: capped,
            })
        }
    }
}

/// One period entry of the quota status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: &'static str,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    pub next_reset: DateTime<Utc>,
    pub warning: bool,
    pub exhausted: bool,
}

/// One tier entry of the quota status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    pub tier: &'static str,
    pub periods: Vec<PeriodStatus>,
}

#[allow(clippy::integer_division)] // floored remaining percentage is intended
fn period_status(
    period: &'static str,
    limit: i64,
    u: BucketUsage,
    next_reset: DateTime<Utc>,
    threshold: u8,
) -> Option<PeriodStatus> {
    if limit <= 0 {
        return None;
    }
    let used = u.spent.saturating_add(u.reserved).max(0);
    let remaining = (limit - used).max(0);
    let pct = u32::try_from(i128::from(remaining) * 100 / i128::from(limit)).unwrap_or(0);
    Some(PeriodStatus {
        period,
        limit_credits_micro: limit,
        used_credits_micro: used,
        remaining_credits_micro: remaining,
        remaining_percentage: pct,
        next_reset,
        warning: pct <= 100 - u32::from(threshold),
        exhausted: pct == 0,
    })
}

/// Quota status of a user (tiers `premium`, `total`).
#[must_use]
pub fn quota_status(
    usage: &UsageView,
    limits: &UserLimits,
    daily: NaiveDate,
    monthly: NaiveDate,
    threshold: u8,
) -> Vec<TierStatus> {
    let nd = clock::next_daily_reset(daily);
    let nm = clock::next_monthly_reset(monthly);
    let premium: Vec<PeriodStatus> = [
        period_status(period::DAILY, limits.premium.limit_daily_credits_micro, usage.premium_daily, nd, threshold),
        period_status(period::MONTHLY, limits.premium.limit_monthly_credits_micro, usage.premium_monthly, nm, threshold),
    ]
    .into_iter()
    .flatten()
    .collect();
    let total: Vec<PeriodStatus> = [
        period_status(period::DAILY, limits.standard.limit_daily_credits_micro, usage.total_daily, nd, threshold),
        period_status(period::MONTHLY, limits.standard.limit_monthly_credits_micro, usage.total_monthly, nm, threshold),
    ]
    .into_iter()
    .flatten()
    .collect();
    vec![
        TierStatus {
            tier: "premium",
            periods: premium,
        },
        TierStatus {
            tier: "total",
            periods: total,
        },
    ]
}

/// Flattens a status into `done.quota_warnings` entries.
#[must_use]
pub fn warnings_from_status(status: &[TierStatus]) -> Vec<QuotaWarning> {
    status
        .iter()
        .flat_map(|t| {
            t.periods.iter().map(move |p| QuotaWarning {
                tier: t.tier,
                period: p.period,
                remaining_percentage: p.remaining_percentage,
                warning: p.warning,
                exhausted: p.exhausted,
                next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
            })
        })
        .collect()
}

/// Quota service.
pub struct QuotaService {
    cfg: QuotaConfig,
    max_output_cap: u32,
    floor: u32,
}

impl QuotaService {
    #[must_use]
    pub fn new(cfg: QuotaConfig, max_output_cap: u32, floor: u32) -> Self {
        Self {
            cfg,
            max_output_cap,
            floor,
        }
    }

    #[must_use]
    pub fn config(&self) -> &QuotaConfig {
        &self.cfg
    }

    /// Current usage view of a user.
    ///
    /// # Errors
    /// Database errors.
    pub async fn usage_view(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        daily: NaiveDate,
        monthly: NaiveDate,
    ) -> Result<UsageView, DomainError> {
        let rows = repo::rows_for_periods(runner, tenant_id, user_id, daily, monthly).await?;
        Ok(UsageView::from_rows(&rows, daily, monthly))
    }

    /// Preflight: cascade and daily tool quotas.
    ///
    /// # Errors
    /// `QuotaExceeded`.
    #[allow(clippy::too_many_arguments)]
    pub async fn preflight(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        snapshot: &PolicySnapshot,
        limits: &UserLimits,
        req: &PreflightRequest<'_>,
        now: DateTime<Utc>,
    ) -> Result<PreflightDecision, DomainError> {
        let daily = clock::day_start(now);
        let monthly = clock::month_start(now);
        let usage = self.usage_view(runner, tenant_id, user_id, daily, monthly).await?;
        let Some(c) = run_cascade(snapshot, limits, &usage, req, self.max_output_cap) else {
            return Err(DomainError::QuotaExceeded(QuotaScope::Tokens));
        };
        if c.reserve.gates.web_search
            && usage.total_daily.web_search_calls >= i64::from(self.cfg.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::WebSearch));
        }
        if c.reserve.gates.code_interpreter
            && usage.total_daily.code_interpreter_calls >= i64::from(self.cfg.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter));
        }
        let floor = i64::from(self.floor).min(c.reserve.max_output_tokens_applied);
        Ok(PreflightDecision {
            effective: c.effective,
            selected_model: req.selected_model.to_owned(),
            downgrade: c.downgrade,
            downgrade_reason: c.reason,
            estimated_input_tokens: c.reserve.estimated_input_tokens,
            max_output_tokens_applied: c.reserve.max_output_tokens_applied,
            reserve_tokens: c.reserve.estimated_input_tokens + c.reserve.max_output_tokens_applied,
            reserved_credits_micro: c.reserve.reserved_credits_micro,
            minimal_generation_floor_applied: floor,
            policy_version: snapshot.policy_version,
            daily_start: daily,
            monthly_start: monthly,
            gates: c.reserve.gates,
        })
    }

    fn keys(premium: bool, daily: NaiveDate, monthly: NaiveDate) -> Vec<BucketKey> {
        let mut keys = vec![
            BucketKey {
                period_type: period::DAILY,
                period_start: daily,
                bucket: bucket::TOTAL,
            },
            BucketKey {
                period_type: period::MONTHLY,
                period_start: monthly,
                bucket: bucket::TOTAL,
            },
        ];
        if premium {
            keys.push(BucketKey {
                period_type: period::DAILY,
                period_start: daily,
                bucket: bucket::PREMIUM,
            });
            keys.push(BucketKey {
                period_type: period::MONTHLY,
                period_start: monthly,
                bucket: bucket::PREMIUM,
            });
        }
        keys
    }

    /// Writes the reserve and re-checks the limits (same transaction).
    ///
    /// # Errors
    /// `QuotaExceeded(Tokens)` when a bucket is over its limit.
    pub async fn reserve(
        &self,
        tx: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        d: &PreflightDecision,
        limits: &UserLimits,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        for key in Self::keys(d.is_premium(), d.daily_start, d.monthly_start) {
            repo::ensure_row(tx, tenant_id, user_id, key, now).await?;
            repo::apply_delta(
                tx,
                tenant_id,
                user_id,
                key,
                BucketDelta {
                    reserved: d.reserved_credits_micro,
                    ..BucketDelta::default()
                },
                now,
            )
            .await?;
        }
        let usage = self
            .usage_view(tx, tenant_id, user_id, d.daily_start, d.monthly_start)
            .await?;
        let tier = if d.is_premium() {
            ModelTier::Premium
        } else {
            ModelTier::Standard
        };
        if !tier_available(tier, &usage, limits, 0) {
            return Err(DomainError::QuotaExceeded(QuotaScope::Tokens));
        }
        Ok(())
    }

    /// Settles a turn's reserve (same transaction as the CAS and outbox).
    ///
    /// # Errors
    /// Credit computation or database failure.
    pub async fn settle(
        &self,
        tx: &impl DBRunner,
        r: &TurnReserve,
        method: SettlementMethod,
        web_search_calls: i32,
        code_interpreter_calls: i32,
        now: DateTime<Utc>,
    ) -> Result<SettlementResult, DomainError> {
        let res = committed_credits(r, method, self.cfg.overshoot_tolerance_factor)?;
        let (in_t, out_t) = match method {
            SettlementMethod::Actual(u) => (u.input_tokens, u.output_tokens),
            _ => (0, 0),
        };
        let (ws, ci) = match method {
            SettlementMethod::Released => (0, 0),
            _ => (web_search_calls, code_interpreter_calls),
        };
        for key in Self::keys(r.premium, r.daily_start, r.monthly_start) {
            repo::ensure_row(tx, r.tenant_id, r.user_id, key, now).await?;
            let total = key.bucket == bucket::TOTAL;
            repo::apply_delta(
                tx,
                r.tenant_id,
                r.user_id,
                key,
                BucketDelta {
                    reserved: -r.reserved_credits_micro,
                    spent: res.committed_credits_micro,
                    calls: 1,
                    input_tokens: if total { in_t } else { 0 },
                    output_tokens: if total { out_t } else { 0 },
                    web_search_calls: if total { ws } else { 0 },
                    code_interpreter_calls: if total { ci } else { 0 },
                },
                now,
            )
            .await?;
        }
        Ok(res)
    }

    /// Quota status for a user.
    ///
    /// # Errors
    /// Database errors.
    pub async fn status(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
        now: DateTime<Utc>,
    ) -> Result<Vec<TierStatus>, DomainError> {
        let daily = clock::day_start(now);
        let monthly = clock::month_start(now);
        let usage = self.usage_view(runner, tenant_id, user_id, daily, monthly).await?;
        Ok(quota_status(&usage, limits, daily, monthly, self.cfg.warning_threshold_pct))
    }
}

/// Re-exported for estimation of the current message.
#[must_use]
pub fn message_tokens(text: &str, m: &ModelCatalogEntry) -> i64 {
    estimate_text_tokens(text, &m.estimation_budgets)
}

/// Shared handle type.
pub type QuotaServiceRef = Arc<QuotaService>;

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
