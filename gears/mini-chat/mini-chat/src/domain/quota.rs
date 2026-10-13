//! Quota service: credit arithmetic, preflight estimation, downgrade cascade,
//! reserve, settlement and status (DESIGN §5).

use std::collections::HashMap;

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::{Date, Month, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::error::{DomainError, quota_scope};
use crate::infra::db::entity::quota_usage;

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("invalid token count")]
    InvalidTokenCount,
    #[error("zero multiplier")]
    ZeroMultiplier,
    #[error("multiplier too large")]
    MultiplierTooLarge,
    #[error("arithmetic overflow")]
    Overflow,
}

fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// Canonical credit formula with per-component ceil division.
///
/// # Errors
/// Out-of-range token counts or multipliers, or overflow.
pub fn credits_micro(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> Result<i64, CreditError> {
    if !(0..=MAX_TOKENS).contains(&input_tokens) || !(0..=MAX_TOKENS).contains(&output_tokens) {
        return Err(CreditError::InvalidTokenCount);
    }
    for m in [in_mult, out_mult] {
        if m <= 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if m > MAX_MULT {
            return Err(CreditError::MultiplierTooLarge);
        }
    }
    let a = input_tokens.checked_mul(in_mult).ok_or(CreditError::Overflow)?;
    let b = output_tokens.checked_mul(out_mult).ok_or(CreditError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditError::Overflow)
}

/// Multipliers of a catalog entry as i64.
#[must_use]
pub fn multipliers(m: &ModelCatalogEntry) -> (i64, i64) {
    (
        i64::try_from(m.input_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
        i64::try_from(m.output_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
    )
}

/// Conservative token estimate of a text: `ceil((ceil(bytes/bpt) + overhead) * (100+margin)/100)`.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX / 4);
    let base = ceil_div(bytes, bpt) + i64::from(b.fixed_overhead_tokens);
    ceil_div(base * (100 + i64::from(b.safety_margin_pct)), 100)
}

/// Token estimate without the fixed overhead (used for context items).
#[must_use]
pub fn estimate_item_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX / 4);
    ceil_div(ceil_div(bytes, bpt) * (100 + i64::from(b.safety_margin_pct)), 100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    #[must_use]
    pub fn at(t: OffsetDateTime) -> Self {
        let d = t.to_offset(time::UtcOffset::UTC).date();
        let monthly = Date::from_calendar_date(d.year(), d.month(), 1).unwrap_or(d);
        Self { daily: d, monthly }
    }

    fn pairs(self) -> [(&'static str, Date); 2] {
        [(PERIOD_DAILY, self.daily), (PERIOD_MONTHLY, self.monthly)]
    }
}

/// Next reset instant (midnight UTC tomorrow / 1st of next month).
#[must_use]
pub fn next_reset(period: &str, now: OffsetDateTime) -> OffsetDateTime {
    let d = now.to_offset(time::UtcOffset::UTC).date();
    let next = if period == PERIOD_DAILY {
        d.next_day().unwrap_or(d)
    } else {
        let (y, m) = if d.month() == Month::December {
            (d.year() + 1, Month::January)
        } else {
            (d.year(), d.month().next())
        };
        Date::from_calendar_date(y, m, 1).unwrap_or(d)
    };
    next.midnight().assume_utc()
}

/// Inputs of the preflight estimate.
#[derive(Debug, Clone, Default)]
pub struct PreflightInput {
    pub content_bytes: usize,
    pub image_count: u32,
    pub prior_context_tokens: i64,
    pub chat_has_ready_documents: bool,
    pub chat_has_ready_ci_files: bool,
    pub web_search_requested: bool,
    pub max_output_cap: u32,
    pub minimal_generation_floor: u32,
}

/// Which tools a model gets for this request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolSelection {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

#[must_use]
pub fn tool_selection(m: &ModelCatalogEntry, input: &PreflightInput, kill: &KillSwitches) -> ToolSelection {
    let ts = &m.general_config.tool_support;
    ToolSelection {
        file_search: input.chat_has_ready_documents && ts.file_search && !kill.disable_file_search,
        web_search: input.web_search_requested && ts.web_search,
        code_interpreter: input.chat_has_ready_ci_files && ts.code_interpreter && !kill.disable_code_interpreter,
    }
}

/// Reserve a candidate model would book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

#[must_use]
pub fn candidate_reserve(m: &ModelCatalogEntry, input: &PreflightInput, kill: &KillSwitches) -> CandidateReserve {
    let b = &m.estimation_budgets;
    let tools = tool_selection(m, input, kill);
    let mut est = estimate_text_tokens(input.content_bytes, b)
        + input.prior_context_tokens.max(0)
        + i64::from(input.image_count) * i64::from(b.image_token_budget);
    if tools.file_search {
        est += i64::from(b.tool_surcharge_tokens);
    }
    if tools.web_search {
        est += i64::from(b.web_search_surcharge_tokens);
    }
    if tools.code_interpreter {
        est += i64::from(b.code_interpreter_surcharge_tokens);
    }
    let max_out = i64::from(m.max_output_tokens.min(input.max_output_cap));
    let (im, om) = multipliers(m);
    let credits = credits_micro(est, max_out, im, om).unwrap_or_else(|e| {
        tracing::warn!(model = %m.id, error = %e, "cannot compute reserve; candidate unavailable");
        i64::MAX
    });
    CandidateReserve {
        estimated_input_tokens: est,
        max_output_tokens_applied: max_out,
        reserve_tokens: est + max_out,
        reserved_credits_micro: credits,
        minimal_generation_floor_applied: i64::from(input.minimal_generation_floor).min(max_out),
    }
}

/// Spent and reserved credits of the user's bucket rows for the current periods.
#[derive(Debug, Clone, Default)]
pub struct UsageView {
    /// `(bucket, period_type)` -> row values.
    pub rows: HashMap<(String, String), RowValues>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowValues {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

impl UsageView {
    #[must_use]
    pub fn get(&self, bucket: &str, period: &str) -> RowValues {
        self.rows
            .get(&(bucket.to_owned(), period.to_owned()))
            .copied()
            .unwrap_or_default()
    }
}

#[must_use]
pub fn limit_for(limits: &UserLimits, bucket: &str, period: &str) -> i64 {
    let t = if bucket == BUCKET_PREMIUM { limits.premium } else { limits.standard };
    if period == PERIOD_DAILY {
        t.limit_daily_credits_micro
    } else {
        t.limit_monthly_credits_micro
    }
}

fn buckets_for(tier: ModelTier) -> &'static [&'static str] {
    match tier {
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
        ModelTier::Standard => &[BUCKET_TOTAL],
    }
}

fn tier_available(tier: ModelTier, usage: &UsageView, limits: &UserLimits, reserve: i64) -> bool {
    buckets_for(tier).iter().all(|bucket| {
        [PERIOD_DAILY, PERIOD_MONTHLY].iter().all(|period| {
            let r = usage.get(bucket, period);
            r.spent
                .saturating_add(r.reserved)
                .saturating_add(reserve)
                <= limit_for(limits, bucket, period)
        })
    })
}

/// Result of the downgrade cascade.
#[derive(Debug, Clone)]
pub struct CascadeDecision {
    pub effective: ModelCatalogEntry,
    pub downgrade_reason: Option<&'static str>,
    pub reserve: CandidateReserve,
    pub tools: ToolSelection,
}

impl CascadeDecision {
    #[must_use]
    pub fn is_downgrade(&self, selected: &str) -> bool {
        self.downgrade_reason.is_some() || self.effective.id != selected
    }
}

fn candidate_for<'a>(snapshot: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snapshot.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Runs the premium -> standard cascade (DESIGN "Downgrade Decision Flow").
///
/// # Errors
/// `QuotaExceeded(tokens)` when no tier is available.
pub fn resolve_effective_model(
    snapshot: &PolicySnapshot,
    selected_model: &str,
    usage: &UsageView,
    limits: &UserLimits,
    input: &PreflightInput,
) -> Result<CascadeDecision, DomainError> {
    let kill = &snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.find(selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled")),
        None => (ModelTier::Premium, Some("model_disabled")),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for &tier in cascade {
        if tier == ModelTier::Premium && (kill.force_standard_tier || kill.disable_premium_tier) {
            if reason.is_none() {
                reason = Some(if kill.force_standard_tier {
                    "force_standard_tier"
                } else {
                    "disable_premium_tier"
                });
            }
            continue;
        }
        let Some(candidate) = candidate_for(snapshot, tier, selected_model) else {
            continue;
        };
        let reserve = candidate_reserve(candidate, input, kill);
        if tier_available(tier, usage, limits, reserve.reserved_credits_micro) {
            return Ok(CascadeDecision {
                effective: candidate.clone(),
                downgrade_reason: reason,
                reserve,
                tools: tool_selection(candidate, input, kill),
            });
        }
        if tier == ModelTier::Premium && reason.is_none() {
            reason = Some("premium_quota_exhausted");
        }
    }
    Err(DomainError::QuotaExceeded(quota_scope::TOKENS))
}

/// Daily tool quotas, checked only for tools that are sent.
///
/// # Errors
/// `QuotaExceeded(web_search|code_interpreter)`.
pub fn check_tool_quotas(
    tools: ToolSelection,
    usage: &UsageView,
    web_search_daily_quota: u32,
    code_interpreter_daily_quota: u32,
) -> Result<(), DomainError> {
    let daily = usage.get(BUCKET_TOTAL, PERIOD_DAILY);
    if tools.web_search && daily.web_search_calls >= i64::from(web_search_daily_quota) {
        return Err(DomainError::QuotaExceeded(quota_scope::WEB_SEARCH));
    }
    if tools.code_interpreter && daily.code_interpreter_calls >= i64::from(code_interpreter_daily_quota) {
        return Err(DomainError::QuotaExceeded(quota_scope::CODE_INTERPRETER));
    }
    Ok(())
}

// ── DB operations ───────────────────────────────────────────────────────────

/// Reads the user's bucket rows of the given periods.
///
/// # Errors
/// Database errors.
pub async fn load_usage(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: PeriodStarts,
) -> Result<UsageView, DomainError> {
    let rows = quota_usage::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(quota_usage::Column::TenantId.eq(tenant_id))
                .add(quota_usage::Column::UserId.eq(user_id))
                .add(
                    Condition::any()
                        .add(
                            Condition::all()
                                .add(quota_usage::Column::PeriodType.eq(PERIOD_DAILY))
                                .add(quota_usage::Column::PeriodStart.eq(periods.daily)),
                        )
                        .add(
                            Condition::all()
                                .add(quota_usage::Column::PeriodType.eq(PERIOD_MONTHLY))
                                .add(quota_usage::Column::PeriodStart.eq(periods.monthly)),
                        ),
                ),
        )
        .all(runner)
        .await?;
    let mut view = UsageView::default();
    for r in rows {
        view.rows.insert(
            (r.bucket.clone(), r.period_type.clone()),
            RowValues {
                spent: r.spent_credits_micro,
                reserved: r.reserved_credits_micro,
                web_search_calls: i64::from(r.web_search_calls),
                code_interpreter_calls: i64::from(r.code_interpreter_calls),
            },
        );
    }
    Ok(view)
}

async fn ensure_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    period: &str,
    start: Date,
    bucket: &str,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = quota_usage::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant_id),
        user_id: ActiveValue::Set(user_id),
        period_type: ActiveValue::Set(period.to_owned()),
        period_start: ActiveValue::Set(start),
        bucket: ActiveValue::Set(bucket.to_owned()),
        spent_credits_micro: ActiveValue::Set(0),
        reserved_credits_micro: ActiveValue::Set(0),
        calls: ActiveValue::Set(0),
        input_tokens: ActiveValue::Set(0),
        output_tokens: ActiveValue::Set(0),
        file_search_calls: ActiveValue::Set(0),
        web_search_calls: ActiveValue::Set(0),
        code_interpreter_calls: ActiveValue::Set(0),
        rag_retrieval_calls: ActiveValue::Set(0),
        image_inputs: ActiveValue::Set(0),
        image_upload_bytes: ActiveValue::Set(0),
        updated_at: ActiveValue::Set(now),
    };
    let res = quota_usage::Entity::insert(am)
        .secure()
        .scope_unchecked(scope)?
        .on_conflict_raw(
            OnConflict::columns([
                quota_usage::Column::TenantId,
                quota_usage::Column::UserId,
                quota_usage::Column::PeriodType,
                quota_usage::Column::PeriodStart,
                quota_usage::Column::Bucket,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec(runner)
        .await;
    match res {
        Ok(_) | Err(toolkit_db::secure::ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn row_filter(tenant_id: Uuid, user_id: Uuid, period: &str, start: Date, bucket: &str) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant_id))
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(period))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

/// Increments reserved credits of the effective tier's buckets and re-checks
/// every limit inside the same transaction.
///
/// # Errors
/// `QuotaExceeded(tokens)` when a bucket is over its limit after the increment.
#[allow(clippy::too_many_arguments)]
pub async fn write_reserve(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    tier: ModelTier,
    reserved_credits: i64,
    periods: PeriodStarts,
    limits: &UserLimits,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    use sea_orm::ExprTrait;
    for (period, start) in periods.pairs() {
        for bucket in buckets_for(tier) {
            ensure_row(runner, scope, tenant_id, user_id, period, start, bucket, now).await?;
            quota_usage::Entity::update_many()
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro).add(reserved_credits),
                )
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now))
                .filter(row_filter(tenant_id, user_id, period, start, bucket))
                .secure()
                .scope_with(scope)
                .exec(runner)
                .await?;
        }
    }
    let view = load_usage(runner, scope, tenant_id, user_id, periods).await?;
    for (period, _) in periods.pairs() {
        for bucket in buckets_for(tier) {
            let r = view.get(bucket, period);
            if r.spent.saturating_add(r.reserved) > limit_for(limits, bucket, period) {
                return Err(DomainError::QuotaExceeded(quota_scope::TOKENS));
            }
        }
    }
    Ok(())
}

