//! Quota service: preflight cascade, reserve with re-check, settlement,
//! warnings (DESIGN §3.2 quota service, §5.4).

use std::collections::HashMap;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

use crate::domain::credits::{Period, credits_micro, estimate_text_tokens, remaining_percentage};
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::db::entities::quota_usage;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";

/// Usage of one `(period, bucket)` row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Rows of the current periods, keyed by `(period, bucket)`.
pub type UsageMap = HashMap<(&'static str, &'static str), BucketUsage>;

/// Tool gates of a candidate model for this request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one independent flag per tool
pub struct ToolGates {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Request facts known before the cascade.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // independent request facts consumed by the preflight cascade
pub struct PreflightRequest {
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub image_count: u32,
    pub has_ready_documents: bool,
    pub has_ready_code_interpreter_files: bool,
    pub web_search_requested: bool,
    pub max_output_cap: u32,
    pub floor: u32,
}

/// Reserve figures of one candidate model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub gates: ToolGates,
}

#[must_use]
pub fn max_output_applied(model: &ModelCatalogEntry, cap: u32) -> u32 {
    if model.max_output_tokens == 0 { cap } else { model.max_output_tokens.min(cap) }
}

#[must_use]
pub fn tool_gates(model: &ModelCatalogEntry, ks: &KillSwitches, req: &PreflightRequest) -> ToolGates {
    let ts = model.tool_support();
    ToolGates {
        file_search: req.has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: req.web_search_requested && ts.web_search,
        code_interpreter: req.has_ready_code_interpreter_files && ts.code_interpreter && !ks.disable_code_interpreter,
    }
}

/// Reserve the candidate would book (DESIGN §5.4.1).
///
/// # Errors
/// Credit computation failure (zero / out-of-range multiplier, overflow).
pub fn candidate_reserve(
    model: &ModelCatalogEntry,
    ks: &KillSwitches,
    req: &PreflightRequest,
) -> Result<CandidateReserve, crate::domain::credits::CreditError> {
    let b = &model.estimation_budgets;
    let gates = tool_gates(model, ks, req);
    let mut est = estimate_text_tokens(req.message_bytes, b) + req.prior_context_tokens.max(0);
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
    let max_out = max_output_applied(model, req.max_output_cap);
    let credits = credits_micro(
        est,
        i64::from(max_out),
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    )?;
    Ok(CandidateReserve {
        estimated_input_tokens: est,
        max_output_tokens_applied: max_out,
        reserve_tokens: est + i64::from(max_out),
        reserved_credits_micro: credits,
        gates,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    Allow,
    Downgrade,
}

impl QuotaDecision {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// Outcome of the cascade.
#[derive(Debug, Clone)]
pub struct CascadeOutcome {
    pub effective: ModelCatalogEntry,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<String>,
    pub reserve: CandidateReserve,
}

fn limit_of(limits: &UserLimits, bucket: &str, period: Period) -> i64 {
    let t = if bucket == BUCKET_PREMIUM { &limits.premium } else { &limits.standard };
    match period {
        Period::Daily => t.limit_daily_credits_micro,
        Period::Monthly => t.limit_monthly_credits_micro,
    }
}

fn buckets_for(tier: ModelTier) -> &'static [&'static str] {
    match tier {
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
        ModelTier::Standard => &[BUCKET_TOTAL],
    }
}

/// `spent + reserved + this <= limit` for every required bucket and period.
#[must_use]
pub fn tier_available(tier: ModelTier, usage: &UsageMap, limits: &UserLimits, this: i64) -> bool {
    for bucket in buckets_for(tier) {
        for period in Period::ALL {
            let u = usage.get(&(period.as_str(), *bucket)).copied().unwrap_or_default();
            let total = u.spent.saturating_add(u.reserved).saturating_add(this);
            if total > limit_of(limits, bucket, period) {
                return false;
            }
        }
    }
    true
}

fn pick_candidate<'a>(snap: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snap.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Run the premium → standard cascade (DESIGN "Downgrade Decision Flow").
///
/// # Errors
/// `QuotaExceeded{tokens}` when no tier is available.
pub fn run_cascade(
    snap: &PolicySnapshot,
    selected: &ModelCatalogEntry,
    usage: &UsageMap,
    limits: &UserLimits,
    req: &PreflightRequest,
) -> DomainResult<CascadeOutcome> {
    let ks = &snap.kill_switches;
    let mut reason: Option<String> = (!selected.enabled).then(|| "model_disabled".to_owned());
    let cascade: &[ModelTier] = match selected.tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for &tier in cascade {
        if tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
            if reason.is_none() {
                reason = Some(
                    if ks.force_standard_tier { "force_standard_tier" } else { "disable_premium_tier" }.to_owned(),
                );
            }
            continue;
        }
        let Some(candidate) = pick_candidate(snap, tier, &selected.id) else {
            continue;
        };
        let reserve = match candidate_reserve(candidate, ks, req) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(model = %candidate.id, error = %e, "cascade candidate reserve not computable");
                if tier == ModelTier::Premium && reason.is_none() {
                    reason = Some("premium_quota_exhausted".to_owned());
                }
                continue;
            }
        };
        if tier_available(tier, usage, limits, reserve.reserved_credits_micro) {
            let decision = if candidate.id == selected.id && reason.is_none() {
                QuotaDecision::Allow
            } else {
                QuotaDecision::Downgrade
            };
            if decision == QuotaDecision::Downgrade && reason.is_none() {
                reason = Some("premium_quota_exhausted".to_owned());
            }
            return Ok(CascadeOutcome {
                effective: candidate.clone(),
                decision,
                downgrade_reason: if decision == QuotaDecision::Downgrade { reason } else { None },
                reserve,
            });
        }
        if tier == ModelTier::Premium && reason.is_none() {
            reason = Some("premium_quota_exhausted".to_owned());
        }
    }
    Err(DomainError::QuotaExceeded { scope: "tokens".to_owned() })
}

