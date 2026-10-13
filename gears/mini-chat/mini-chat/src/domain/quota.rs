//! Quota service: credit buckets, preflight cascade, reserve / settle, status
//! (DESIGN §3.2 quota service, §5.4, §5.5).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, TierLimits, UserLimits};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter, Set};
use time::{Date, Month, OffsetDateTime, Time};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::billing::Settlement;
use crate::domain::credits::credits_micro_checked;
use crate::domain::error::DomainError;
use crate::domain::estimate::estimate_text_tokens;
use crate::infra::db::entity::quota_usage;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";

/// Period starts captured at preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    /// UTC day and month starts of `t`.
    #[must_use]
    pub fn of(t: OffsetDateTime) -> Self {
        let d = t.to_offset(time::UtcOffset::UTC).date();
        let monthly = Date::from_calendar_date(d.year(), d.month(), 1).unwrap_or(d);
        Self { daily: d, monthly }
    }

    fn pairs(self) -> [(&'static str, Date); 2] {
        [(PERIOD_DAILY, self.daily), (PERIOD_MONTHLY, self.monthly)]
    }
}

/// Next reset of a period (RFC 3339 midnight UTC).
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
    next.with_time(Time::MIDNIGHT).assume_utc()
}

/// Spent / reserved / tool counters of one bucket row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BucketUsage {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Usage rows of the four (period × bucket) combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageSnapshot {
    pub daily_total: BucketUsage,
    pub monthly_total: BucketUsage,
    pub daily_premium: BucketUsage,
    pub monthly_premium: BucketUsage,
}

/// Request facts that drive estimation and tool inclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // reason: independent request facts, not a state machine
pub struct RequestFacts {
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub image_count: usize,
    pub has_ready_docs: bool,
    pub has_ready_xlsx: bool,
    pub web_search_requested: bool,
}

/// Built-in tools sent with a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolFlags {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Tools the model `m` gets for these facts and kill switches.
#[must_use]
pub fn tool_flags(m: &ModelCatalogEntry, facts: &RequestFacts, ks: KillSwitches) -> ToolFlags {
    let ts = m.tool_support();
    ToolFlags {
        file_search: facts.has_ready_docs && ts.file_search && !ks.disable_file_search,
        web_search: facts.web_search_requested && ts.web_search,
        code_interpreter: facts.has_ready_xlsx && ts.code_interpreter && !ks.disable_code_interpreter,
    }
}

/// Reserve a candidate model would book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    /// `i64::MAX` when the credit computation fails (candidate unavailable).
    pub reserved_credits_micro: i64,
    pub tools: ToolFlags,
}

/// `candidate_reserve(m)` (§5.4.1).
#[must_use]
pub fn candidate_reserve(
    m: &ModelCatalogEntry,
    facts: &RequestFacts,
    ks: KillSwitches,
    max_output_cap: u32,
) -> CandidateReserve {
    let b = &m.estimation_budgets;
    let tools = tool_flags(m, facts, ks);
    #[allow(clippy::integer_division)] // reason: saturation sentinel (half of i64::MAX), exact by design
    let images = i64::try_from(facts.image_count).unwrap_or(i64::MAX / 2);
    let mut input = estimate_text_tokens(facts.message_bytes, b)
        .saturating_add(std::cmp::max(facts.prior_context_tokens, 0))
        .saturating_add(images.saturating_mul(i64::from(b.image_token_budget)));
    if tools.file_search {
        input = input.saturating_add(i64::from(b.tool_surcharge_tokens));
    }
    if tools.web_search {
        input = input.saturating_add(i64::from(b.web_search_surcharge_tokens));
    }
    if tools.code_interpreter {
        input = input.saturating_add(i64::from(b.code_interpreter_surcharge_tokens));
    }
    let max_out = i64::from(std::cmp::min(m.max_output_tokens, max_output_cap));
    let credits = credits_micro_checked(
        input,
        max_out,
        m.input_tokens_credit_multiplier_micro,
        m.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(model = %m.id, error = %e, "candidate reserve not computable; treated as unavailable");
        i64::MAX
    });
    CandidateReserve {
        estimated_input_tokens: input,
        max_output_tokens_applied: max_out,
        reserve_tokens: input.saturating_add(max_out),
        reserved_credits_micro: credits,
        tools,
    }
}

