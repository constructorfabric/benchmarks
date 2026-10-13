//! Quota service (DESIGN §3.2, §5.4): cascade, reserve, settlement, status.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, TierLimits, UserLimits};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::{Date, Duration, Month, OffsetDateTime, Time};
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::credits::{Reserve, ReserveInputs, ToolFlags, compute_reserve};
use crate::domain::error::{DomainError, QuotaScope};
use crate::infra::db::entities::quota_usage;
use crate::infra::db::now;

/// Bucket names.
pub const BUCKET_TOTAL: &str = "total";
/// Premium sub-cap bucket.
pub const BUCKET_PREMIUM: &str = "tier:premium";

/// Quota period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// Calendar day (UTC).
    Daily,
    /// Calendar month (UTC).
    Monthly,
}

impl Period {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// Period starts of a turn (computed once at preflight).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    /// UTC day.
    pub daily: Date,
    /// First day of the UTC month.
    pub monthly: Date,
}

impl PeriodStarts {
    /// Period starts of a UTC timestamp.
    #[must_use]
    pub fn of(ts: OffsetDateTime) -> Self {
        let d = ts.to_offset(time::UtcOffset::UTC).date();
        let monthly = Date::from_calendar_date(d.year(), d.month(), 1).unwrap_or(d);
        Self { daily: d, monthly }
    }

    fn start(self, p: Period) -> Date {
        match p {
            Period::Daily => self.daily,
            Period::Monthly => self.monthly,
        }
    }
}

/// Next reset (RFC 3339 midnight UTC) of a period.
#[must_use]
pub fn next_reset(p: Period, starts: PeriodStarts) -> OffsetDateTime {
    let date = match p {
        Period::Daily => starts.daily.next_day().unwrap_or(starts.daily),
        Period::Monthly => {
            let (y, m) = (starts.monthly.year(), starts.monthly.month());
            let (ny, nm) = if m == Month::December { (y + 1, Month::January) } else { (y, m.next()) };
            Date::from_calendar_date(ny, nm, 1).unwrap_or(starts.monthly + Duration::days(31))
        }
    };
    date.with_time(Time::MIDNIGHT).assume_utc()
}

/// Spent/reserved/counters of one bucket row (zeros when missing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    /// Settled credits.
    pub spent: i64,
    /// In-flight reserves.
    pub reserved: i64,
    /// Web search calls (daily total).
    pub web_search_calls: i64,
    /// Code interpreter calls (daily total).
    pub code_interpreter_calls: i64,
}

/// Usage of the four relevant rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageRows {
    /// total / daily
    pub total_daily: BucketUsage,
    /// total / monthly
    pub total_monthly: BucketUsage,
    /// premium / daily
    pub premium_daily: BucketUsage,
    /// premium / monthly
    pub premium_monthly: BucketUsage,
}

impl UsageRows {
    fn get(&self, bucket: &str, p: Period) -> BucketUsage {
        match (bucket, p) {
            (BUCKET_TOTAL, Period::Daily) => self.total_daily,
            (BUCKET_TOTAL, Period::Monthly) => self.total_monthly,
            (_, Period::Daily) => self.premium_daily,
            (_, Period::Monthly) => self.premium_monthly,
        }
    }
}

fn limit_of(limits: &UserLimits, bucket: &str, p: Period) -> i64 {
    let t: &TierLimits = if bucket == BUCKET_TOTAL { &limits.standard } else { &limits.premium };
    match p {
        Period::Daily => t.limit_daily_credits_micro,
        Period::Monthly => t.limit_monthly_credits_micro,
    }
}

fn bucket_available(rows: &UsageRows, limits: &UserLimits, bucket: &str, p: Period, extra: i64) -> bool {
    let u = rows.get(bucket, p);
    u.spent.saturating_add(u.reserved).saturating_add(extra) <= limit_of(limits, bucket, p)
}

/// Buckets charged for a tier.
#[must_use]
pub fn buckets_for(tier: ModelTier) -> &'static [&'static str] {
    match tier {
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
        ModelTier::Standard => &[BUCKET_TOTAL],
    }
}