/// Owner scope `(tenant, user)` for `quota_usage`.
#[must_use]
pub fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant_id),
        ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
    ])])
}

/// Period starts of `at`.
#[must_use]
pub fn period_starts(at: OffsetDateTime) -> [(Period, Date); 2] {
    [(Period::Daily, Period::Daily.start(at)), (Period::Monthly, Period::Monthly.start(at))]
}

/// Load the user's bucket rows for the periods containing `at`.
///
/// # Errors
/// Database failure.
pub async fn load_usage(runner: &impl DBRunner, tenant_id: Uuid, user_id: Uuid, at: OffsetDateTime) -> DomainResult<UsageMap> {
    let scope = owner_scope(tenant_id, user_id);
    let mut cond = Condition::any();
    for (p, start) in period_starts(at) {
        cond = cond.add(
            Condition::all()
                .add(quota_usage::Column::PeriodType.eq(p.as_str()))
                .add(quota_usage::Column::PeriodStart.eq(start)),
        );
    }
    let rows = quota_usage::Entity::find()
        .filter(cond)
        .secure()
        .scope_with(&scope)
        .all(runner)
        .await?;
    let mut map = UsageMap::new();
    for r in rows {
        let period = if r.period_type == "daily" { "daily" } else if r.period_type == "monthly" { "monthly" } else { continue };
        let bucket = if r.bucket == BUCKET_TOTAL { BUCKET_TOTAL } else if r.bucket == BUCKET_PREMIUM { BUCKET_PREMIUM } else { continue };
        map.insert(
            (period, bucket),
            BucketUsage {
                spent: r.spent_credits_micro,
                reserved: r.reserved_credits_micro,
                web_search_calls: i64::from(r.web_search_calls),
                code_interpreter_calls: i64::from(r.code_interpreter_calls),
            },
        );
    }
    Ok(map)
}