fn fits(b: BucketUsage, reserve: i64, limit: i64) -> bool {
    b.spent
        .checked_add(b.reserved)
        .and_then(|v| v.checked_add(reserve))
        .is_some_and(|v| v <= limit)
}

/// Bucket availability for a tier with a given reserve (§5.4.2).
#[must_use]
pub fn tier_available(tier: ModelTier, usage: &UsageSnapshot, limits: &UserLimits, reserve: i64) -> bool {
    let total_ok = fits(usage.daily_total, reserve, limits.standard.limit_daily_credits_micro)
        && fits(usage.monthly_total, reserve, limits.standard.limit_monthly_credits_micro);
    match tier {
        ModelTier::Standard => total_ok,
        ModelTier::Premium => {
            total_ok
                && fits(usage.daily_premium, reserve, limits.premium.limit_daily_credits_micro)
                && fits(usage.monthly_premium, reserve, limits.premium.limit_monthly_credits_micro)
        }
    }
}

/// Outcome of the downgrade cascade.
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeDecision {
    pub effective: ModelCatalogEntry,
    pub tier: ModelTier,
    pub downgraded: bool,
    pub downgrade_reason: Option<&'static str>,
    pub reserve: CandidateReserve,
}

fn candidate_for<'a>(snap: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snap.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Resolve the effective model (§4 "Downgrade Decision Flow").
/// `None` = every tier exhausted / unavailable (429 `quota_exceeded`).
#[must_use]
pub fn cascade(
    snap: &PolicySnapshot,
    limits: &UserLimits,
    usage: &UsageSnapshot,
    selected_model: &str,
    facts: &RequestFacts,
    max_output_cap: u32,
) -> Option<CascadeDecision> {
    let ks = &snap.kill_switches;
    let (start_tier, mut reason): (ModelTier, Option<&'static str>) = match snap.find(selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled")),
        None => (ModelTier::Premium, Some("model_disabled")),
    };
    let tiers: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for &tier in tiers {
        if tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
            if reason.is_none() {
                reason = Some(if ks.force_standard_tier {
                    "force_standard_tier"
                } else {
                    "disable_premium_tier"
                });
            }
            continue;
        }
        let Some(cand) = candidate_for(snap, tier, selected_model) else {
            continue;
        };
        let reserve = candidate_reserve(cand, facts, *ks, max_output_cap);
        if reserve.reserved_credits_micro != i64::MAX
            && tier_available(tier, usage, limits, reserve.reserved_credits_micro)
        {
            let downgraded = cand.id != selected_model || reason.is_some();
            return Some(CascadeDecision {
                effective: cand.clone(),
                tier,
                downgraded,
                downgrade_reason: if downgraded { reason.or(Some("premium_quota_exhausted")) } else { None },
                reserve,
            });
        }
        if tier == ModelTier::Premium && reason.is_none() {
            reason = Some("premium_quota_exhausted");
        }
    }
    None
}

/// One period entry of the quota status / warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: OffsetDateTime,
}