/// `allow` / `downgrade`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    /// Selected model used.
    Allow,
    /// Another model used.
    Downgrade,
}

impl QuotaDecision {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// Request facts used by the cascade.
#[derive(Debug, Clone, Copy, Default)]
pub struct CascadeRequest {
    /// Current message and history proxy.
    pub reserve_inputs: ReserveInputs,
    /// Tools the request is eligible for before model support and kill switches:
    /// `file_search` = chat has a ready document (and a vector store),
    /// `web_search` = `web_search.enabled`,
    /// `code_interpreter` = chat has a ready code-interpreter attachment.
    pub eligible_tools: ToolFlags,
    /// `streaming.max_output_tokens`.
    pub streaming_max_output_tokens: u32,
}

/// Result of the preflight cascade.
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    /// Effective model.
    pub effective: ModelCatalogEntry,
    /// Decision.
    pub decision: QuotaDecision,
    /// Downgrade reason.
    pub downgrade_reason: Option<&'static str>,
    /// Reserve of the effective model.
    pub reserve: Reserve,
    /// Tools sent with the effective model.
    pub tools: ToolFlags,
}

/// Tools sent with a candidate model.
#[must_use]
pub fn tool_flags(m: &ModelCatalogEntry, ks: KillSwitches, req: &CascadeRequest) -> ToolFlags {
    let ts = &m.general_config.tool_support;
    ToolFlags {
        file_search: req.eligible_tools.file_search && ts.file_search && !ks.disable_file_search,
        web_search: req.eligible_tools.web_search && ts.web_search,
        code_interpreter: req.eligible_tools.code_interpreter && ts.code_interpreter && !ks.disable_code_interpreter,
    }
}

fn candidate_for<'a>(snapshot: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snapshot.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Runs the downgrade cascade (DESIGN "Downgrade Decision Flow").
///
/// # Errors
/// `QuotaExceeded(Tokens)` when no tier is available.
pub fn cascade(
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    rows: &UsageRows,
    selected: &str,
    req: &CascadeRequest,
) -> Result<PreflightDecision, DomainError> {
    let ks = snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.model(selected) {
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
                reason = Some(if ks.force_standard_tier { "force_standard_tier" } else { "disable_premium_tier" });
            }
            continue;
        }
        let Some(candidate) = candidate_for(snapshot, tier, selected) else { continue };
        let tools = tool_flags(candidate, ks, req);
        let mut inputs = req.reserve_inputs;
        inputs.tools = tools;
        let reserve = compute_reserve(
            &inputs,
            &candidate.estimation_budgets,
            candidate.max_output_tokens,
            req.streaming_max_output_tokens,
            candidate.input_tokens_credit_multiplier_micro,
            candidate.output_tokens_credit_multiplier_micro,
        );
        if reserve.reserved_credits_micro == i64::MAX {
            tracing::warn!(model = %candidate.id, "reserve of cascade candidate cannot be computed");
        }
        let available = reserve.reserved_credits_micro != i64::MAX
            && buckets_for(tier).iter().all(|b| {
                [Period::Daily, Period::Monthly]
                    .iter()
                    .all(|p| bucket_available(rows, limits, b, *p, reserve.reserved_credits_micro))
            });
        if !available {
            if tier == ModelTier::Premium && reason.is_none() {
                reason = Some("premium_quota_exhausted");
            }
            continue;
        }
        let decision = if candidate.id == selected && reason.is_none() {
            QuotaDecision::Allow
        } else {
            QuotaDecision::Downgrade
        };
        return Ok(PreflightDecision {
            effective: candidate.clone(),
            decision,
            downgrade_reason: if decision == QuotaDecision::Downgrade { reason.or(Some("premium_quota_exhausted")) } else { None },
            reserve,
            tools,
        });
    }
    Err(DomainError::QuotaExceeded(QuotaScope::Tokens))
}

