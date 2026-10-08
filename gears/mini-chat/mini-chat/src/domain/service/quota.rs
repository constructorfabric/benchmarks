//! Quota service (DESIGN §3.2 "quota service", §5): preflight cascade, reserve,
//! settlement, billing outcome derivation and quota status.

use chrono::{DateTime, NaiveDate, Utc};
use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, TierLimits, UsageTokens, UserLimits,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{AppServices, now, policy};
use crate::domain::error::DomainError;
use crate::domain::estimate::{
    BUCKET_PREMIUM, BUCKET_TOTAL, PeriodType, credits_micro, estimate_text_tokens, remaining_percentage,
    warning_flags,
};
use crate::infra::db::entity::quota_usage;

/// Tools that a model gets for a turn (decided before the adapter runs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolFlags {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Request facts needed by the preflight.
#[derive(Debug, Clone)]
pub struct PreflightRequest {
    pub selected_model: String,
    pub message_bytes: usize,
    pub image_count: usize,
    pub prior_context_tokens: i64,
    pub has_ready_documents: bool,
    pub has_ready_code_files: bool,
    pub web_search_requested: bool,
}

/// Outcome of a successful preflight.
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub effective: ModelCatalogEntry,
    pub selected_model: String,
    pub downgrade_reason: Option<&'static str>,
    pub tools: ToolFlags,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: u32,
    pub policy_version: u64,
    pub kill_switches: KillSwitches,
    pub limits: UserLimits,
    pub periods: Vec<(PeriodType, NaiveDate)>,
}

impl PreflightDecision {
    #[must_use]
    pub fn quota_decision(&self) -> &'static str {
        if self.effective.id == self.selected_model && self.downgrade_reason.is_none() {
            "allow"
        } else {
            "downgrade"
        }
    }

    #[must_use]
    pub fn is_premium(&self) -> bool {
        self.effective.tier == ModelTier::Premium
    }
}

/// Current bucket state of one row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketState {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Bucket states keyed by (period, bucket).
#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub rows: Vec<(PeriodType, &'static str, BucketState)>,
}

impl UsageSnapshot {
    #[must_use]
    pub fn get(&self, period: PeriodType, bucket: &str) -> BucketState {
        self.rows
            .iter()
            .find(|(p, b, _)| *p == period && *b == bucket)
            .map(|(_, _, s)| *s)
            .unwrap_or_default()
    }
}

fn limit_for(limits: &UserLimits, bucket: &str, period: PeriodType) -> i64 {
    let t: &TierLimits = if bucket == BUCKET_PREMIUM {
        &limits.premium
    } else {
        &limits.standard
    };
    match period {
        PeriodType::Daily => t.limit_daily_credits_micro,
        PeriodType::Monthly => t.limit_monthly_credits_micro,
    }
}

/// Tools a candidate model would get.
#[must_use]
pub fn tools_for(model: &ModelCatalogEntry, req: &PreflightRequest, ks: &KillSwitches) -> ToolFlags {
    let ts = &model.general_config.tool_support;
    ToolFlags {
        file_search: req.has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: req.web_search_requested && ts.web_search,
        code_interpreter: req.has_ready_code_files && ts.code_interpreter && !ks.disable_code_interpreter,
    }
}

/// Reserve figures of a candidate: (estimated input, max output applied, reserve tokens, credits).
#[must_use]
pub fn candidate_reserve(
    model: &ModelCatalogEntry,
    req: &PreflightRequest,
    tools: ToolFlags,
    output_cap: u32,
) -> (i64, u32, i64, i64) {
    let b = &model.estimation_budgets;
    let mut est = estimate_text_tokens(req.message_bytes, b) + req.prior_context_tokens.max(0);
    est += i64::try_from(req.image_count).unwrap_or(0) * i64::from(b.image_token_budget);
    if tools.file_search {
        est += i64::from(b.tool_surcharge_tokens);
    }
    if tools.web_search {
        est += i64::from(b.web_search_surcharge_tokens);
    }
    if tools.code_interpreter {
        est += i64::from(b.code_interpreter_surcharge_tokens);
    }
    let mot = if model.max_output_tokens == 0 {
        output_cap
    } else {
        model.max_output_tokens.min(output_cap)
    };
    let credits = credits_micro(
        est,
        i64::from(mot),
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(model = %model.id, error = %e, "candidate reserve cannot be computed");
        i64::MAX
    });
    (est, mot, est + i64::from(mot), credits)
}