/// Per-tier, per-period status (periods with limit <= 0 are skipped).
#[must_use]
pub fn status_entries(
    usage: &UsageSnapshot,
    limits: &UserLimits,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> Vec<PeriodStatus> {
    let mut out = Vec::new();
    let rows: [(&'static str, &'static str, BucketUsage, i64); 4] = [
        ("premium", PERIOD_DAILY, usage.daily_premium, limits.premium.limit_daily_credits_micro),
        ("premium", PERIOD_MONTHLY, usage.monthly_premium, limits.premium.limit_monthly_credits_micro),
        ("total", PERIOD_DAILY, usage.daily_total, limits.standard.limit_daily_credits_micro),
        ("total", PERIOD_MONTHLY, usage.monthly_total, limits.standard.limit_monthly_credits_micro),
    ];
    for (tier, period, b, limit) in rows {
        if limit <= 0 {
            continue;
        }
        let used = b.spent.saturating_add(b.reserved);
        let remaining = std::cmp::max(limit - used, 0);
        #[allow(clippy::integer_division)] // reason: deliberate truncating integer percentage
        let pct = u32::try_from((i128::from(remaining) * 100 / i128::from(limit)).clamp(0, 100)).unwrap_or(0);
        out.push(PeriodStatus {
            tier,
            period,
            limit,
            used,
            remaining,
            remaining_percentage: pct,
            warning: pct <= 100 - u32::from(warning_threshold_pct),
            exhausted: pct == 0,
            next_reset: next_reset(period, now),
        });
    }
    out
}

// ─────────────────────────────── DB operations ───────────────────────────────

fn owner_cond(tenant: Uuid, user: Uuid) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant))
        .add(quota_usage::Column::UserId.eq(user))
}