/// Settlement method of a terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    Actual,
    Estimated,
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Values applied by one settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub method: SettlementMethod,
    pub committed_credits_micro: i64,
    pub actual_input_tokens: i64,
    pub actual_output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
    pub overshoot: bool,
}

/// Persisted preflight values of a turn.
#[derive(Debug, Clone, Copy)]
pub struct TurnReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

/// Computes the settlement of a terminal outcome.
///
/// `usage` is the provider-reported `(input, output)` when the actual path
/// applies.
///
/// # Errors
/// Credit computation errors.
pub fn compute_settlement(
    method: SettlementMethod,
    reserve: TurnReserve,
    usage: Option<(i64, i64)>,
    mults: (i64, i64),
    overshoot_tolerance: f64,
    tool_calls: (i64, i64),
) -> Result<Settlement, CreditError> {
    let (web, ci) = tool_calls;
    match method {
        SettlementMethod::Actual => {
            let (i, o) = usage.unwrap_or((0, 0));
            let actual = credits_micro(i, o, mults.0, mults.1)?;
            let actual_tokens = i.saturating_add(o);
            let mut committed = actual;
            let overshoot = actual_tokens > reserve.reserve_tokens;
            if overshoot && reserve.reserve_tokens > 0 {
                #[allow(clippy::cast_precision_loss)]
                let factor = actual_tokens as f64 / reserve.reserve_tokens as f64;
                if factor > overshoot_tolerance {
                    committed = reserve.reserved_credits_micro;
                }
            }
            Ok(Settlement {
                method,
                committed_credits_micro: committed,
                actual_input_tokens: i,
                actual_output_tokens: o,
                web_search_calls: web,
                code_interpreter_calls: ci,
                overshoot,
            })
        }
        SettlementMethod::Estimated => {
            let est_input = (reserve.reserve_tokens - reserve.max_output_tokens_applied).max(0);
            let charged = credits_micro(est_input, reserve.minimal_generation_floor_applied.max(0), mults.0, mults.1)?;
            Ok(Settlement {
                method,
                committed_credits_micro: charged,
                actual_input_tokens: 0,
                actual_output_tokens: 0,
                web_search_calls: web,
                code_interpreter_calls: ci,
                overshoot: false,
            })
        }
        SettlementMethod::Released => Ok(Settlement {
            method,
            committed_credits_micro: 0,
            actual_input_tokens: 0,
            actual_output_tokens: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            overshoot: false,
        }),
    }
}