fn tier_available(
    tier: ModelTier,
    usage: &UsageSnapshot,
    limits: &UserLimits,
    reserve: i64,
) -> bool {
    let buckets: &[&str] = match tier {
        ModelTier::Standard => &[BUCKET_TOTAL],
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
    };
    PeriodType::ALL.iter().all(|p| {
        buckets.iter().all(|b| {
            let s = usage.get(*p, b);
            s.spent
                .saturating_add(s.reserved)
                .saturating_add(reserve)
                <= limit_for(limits, b, *p)
        })
    })
}

fn candidate<'a>(snapshot: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snapshot.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.preference.is_default))
        .or_else(|| enabled().next())
}

/// Pure cascade (DESIGN §4 "Downgrade Decision Flow").
///
/// # Errors
/// `QuotaExceeded("tokens"|"web_search"|"code_interpreter")`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_cascade(
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    usage: &UsageSnapshot,
    req: &PreflightRequest,
    output_cap: u32,
    gear_floor: u32,
    web_daily_quota: u32,
    ci_daily_quota: u32,
    periods: Vec<(PeriodType, NaiveDate)>,
) -> Result<PreflightDecision, DomainError> {
    let ks = snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.model(&req.selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled")),
        None => (ModelTier::Premium, Some("model_disabled")),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
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
        let Some(model) = candidate(snapshot, *tier, &req.selected_model) else {
            continue;
        };
        let tools = tools_for(model, req, &ks);
        let (est, mot, reserve_tokens, credits) = candidate_reserve(model, req, tools, output_cap);
        if !tier_available(*tier, usage, limits, credits) {
            if *tier == ModelTier::Premium && reason.is_none() {
                reason = Some("premium_quota_exhausted");
            }
            continue;
        }
        let daily_total = usage.get(PeriodType::Daily, BUCKET_TOTAL);
        if tools.web_search && daily_total.web_search_calls >= i64::from(web_daily_quota) {
            return Err(DomainError::QuotaExceeded("web_search"));
        }
        if tools.code_interpreter && daily_total.code_interpreter_calls >= i64::from(ci_daily_quota) {
            return Err(DomainError::QuotaExceeded("code_interpreter"));
        }
        return Ok(PreflightDecision {
            effective: model.clone(),
            selected_model: req.selected_model.clone(),
            downgrade_reason: if model.id == req.selected_model { reason } else { reason.or(Some("premium_quota_exhausted")) },
            tools,
            estimated_input_tokens: est,
            max_output_tokens_applied: mot,
            reserve_tokens,
            reserved_credits_micro: credits,
            minimal_generation_floor_applied: gear_floor.min(mot),
            policy_version: snapshot.policy_version,
            kill_switches: ks,
            limits: limits.clone(),
            periods,
        });
    }
    Err(DomainError::QuotaExceeded("tokens"))
}

/// Current period keys.
#[must_use]
pub fn current_periods(at: DateTime<Utc>) -> Vec<(PeriodType, NaiveDate)> {
    PeriodType::ALL.iter().map(|p| (*p, p.start_of(at))).collect()
}

/// Reads the user's bucket rows for the given periods.
///
/// # Errors
/// Database failure.
pub async fn read_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &[(PeriodType, NaiveDate)],
) -> Result<UsageSnapshot, DomainError> {
    let mut out = UsageSnapshot::default();
    for (p, start) in periods {
        let rows = quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(
                Condition::all()
                    .add(quota_usage::Column::UserId.eq(user_id))
                    .add(quota_usage::Column::PeriodType.eq(p.as_str()))
                    .add(quota_usage::Column::PeriodStart.eq(*start)),
            )
            .all(runner)
            .await?;
        for r in rows {
            let bucket = if r.bucket == BUCKET_PREMIUM {
                BUCKET_PREMIUM
            } else if r.bucket == BUCKET_TOTAL {
                BUCKET_TOTAL
            } else {
                continue;
            };
            out.rows.push((
                *p,
                bucket,
                BucketState {
                    spent: r.spent_credits_micro,
                    reserved: r.reserved_credits_micro,
                    web_search_calls: i64::from(r.web_search_calls),
                    code_interpreter_calls: i64::from(r.code_interpreter_calls),
                },
            ));
        }
    }
    Ok(out)
}