/// Per-tier, per-period status entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    /// `premium` / `total`.
    pub tier: &'static str,
    /// Period.
    pub period: Period,
    /// Limit.
    pub limit: i64,
    /// Spent + reserved.
    pub used: i64,
    /// Remaining.
    pub remaining: i64,
    /// Floor percentage 0–100.
    pub remaining_percentage: u32,
    /// Next reset.
    pub next_reset: OffsetDateTime,
    /// Warning flag.
    pub warning: bool,
    /// Exhausted flag.
    pub exhausted: bool,
}

/// Computes status entries (periods with a limit `<= 0` are skipped).
#[must_use]
pub fn status_entries(rows: &UsageRows, limits: &UserLimits, starts: PeriodStarts, warning_pct: u8) -> Vec<PeriodStatus> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        for p in [Period::Daily, Period::Monthly] {
            let limit = limit_of(limits, bucket, p);
            if limit <= 0 {
                continue;
            }
            let u = rows.get(bucket, p);
            let used = u.spent.saturating_add(u.reserved).max(0);
            let remaining = (limit - used).max(0);
            let pct = u32::try_from((i128::from(remaining) * 100).checked_div(i128::from(limit)).unwrap_or(0).clamp(0, 100)).unwrap_or(0);
            out.push(PeriodStatus {
                tier,
                period: p,
                limit,
                used,
                remaining,
                remaining_percentage: pct,
                next_reset: next_reset(p, starts),
                warning: pct <= u32::from(100 - warning_pct.min(100)),
                exhausted: pct == 0,
            });
        }
    }
    out
}

fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