/// Bucket row deltas.
#[derive(Debug, Clone, Copy, Default)]
pub struct BucketDelta {
    pub reserved: i64,
    pub spent: i64,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Atomically add `delta` to the row `(tenant, user, period, start, bucket)`, creating it if absent.
///
/// # Errors
/// Database failure.
pub async fn apply_delta(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: Period,
    start: Date,
    bucket: &str,
    delta: BucketDelta,
) -> DomainResult<()> {
    use quota_usage::Column as C;
    use sea_orm::sea_query::ExprTrait as _;

    let scope = owner_scope(tenant_id, user_id);
    let now = OffsetDateTime::now_utc();
    let key = Condition::all()
        .add(quota_usage::Column::PeriodType.eq(period.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket));
    let existing = quota_usage::Entity::find()
        .filter(key.clone())
        .secure()
        .scope_with(&scope)
        .one(runner)
        .await?;
    if existing.is_none() {
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
            updated_at: Set(now),
        };
        secure_insert::<quota_usage::Entity>(am, &scope, runner).await?;
    }
    quota_usage::Entity::update_many()
        .col_expr(C::ReservedCreditsMicro, Expr::col(C::ReservedCreditsMicro).add(delta.reserved))
        .col_expr(C::SpentCreditsMicro, Expr::col(C::SpentCreditsMicro).add(delta.spent))
        .col_expr(C::Calls, Expr::col(C::Calls).add(delta.calls))
        .col_expr(C::InputTokens, Expr::col(C::InputTokens).add(delta.input_tokens))
        .col_expr(C::OutputTokens, Expr::col(C::OutputTokens).add(delta.output_tokens))
        .col_expr(C::WebSearchCalls, Expr::col(C::WebSearchCalls).add(delta.web_search_calls))
        .col_expr(C::CodeInterpreterCalls, Expr::col(C::CodeInterpreterCalls).add(delta.code_interpreter_calls))
        .col_expr(C::UpdatedAt, Expr::value(now))
        .filter(key)
        .secure()
        .scope_with(&scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Write the reserve for `tier` and re-check every bucket against its limit.
///
/// # Errors
/// `QuotaExceeded{tokens}` when a bucket went over its limit (the caller's
/// transaction must roll back), or a database failure.
pub async fn reserve_and_recheck(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    at: OffsetDateTime,
    tier: ModelTier,
    credits: i64,
    limits: &UserLimits,
) -> DomainResult<()> {
    for (period, start) in period_starts(at) {
        for bucket in buckets_for(tier) {
            apply_delta(runner, tenant_id, user_id, period, start, bucket, BucketDelta { reserved: credits, ..Default::default() })
                .await?;
        }
    }
    let usage = load_usage(runner, tenant_id, user_id, at).await?;
    if !tier_available(tier, &usage, limits, 0) {
        return Err(DomainError::QuotaExceeded { scope: "tokens".to_owned() });
    }
    Ok(())
}

/// One entry of `done.quota_warnings` / quota status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Status of tiers `premium` and `total` (periods with limit <= 0 omitted).
#[must_use]
pub fn quota_status(usage: &UsageMap, limits: &UserLimits, warning_threshold_pct: u8, now: OffsetDateTime) -> Vec<(&'static str, Vec<PeriodStatus>)> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        let mut periods = Vec::new();
        for period in Period::ALL {
            let limit = limit_of(limits, bucket, period);
            if limit <= 0 {
                continue;
            }
            let u = usage.get(&(period.as_str(), bucket)).copied().unwrap_or_default();
            let used = u.spent + u.reserved;
            let pct = remaining_percentage(limit, used);
            periods.push(PeriodStatus {
                tier,
                period: period.as_str(),
                limit,
                used,
                remaining: (limit - used).max(0),
                remaining_percentage: pct,
                next_reset: period.next_reset(now),
                warning: pct <= 100 - u32::from(warning_threshold_pct),
                exhausted: pct == 0,
            });
        }
        out.push((tier, periods));
    }
    out
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::TierLimits;
    use serde_json::json;

    use super::*;

    fn model(id: &str, tier: &str, enabled: bool, default: bool, mult: i64) -> ModelCatalogEntry {
        serde_json::from_value(json!({
            "id": id, "tier": tier, "enabled": enabled,
            "input_tokens_credit_multiplier_micro": mult, "output_tokens_credit_multiplier_micro": mult,
            "max_output_tokens": 500, "context_window": 10000,
            "preference": {"is_default": default, "sort_order": 0},
            "general_config": {"tool_support": {"web_search": true, "file_search": true}}
        }))
        .expect("model")
    }

    fn snap(ks: KillSwitches) -> PolicySnapshot {
        PolicySnapshot {
            policy_version: 1,
            model_catalog: vec![
                model("p", "premium", true, true, 2_500_000_000),
                model("s", "standard", true, false, 1_000_000_000),
            ],
            kill_switches: ks,
        }
    }

    fn limits() -> UserLimits {
        UserLimits {
            user_id: Uuid::nil(),
            policy_version: 1,
            standard: TierLimits { limit_daily_credits_micro: 60_000_000, limit_monthly_credits_micro: 600_000_000 },
            premium: TierLimits { limit_daily_credits_micro: 22_000_000, limit_monthly_credits_micro: 300_000_000 },
        }
    }

    fn req() -> PreflightRequest {
        PreflightRequest {
            message_bytes: 0,
            prior_context_tokens: 890,
            image_count: 0,
            has_ready_documents: false,
            has_ready_code_interpreter_files: false,
            web_search_requested: false,
            max_output_cap: 32768,
            floor: 50,
        }
    }

    #[test]
    fn premium_allowed_when_quota_available() {
        let s = snap(KillSwitches::default());
        let out = run_cascade(&s, &s.model_catalog[0], &UsageMap::new(), &limits(), &req()).expect("ok");
        assert_eq!(out.effective.id, "p");
        assert_eq!(out.decision, QuotaDecision::Allow);
        assert!(out.downgrade_reason.is_none());
        // est input = 110 + 890 = 1000 ; out 500 => 2.5M + 1.25M
        assert_eq!(out.reserve.reserved_credits_micro, 3_750_000);
        assert_eq!(out.reserve.reserve_tokens, 1500);
    }

    #[test]
    fn design_example_downgrades_to_standard() {
        let s = snap(KillSwitches::default());
        let mut usage = UsageMap::new();
        usage.insert(("daily", BUCKET_PREMIUM), BucketUsage { spent: 20_000_000, ..Default::default() });
        usage.insert(("monthly", BUCKET_PREMIUM), BucketUsage { spent: 200_000_000, ..Default::default() });
        usage.insert(("daily", BUCKET_TOTAL), BucketUsage { spent: 25_000_000, ..Default::default() });
        usage.insert(("monthly", BUCKET_TOTAL), BucketUsage { spent: 240_000_000, ..Default::default() });
        let out = run_cascade(&s, &s.model_catalog[0], &usage, &limits(), &req()).expect("ok");
        assert_eq!(out.effective.id, "s");
        assert_eq!(out.decision, QuotaDecision::Downgrade);
        assert_eq!(out.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));
        assert_eq!(out.reserve.reserved_credits_micro, 1_500_000);
    }