/// Read the four bucket rows (missing rows count as zero).
///
/// # Errors
/// Database failure.
pub async fn read_usage(
    runner: &impl DBRunner,
    tenant: Uuid,
    user: Uuid,
    periods: PeriodStarts,
) -> Result<UsageSnapshot, DomainError> {
    let rows = quota_usage::Entity::find()
        .filter(
            owner_cond(tenant, user).add(
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
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .all(runner)
        .await?;
    let mut s = UsageSnapshot::default();
    for r in rows {
        let b = BucketUsage {
            spent: r.spent_credits_micro,
            reserved: r.reserved_credits_micro,
            web_search_calls: i64::from(r.web_search_calls),
            code_interpreter_calls: i64::from(r.code_interpreter_calls),
        };
        match (r.period_type.as_str(), r.bucket.as_str()) {
            (PERIOD_DAILY, BUCKET_TOTAL) => s.daily_total = b,
            (PERIOD_MONTHLY, BUCKET_TOTAL) => s.monthly_total = b,
            (PERIOD_DAILY, BUCKET_PREMIUM) => s.daily_premium = b,
            (PERIOD_MONTHLY, BUCKET_PREMIUM) => s.monthly_premium = b,
            _ => {}
        }
    }
    Ok(s)
}

async fn ensure_row(
    runner: &impl DBRunner,
    tenant: Uuid,
    user: Uuid,
    period_type: &str,
    period_start: Date,
    bucket: &str,
) -> Result<(), DomainError> {
    let now = crate::infra::db::now();
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        user_id: Set(user),
        period_type: Set(period_type.to_owned()),
        period_start: Set(period_start),
        bucket: Set(bucket.to_owned()),
        spent_credits_micro: Set(0),
        reserved_credits_micro: Set(0),
        calls: Set(0),
        input_tokens: Set(0),
        output_tokens: Set(0),
        file_search_calls: Set(0),
        web_search_calls: Set(0),
        code_interpreter_calls: Set(0),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(now),
    };
    let res = quota_usage::Entity::insert(am)
        .secure()
        .scope_unchecked(&AccessScope::for_tenant(tenant))?
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

fn row_cond(tenant: Uuid, user: Uuid, period_type: &str, period_start: Date, bucket: &str) -> Condition {
    owner_cond(tenant, user)
        .add(quota_usage::Column::PeriodType.eq(period_type))
        .add(quota_usage::Column::PeriodStart.eq(period_start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

fn buckets(premium: bool) -> &'static [&'static str] {
    if premium {
        &[BUCKET_TOTAL, BUCKET_PREMIUM]
    } else {
        &[BUCKET_TOTAL]
    }
}

/// Increment `reserved_credits_micro` of the turn's bucket rows, then re-check
/// every bucket of the decision against its limit (§5.4.2 "TOCTOU").
///
/// # Errors
/// `QuotaExceeded(Tokens)` when any bucket is over its limit after the increment.
pub async fn reserve_and_recheck(
    runner: &impl DBRunner,
    tenant: Uuid,
    user: Uuid,
    periods: PeriodStarts,
    premium: bool,
    credits: i64,
    limits: &UserLimits,
) -> Result<(), DomainError> {
    for (pt, ps) in periods.pairs() {
        for bucket in buckets(premium) {
            ensure_row(runner, tenant, user, pt, ps, bucket).await?;
            quota_usage::Entity::update_many()
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro).add(credits),
                )
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(crate::infra::db::now()))
                .filter(row_cond(tenant, user, pt, ps, bucket))
                .secure()
                .scope_with(&AccessScope::for_tenant(tenant))
                .exec(runner)
                .await?;
        }
    }
    let usage = read_usage(runner, tenant, user, periods).await?;
    let over = |b: BucketUsage, limit: i64| b.spent.saturating_add(b.reserved) > limit;
    let mut exceeded = over(usage.daily_total, limits.standard.limit_daily_credits_micro)
        || over(usage.monthly_total, limits.standard.limit_monthly_credits_micro);
    if premium {
        exceeded = exceeded
            || over(usage.daily_premium, limits.premium.limit_daily_credits_micro)
            || over(usage.monthly_premium, limits.premium.limit_monthly_credits_micro);
    }
    if exceeded {
        return Err(DomainError::QuotaExceeded(crate::domain::error::QuotaScope::Tokens));
    }
    Ok(())
}

/// Per-settlement tool call counters (bucket `total` only).
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounts {
    pub web_search: i32,
    pub code_interpreter: i32,
}

/// Apply a settlement to the turn's bucket rows (same period starts as the reserve).
///
/// # Errors
/// Database failure.
#[allow(clippy::too_many_arguments)]
pub async fn apply_settlement(
    runner: &impl DBRunner,
    tenant: Uuid,
    user: Uuid,
    periods: PeriodStarts,
    premium: bool,
    reserved_credits: i64,
    s: &Settlement,
    tools: ToolCounts,
) -> Result<(), DomainError> {
    let now = crate::infra::db::now();
    for (pt, ps) in periods.pairs() {
        for bucket in buckets(premium) {
            ensure_row(runner, tenant, user, pt, ps, bucket).await?;
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
                upd = upd
                    .col_expr(
                        quota_usage::Column::InputTokens,
                        Expr::col(quota_usage::Column::InputTokens).add(s.telemetry_input_tokens),
                    )
                    .col_expr(
                        quota_usage::Column::OutputTokens,
                        Expr::col(quota_usage::Column::OutputTokens).add(s.telemetry_output_tokens),
                    );
                if s.count_tool_calls {
                    upd = upd
                        .col_expr(
                            quota_usage::Column::WebSearchCalls,
                            Expr::col(quota_usage::Column::WebSearchCalls).add(tools.web_search),
                        )
                        .col_expr(
                            quota_usage::Column::CodeInterpreterCalls,
                            Expr::col(quota_usage::Column::CodeInterpreterCalls).add(tools.code_interpreter),
                        );
                }
            }
            upd.filter(row_cond(tenant, user, pt, ps, bucket))
                .secure()
                .scope_with(&AccessScope::for_tenant(tenant))
                .exec(runner)
                .await?;
        }
    }
    Ok(())
}

/// Limits used for a tier (exposed for tests).
#[must_use]
pub fn limits_of(limits: &UserLimits, tier: ModelTier) -> TierLimits {
    match tier {
        ModelTier::Premium => limits.premium,
        ModelTier::Standard => limits.standard,
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