/// Applies a settlement to the bucket rows of the reserve's periods.
///
/// # Errors
/// Database errors.
#[allow(clippy::too_many_arguments)]
pub async fn apply_settlement(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    tier: ModelTier,
    reserved_credits: i64,
    periods: PeriodStarts,
    s: &Settlement,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    use sea_orm::ExprTrait;
    let actual = s.method == SettlementMethod::Actual;
    for (period, start) in periods.pairs() {
        for bucket in buckets_for(tier) {
            ensure_row(runner, scope, tenant_id, user_id, period, start, bucket, now).await?;
            let mut upd = quota_usage::Entity::update_many()
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro).sub(reserved_credits),
                )
                .col_expr(
                    quota_usage::Column::SpentCreditsMicro,
                    Expr::col(quota_usage::Column::SpentCreditsMicro).add(s.committed_credits_micro),
                )
                .col_expr(quota_usage::Column::Calls, Expr::col(quota_usage::Column::Calls).add(1))
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now));
            if *bucket == BUCKET_TOTAL {
                if actual {
                    upd = upd
                        .col_expr(
                            quota_usage::Column::InputTokens,
                            Expr::col(quota_usage::Column::InputTokens).add(s.actual_input_tokens),
                        )
                        .col_expr(
                            quota_usage::Column::OutputTokens,
                            Expr::col(quota_usage::Column::OutputTokens).add(s.actual_output_tokens),
                        );
                }
                if s.method != SettlementMethod::Released {
                    upd = upd
                        .col_expr(
                            quota_usage::Column::WebSearchCalls,
                            Expr::col(quota_usage::Column::WebSearchCalls).add(s.web_search_calls),
                        )
                        .col_expr(
                            quota_usage::Column::CodeInterpreterCalls,
                            Expr::col(quota_usage::Column::CodeInterpreterCalls).add(s.code_interpreter_calls),
                        );
                }
            }
            upd.filter(row_filter(tenant_id, user_id, period, start, bucket))
                .secure()
                .scope_with(scope)
                .exec(runner)
                .await?;
        }
    }
    Ok(())
}

/// One per-tier, per-period status entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: i64,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Computes the quota status of the user; periods with a limit `<= 0` are skipped.
#[must_use]
pub fn status(usage: &UsageView, limits: &UserLimits, warning_threshold_pct: u8, now: OffsetDateTime) -> Vec<PeriodStatus> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            let limit = limit_for(limits, bucket, period);
            if limit <= 0 {
                continue;
            }
            let r = usage.get(bucket, period);
            let used = r.spent.saturating_add(r.reserved).max(0);
            let remaining = (limit - used).max(0);
            let pct = i64::try_from(i128::from(remaining) * 100 / i128::from(limit)).unwrap_or(0);
            out.push(PeriodStatus {
                tier,
                period,
                limit,
                used,
                remaining,
                remaining_percentage: pct,
                next_reset: next_reset(period, now),
                warning: pct <= 100 - i64::from(warning_threshold_pct),
                exhausted: pct == 0,
            });
        }
    }
    out
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