/// Increments applied to one bucket row.
#[derive(Debug, Clone, Copy, Default)]
pub struct BucketDelta {
    pub reserved: i64,
    pub spent: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

/// Applies a delta to a bucket row, creating it when missing.
///
/// # Errors
/// Database failure.
pub async fn apply_delta(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: PeriodType,
    start: NaiveDate,
    bucket: &str,
    d: BucketDelta,
) -> Result<(), DomainError> {
    use sea_orm::ExprTrait as _;
    let scope = AccessScope::for_tenant(tenant_id);
    let filter = Condition::all()
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(period.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket));
    for _ in 0..2 {
        let rows = quota_usage::Entity::update_many()
            .col_expr(
                quota_usage::Column::ReservedCreditsMicro,
                Expr::col(quota_usage::Column::ReservedCreditsMicro).add(d.reserved),
            )
            .col_expr(
                quota_usage::Column::SpentCreditsMicro,
                Expr::col(quota_usage::Column::SpentCreditsMicro).add(d.spent),
            )
            .col_expr(quota_usage::Column::Calls, Expr::col(quota_usage::Column::Calls).add(d.calls))
            .col_expr(
                quota_usage::Column::InputTokens,
                Expr::col(quota_usage::Column::InputTokens).add(d.input_tokens),
            )
            .col_expr(
                quota_usage::Column::OutputTokens,
                Expr::col(quota_usage::Column::OutputTokens).add(d.output_tokens),
            )
            .col_expr(
                quota_usage::Column::WebSearchCalls,
                Expr::col(quota_usage::Column::WebSearchCalls).add(d.web_search_calls),
            )
            .col_expr(
                quota_usage::Column::CodeInterpreterCalls,
                Expr::col(quota_usage::Column::CodeInterpreterCalls).add(d.code_interpreter_calls),
            )
            .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now()))
            .filter(filter.clone())
            .secure()
            .scope_with(&scope)
            .exec(runner)
            .await?
            .rows_affected;
        if rows > 0 {
            return Ok(());
        }
        let am = quota_usage::ActiveModel {
            id: sea_orm::Set(Uuid::new_v4()),
            tenant_id: sea_orm::Set(tenant_id),
            user_id: sea_orm::Set(user_id),
            period_type: sea_orm::Set(period.as_str().to_owned()),
            period_start: sea_orm::Set(start),
            bucket: sea_orm::Set(bucket.to_owned()),
            spent_credits_micro: sea_orm::Set(d.spent),
            reserved_credits_micro: sea_orm::Set(d.reserved),
            calls: sea_orm::Set(d.calls),
            input_tokens: sea_orm::Set(d.input_tokens),
            output_tokens: sea_orm::Set(d.output_tokens),
            file_search_calls: sea_orm::Set(0),
            web_search_calls: sea_orm::Set(d.web_search_calls),
            code_interpreter_calls: sea_orm::Set(d.code_interpreter_calls),
            rag_retrieval_calls: sea_orm::Set(0),
            image_inputs: sea_orm::Set(0),
            image_upload_bytes: sea_orm::Set(0),
            updated_at: sea_orm::Set(now()),
        };
        match quota_usage::Entity::insert(am)
            .secure()
            .scope_unchecked(&scope)?
            .exec(runner)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                let err = DomainError::from(e);
                if !matches!(err, DomainError::UniqueViolation) {
                    return Err(err);
                }
            }
        }
    }
    Err(DomainError::internal("quota row upsert contention"))
}