    #[test]
    fn all_tiers_exhausted_is_429_tokens() {
        let s = snap(KillSwitches::default());
        let mut usage = UsageMap::new();
        usage.insert(("monthly", BUCKET_TOTAL), BucketUsage { spent: 600_000_000, ..Default::default() });
        let err = run_cascade(&s, &s.model_catalog[0], &usage, &limits(), &req()).expect_err("429");
        assert!(matches!(err, DomainError::QuotaExceeded { scope } if scope == "tokens"));
    }

    #[test]
    fn standard_never_upgrades() {
        let s = snap(KillSwitches::default());
        let mut usage = UsageMap::new();
        usage.insert(("daily", BUCKET_TOTAL), BucketUsage { spent: 60_000_000, ..Default::default() });
        assert!(run_cascade(&s, &s.model_catalog[1], &usage, &limits(), &req()).is_err());
    }

    #[test]
    fn kill_switch_and_disabled_model_reasons() {
        let s = snap(KillSwitches { force_standard_tier: true, ..Default::default() });
        let out = run_cascade(&s, &s.model_catalog[0], &UsageMap::new(), &limits(), &req()).expect("ok");
        assert_eq!(out.downgrade_reason.as_deref(), Some("force_standard_tier"));
        let s = snap(KillSwitches { disable_premium_tier: true, ..Default::default() });
        let out = run_cascade(&s, &s.model_catalog[0], &UsageMap::new(), &limits(), &req()).expect("ok");
        assert_eq!(out.downgrade_reason.as_deref(), Some("disable_premium_tier"));
        let mut s = snap(KillSwitches::default());
        s.model_catalog[0].enabled = false;
        let selected = s.model_catalog[0].clone();
        let out = run_cascade(&s, &selected, &UsageMap::new(), &limits(), &req()).expect("ok");
        assert_eq!(out.effective.id, "s");
        assert_eq!(out.downgrade_reason.as_deref(), Some("model_disabled"));
    }

    #[test]
    fn surcharges_follow_tool_support() {
        let s = snap(KillSwitches::default());
        let mut r = req();
        r.web_search_requested = true;
        r.has_ready_documents = true;
        r.image_count = 2;
        let res = candidate_reserve(&s.model_catalog[0], &s.kill_switches, &r).expect("ok");
        assert_eq!(res.estimated_input_tokens, 110 + 890 + 2000 + 500 + 500);
        assert!(res.gates.web_search && res.gates.file_search && !res.gates.code_interpreter);
        let ks = KillSwitches { disable_file_search: true, ..Default::default() };
        let res = candidate_reserve(&s.model_catalog[0], &ks, &r).expect("ok");
        assert!(!res.gates.file_search);
    }

    #[test]
    fn status_skips_periods_without_a_positive_limit() {
        let mut l = limits();
        l.premium.limit_daily_credits_micro = 0;
        l.premium.limit_monthly_credits_micro = -1;
        l.standard.limit_monthly_credits_micro = 0;
        let st = quota_status(&UsageMap::new(), &l, 80, OffsetDateTime::now_utc());
        assert_eq!(st[0].0, "premium");
        assert!(st[0].1.is_empty(), "premium periods skipped");
        let total: Vec<&str> = st[1].1.iter().map(|p| p.period).collect();
        assert_eq!(total, vec!["daily"]);
    }

    #[test]
    fn status_flags() {
        let mut usage = UsageMap::new();
        usage.insert(("daily", BUCKET_TOTAL), BucketUsage { spent: 50_000_000, reserved: 0, ..Default::default() });
        usage.insert(("daily", BUCKET_PREMIUM), BucketUsage { spent: 22_000_000, ..Default::default() });
        let st = quota_status(&usage, &limits(), 80, OffsetDateTime::now_utc());
        let total_daily = st[1].1.iter().find(|p| p.period == "daily").expect("daily");
        assert_eq!(total_daily.remaining_percentage, 16);
        assert!(total_daily.warning);
        assert!(!total_daily.exhausted);
        let prem_daily = st[0].1.iter().find(|p| p.period == "daily").expect("daily");
        assert!(prem_daily.exhausted);
    }
}