fn row_cond(user_id: Uuid, period: Period, start: Date, bucket: &str) -> Condition {
    Condition::all()
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(period.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

/// Reads the four bucket rows of a user.
///
/// # Errors
/// Database errors.
pub async fn read_rows(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    starts: &PeriodStarts,
) -> Result<UsageRows, DomainError> {
    let rows = quota_usage::Entity::find()
        .filter(
            Condition::all()
                .add(quota_usage::Column::UserId.eq(user_id))
                .add(quota_usage::Column::PeriodStart.is_in([starts.daily, starts.monthly])),
        )
        .secure()
        .scope_with(&owner_scope(tenant_id, user_id))
        .all(runner)
        .await?;
    let mut out = UsageRows::default();
    for r in rows {
        let usage = BucketUsage {
            spent: r.spent_credits_micro,
            reserved: r.reserved_credits_micro,
            web_search_calls: i64::from(r.web_search_calls),
            code_interpreter_calls: i64::from(r.code_interpreter_calls),
        };
        match (r.bucket.as_str(), r.period_type.as_str()) {
            (BUCKET_TOTAL, "daily") if r.period_start == starts.daily => out.total_daily = usage,
            (BUCKET_TOTAL, "monthly") if r.period_start == starts.monthly => out.total_monthly = usage,
            (BUCKET_PREMIUM, "daily") if r.period_start == starts.daily => out.premium_daily = usage,
            (BUCKET_PREMIUM, "monthly") if r.period_start == starts.monthly => out.premium_monthly = usage,
            _ => {}
        }
    }
    Ok(out)
}

async fn ensure_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: Period,
    start: Date,
    bucket: &str,
) -> Result<(), DomainError> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set(period.as_str().to_owned()),
        period_start: Set(start),
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
        updated_at: Set(now()),
    };
    let scope = owner_scope(tenant_id, user_id);
    let res = quota_usage::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)?
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
        Ok(_) | Err(ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Increments of one settlement / reserve on a bucket row.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowDelta {
    /// Added to `reserved_credits_micro`.
    pub reserved: i64,
    /// Added to `spent_credits_micro`.
    pub spent: i64,
    /// Added to `calls`.
    pub calls: i32,
    /// Added to `input_tokens`.
    pub input_tokens: i64,
    /// Added to `output_tokens`.
    pub output_tokens: i64,
    /// Added to `web_search_calls`.
    pub web_search_calls: i32,
    /// Added to `code_interpreter_calls`.
    pub code_interpreter_calls: i32,
}

async fn apply_delta(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: Period,
    start: Date,
    bucket: &str,
    d: RowDelta,
) -> Result<(), DomainError> {
    use quota_usage::Column as C;
    use sea_orm::sea_query::ExprTrait as _;
    ensure_row(runner, tenant_id, user_id, period, start, bucket).await?;
    quota_usage::Entity::update_many()
        .secure()
        .col_expr(C::ReservedCreditsMicro, Expr::col(C::ReservedCreditsMicro).add(d.reserved))
        .col_expr(C::SpentCreditsMicro, Expr::col(C::SpentCreditsMicro).add(d.spent))
        .col_expr(C::Calls, Expr::col(C::Calls).add(d.calls))
        .col_expr(C::InputTokens, Expr::col(C::InputTokens).add(d.input_tokens))
        .col_expr(C::OutputTokens, Expr::col(C::OutputTokens).add(d.output_tokens))
        .col_expr(C::WebSearchCalls, Expr::col(C::WebSearchCalls).add(d.web_search_calls))
        .col_expr(C::CodeInterpreterCalls, Expr::col(C::CodeInterpreterCalls).add(d.code_interpreter_calls))
        .col_expr(C::UpdatedAt, Expr::value(now()))
        .filter(row_cond(user_id, period, start, bucket))
        .scope_with(&owner_scope(tenant_id, user_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Writes the reserve and re-checks every bucket of the decision (TOCTOU guard).
///
/// # Errors
/// `QuotaExceeded(Tokens)` when a bucket is over its limit after the increment.
pub async fn write_reserve(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    starts: &PeriodStarts,
    tier: ModelTier,
    credits: i64,
    limits: &UserLimits,
) -> Result<(), DomainError> {
    for bucket in buckets_for(tier) {
        for p in [Period::Daily, Period::Monthly] {
            apply_delta(runner, tenant_id, user_id, p, starts.start(p), bucket, RowDelta { reserved: credits, ..RowDelta::default() })
                .await?;
        }
    }
    let rows = read_rows(runner, tenant_id, user_id, starts).await?;
    for bucket in buckets_for(tier) {
        for p in [Period::Daily, Period::Monthly] {
            if !bucket_available(&rows, limits, bucket, p, 0) {
                return Err(DomainError::QuotaExceeded(QuotaScope::Tokens));
            }
        }
    }
    Ok(())
}

/// Applies a settlement to the bucket rows of a turn.
///
/// # Errors
/// Database errors.
#[allow(clippy::too_many_arguments)]
pub async fn apply_settlement(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    starts: &PeriodStarts,
    tier: ModelTier,
    turn_reserved: i64,
    committed: i64,
    telemetry_tokens: (i64, i64),
    tool_calls: (i32, i32),
) -> Result<(), DomainError> {
    for p in [Period::Daily, Period::Monthly] {
        let total = RowDelta {
            reserved: -turn_reserved,
            spent: committed,
            calls: 1,
            input_tokens: telemetry_tokens.0,
            output_tokens: telemetry_tokens.1,
            web_search_calls: tool_calls.0,
            code_interpreter_calls: tool_calls.1,
        };
        apply_delta(runner, tenant_id, user_id, p, starts.start(p), BUCKET_TOTAL, total).await?;
        if tier == ModelTier::Premium {
            let prem = RowDelta { reserved: -turn_reserved, spent: committed, calls: 1, ..RowDelta::default() };
            apply_delta(runner, tenant_id, user_id, p, starts.start(p), BUCKET_PREMIUM, prem).await?;
        }
    }
    Ok(())
}

/// Daily tool quotas (checked only for tools actually sent).
///
/// # Errors
/// `QuotaExceeded(WebSearch|CodeInterpreter)`.
pub fn check_tool_quotas(
    rows: &UsageRows,
    tools: ToolFlags,
    web_search_daily: u32,
    code_interpreter_daily: u32,
) -> Result<(), DomainError> {
    if tools.web_search && rows.total_daily.web_search_calls >= i64::from(web_search_daily) {
        return Err(DomainError::QuotaExceeded(QuotaScope::WebSearch));
    }
    if tools.code_interpreter && rows.total_daily.code_interpreter_calls >= i64::from(code_interpreter_daily) {
        return Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter));
    }
    Ok(())
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