/// Books the reserve and re-checks the limits (TOCTOU guard, ADR-0008).
///
/// # Errors
/// `QuotaExceeded("tokens")` when any bucket is over its limit after the increment.
pub async fn reserve(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    d: &PreflightDecision,
) -> Result<(), DomainError> {
    let buckets: &[&str] = if d.is_premium() {
        &[BUCKET_TOTAL, BUCKET_PREMIUM]
    } else {
        &[BUCKET_TOTAL]
    };
    for (p, start) in &d.periods {
        for b in buckets {
            apply_delta(
                runner,
                tenant_id,
                user_id,
                *p,
                *start,
                b,
                BucketDelta {
                    reserved: d.reserved_credits_micro,
                    ..Default::default()
                },
            )
            .await?;
        }
    }
    let usage = read_usage(runner, tenant_id, user_id, &d.periods).await?;
    for (p, _) in &d.periods {
        for b in buckets {
            let s = usage.get(*p, b);
            if s.spent.saturating_add(s.reserved) > limit_for(&d.limits, b, *p) {
                return Err(DomainError::QuotaExceeded("tokens"));
            }
        }
    }
    Ok(())
}

/// Billing classification of a terminal outcome (DESIGN §5.8).
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

/// `(billing_outcome, settlement_method)` for a terminal condition.
#[must_use]
pub fn derive_billing(state: &str, error_code: Option<&str>, usage: Option<&UsageTokens>) -> (&'static str, SettlementMethod) {
    match state {
        "completed" => ("completed", SettlementMethod::Actual),
        "cancelled" => ("aborted", SettlementMethod::Estimated),
        _ => match error_code {
            Some("orphan_timeout") => ("aborted", SettlementMethod::Estimated),
            Some("context_length_exceeded" | "validation_error" | "input_too_long" | "turn_setup_failed") => {
                ("failed", SettlementMethod::Released)
            }
            Some(code) => {
                const KNOWN: &[&str] = &[
                    "provider_error",
                    "provider_timeout",
                    "rate_limited",
                    "web_search_calls_exceeded",
                    "code_interpreter_calls_exceeded",
                    "agentic_iterations_exceeded",
                    "unexpected_tool_use",
                    "message_persistence_failed",
                ];
                if !KNOWN.contains(&code) {
                    tracing::error!(error_code = code, "unknown error code at settlement");
                    return ("failed", SettlementMethod::Estimated);
                }
                if usage.is_some_and(UsageTokens::is_nonzero) {
                    ("failed", SettlementMethod::Actual)
                } else {
                    ("failed", SettlementMethod::Estimated)
                }
            }
            None => ("failed", SettlementMethod::Estimated),
        },
    }
}

/// Persisted preflight fields of a turn.
#[derive(Debug, Clone, Copy)]
pub struct TurnReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub floor_applied: i64,
    pub in_mult: i64,
    pub out_mult: i64,
}

/// Settlement amounts.
#[derive(Debug, Clone, Copy)]
pub struct Settlement {
    pub method: SettlementMethod,
    pub committed_credits: i64,
    pub telemetry_input: i64,
    pub telemetry_output: i64,
    pub count_tools: bool,
    pub overshoot: bool,
}

/// Computes the settlement of a turn (§5.4.4, §5.4.5, §5.8, §5.9).
///
/// # Errors
/// Credit computation failure.
pub fn compute_settlement(
    method: SettlementMethod,
    usage: Option<&UsageTokens>,
    r: &TurnReserve,
    overshoot_tolerance: f64,
) -> Result<Settlement, DomainError> {
    match method {
        SettlementMethod::Actual => {
            let u = usage.copied().unwrap_or_default();
            let actual = credits_micro(u.input_tokens, u.output_tokens, r.in_mult, r.out_mult)
                .map_err(|e| DomainError::internal(format!("settlement credits: {e}")))?;
            let actual_tokens = u.input_tokens + u.output_tokens;
            let mut committed = actual;
            let overshoot = actual_tokens > r.reserve_tokens;
            #[allow(clippy::cast_precision_loss)]
            if overshoot && r.reserve_tokens > 0 {
                let factor = actual_tokens as f64 / r.reserve_tokens as f64;
                if factor > overshoot_tolerance {
                    committed = r.reserved_credits_micro;
                }
            }
            Ok(Settlement {
                method,
                committed_credits: committed,
                telemetry_input: u.input_tokens,
                telemetry_output: u.output_tokens,
                count_tools: true,
                overshoot,
            })
        }
        SettlementMethod::Estimated => {
            let est_in = (r.reserve_tokens - r.max_output_tokens_applied).max(0);
            let credits = credits_micro(est_in, r.floor_applied.max(0), r.in_mult, r.out_mult)
                .map_err(|e| DomainError::internal(format!("settlement credits: {e}")))?;
            Ok(Settlement {
                method,
                committed_credits: credits,
                telemetry_input: 0,
                telemetry_output: 0,
                count_tools: true,
                overshoot: false,
            })
        }
        SettlementMethod::Released => Ok(Settlement {
            method,
            committed_credits: 0,
            telemetry_input: 0,
            telemetry_output: 0,
            count_tools: false,
            overshoot: false,
        }),
    }
}

/// Applies a settlement to the turn's bucket rows.
///
/// # Errors
/// Database failure.
#[allow(clippy::too_many_arguments)]
pub async fn settle(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &[(PeriodType, NaiveDate)],
    premium: bool,
    turn_reserved: i64,
    s: &Settlement,
    web_search_calls: i32,
    code_interpreter_calls: i32,
) -> Result<(), DomainError> {
    for (p, start) in periods {
        let tools = s.count_tools;
        apply_delta(
            runner,
            tenant_id,
            user_id,
            *p,
            *start,
            BUCKET_TOTAL,
            BucketDelta {
                reserved: -turn_reserved,
                spent: s.committed_credits,
                calls: 1,
                input_tokens: s.telemetry_input,
                output_tokens: s.telemetry_output,
                web_search_calls: if tools { web_search_calls } else { 0 },
                code_interpreter_calls: if tools { code_interpreter_calls } else { 0 },
            },
        )
        .await?;
        if premium {
            apply_delta(
                runner,
                tenant_id,
                user_id,
                *p,
                *start,
                BUCKET_PREMIUM,
                BucketDelta {
                    reserved: -turn_reserved,
                    spent: s.committed_credits,
                    calls: 1,
                    ..Default::default()
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// One per-tier, per-period status entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: PeriodType,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u8,
    pub next_reset: DateTime<Utc>,
    pub warning: bool,
    pub exhausted: bool,
}

/// Per-tier, per-period quota status (skips periods with limit <= 0).
#[must_use]
pub fn quota_status(
    limits: &UserLimits,
    usage: &UsageSnapshot,
    at: DateTime<Utc>,
    warning_threshold_pct: u8,
) -> Vec<PeriodStatus> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        for p in PeriodType::ALL {
            let limit = limit_for(limits, bucket, p);
            if limit <= 0 {
                continue;
            }
            let s = usage.get(p, bucket);
            let used = s.spent.saturating_add(s.reserved);
            let pct = remaining_percentage(limit, used);
            let (warning, exhausted) = warning_flags(pct, warning_threshold_pct);
            out.push(PeriodStatus {
                tier,
                period: p,
                limit,
                used,
                remaining: (limit - used).max(0),
                remaining_percentage: pct,
                next_reset: p.next_reset(at),
                warning,
                exhausted,
            });
        }
    }
    out
}

impl AppServices {
    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// Authorization, policy or database failure.
    pub async fn get_quota_status(&self, ctx: &SecurityContext) -> Result<(Vec<PeriodStatus>, u8), DomainError> {
        let _scope = self.authz.quota_scope(ctx).await?;
        let user = ctx.subject_id();
        let p = self.policy.plugin().await?;
        let version = p
            .get_current_policy_version(user)
            .await
            .map_err(|e| DomainError::internal(format!("model policy plugin failure: {e}")))?;
        let limits = policy::user_limits(self.policy.as_ref(), user, version).await?;
        let at = now();
        let periods = current_periods(at);
        let conn = self.conn()?;
        let usage = read_usage(&conn, ctx.subject_tenant_id(), user, &periods).await?;
        Ok((
            quota_status(&limits, &usage, at, self.cfg.quota.warning_threshold_pct),
            self.cfg.quota.warning_threshold_pct,
        ))
    }

    /// Quota warnings for the `done` event (after the settlement commit).
    ///
    /// # Errors
    /// Database failure.
    pub async fn quota_warnings(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
    ) -> Result<Vec<PeriodStatus>, DomainError> {
        let at = now();
        let periods = current_periods(at);
        let conn = self.conn()?;
        let usage = read_usage(&conn, tenant_id, user_id, &periods).await?;
        Ok(quota_status(limits, &usage, at, self.cfg.quota.warning_threshold_pct))
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
