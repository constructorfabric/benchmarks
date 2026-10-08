//! Quota service: preflight cascade (premium -> standard), reserve with limit
//! re-check, settlement and status (DESIGN §3.2 quota service, §5.4).

use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter, Set};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

use super::clock;
use super::credits::{ReserveEstimate, Surcharges, reserve_estimate};
use super::error::{DomainError, quota_scope};
use super::periods::{PeriodType, next_reset, period_start};
use super::policy::PolicyView;
use crate::infra::storage::entity::quota_usage;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, TierLimits, UserLimits};

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";

/// Inputs of the preflight estimate.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools, reason = "independent preflight input flags")]
pub struct QuotaInputs {
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub images: u32,
    pub has_ready_docs: bool,
    pub has_ready_code_files: bool,
    pub web_search_requested: bool,
}

/// Tools that will be sent for the effective model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent per-tool flags")]
pub struct ToolFlags {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Preflight outcome.
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub selected_model: String,
    pub effective: ModelCatalogEntry,
    pub downgrade_reason: Option<&'static str>,
    pub reserve: ReserveEstimate,
    pub reserved_credits_micro: i64,
    pub policy_version: u64,
    pub daily_start: Date,
    pub monthly_start: Date,
    pub tools: ToolFlags,
    pub floor_applied: i64,
    pub limits: UserLimits,
}

impl PreflightDecision {
    /// `allow` or `downgrade`.
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

/// Tool flags for a candidate model.
#[must_use]
pub fn tool_flags(m: &ModelCatalogEntry, ks: KillSwitches, inputs: &QuotaInputs) -> ToolFlags {
    let ts = &m.general_config.tool_support;
    ToolFlags {
        file_search: inputs.has_ready_docs && ts.file_search && !ks.disable_file_search,
        web_search: inputs.web_search_requested && ts.web_search,
        code_interpreter: inputs.has_ready_code_files && ts.code_interpreter && !ks.disable_code_interpreter,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    spent: i64,
    reserved: i64,
    web_search_calls: i64,
    code_interpreter_calls: i64,
}

#[derive(Debug, Clone, Copy, Default)]
struct UsageRows {
    total_daily: Bucket,
    total_monthly: Bucket,
    premium_daily: Bucket,
    premium_monthly: Bucket,
}

fn fits(b: Bucket, add: i64, limit: i64) -> bool {
    b.spent.saturating_add(b.reserved).saturating_add(add) <= limit
}

impl UsageRows {
    fn tier_available(&self, premium: bool, add: i64, limits: &UserLimits) -> bool {
        let total_ok = fits(self.total_daily, add, limits.standard.limit_daily_credits_micro)
            && fits(self.total_monthly, add, limits.standard.limit_monthly_credits_micro);
        if !premium {
            return total_ok;
        }
        total_ok
            && fits(self.premium_daily, add, limits.premium.limit_daily_credits_micro)
            && fits(self.premium_monthly, add, limits.premium.limit_monthly_credits_micro)
    }
}

async fn load_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    daily: Date,
    monthly: Date,
) -> Result<UsageRows, DomainError> {
    let rows = quota_usage::Entity::find()
        .filter(quota_usage::Column::TenantId.eq(tenant_id))
        .filter(quota_usage::Column::UserId.eq(user_id))
        .filter(
            Condition::any()
                .add(
                    Condition::all()
                        .add(quota_usage::Column::PeriodType.eq("daily"))
                        .add(quota_usage::Column::PeriodStart.eq(daily)),
                )
                .add(
                    Condition::all()
                        .add(quota_usage::Column::PeriodType.eq("monthly"))
                        .add(quota_usage::Column::PeriodStart.eq(monthly)),
                ),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    let mut out = UsageRows::default();
    for r in rows {
        let b = Bucket {
            spent: r.spent_credits_micro,
            reserved: r.reserved_credits_micro,
            web_search_calls: i64::from(r.web_search_calls),
            code_interpreter_calls: i64::from(r.code_interpreter_calls),
        };
        match (r.bucket.as_str(), r.period_type.as_str()) {
            (BUCKET_TOTAL, "daily") => out.total_daily = b,
            (BUCKET_TOTAL, "monthly") => out.total_monthly = b,
            (BUCKET_PREMIUM, "daily") => out.premium_daily = b,
            (BUCKET_PREMIUM, "monthly") => out.premium_monthly = b,
            _ => {}
        }
    }
    Ok(out)
}

/// Candidate model of a tier: the selected model if it is an enabled model of
/// the tier, else the enabled `is_default` model of the tier, else the first
/// enabled model of the tier.
fn candidate<'a>(policy: &'a PolicyView, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let cat = &policy.snapshot.model_catalog;
    cat.iter()
        .find(|m| m.enabled && m.tier == tier && m.id == selected)
        .or_else(|| cat.iter().find(|m| m.enabled && m.tier == tier && m.is_default()))
        .or_else(|| cat.iter().find(|m| m.enabled && m.tier == tier))
}

/// Settlement parameters of one terminal outcome.
#[derive(Debug, Clone)]
pub struct SettleParams {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub premium: bool,
    pub daily_start: Date,
    pub monthly_start: Date,
    pub reserved_credits: i64,
    pub committed_credits: i64,
    /// Actual token telemetry (actual settlements only).
    pub actual_tokens: Option<(i64, i64)>,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// One quota status entry.
#[derive(Debug, Clone)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: PeriodType,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Quota service (stateless; operates on the caller's runner).
#[derive(Debug, Clone)]
pub struct QuotaService {
    pub max_output_tokens_cap: u32,
    pub minimal_generation_floor: u32,
    pub web_search_daily_quota: u32,
    pub code_interpreter_daily_quota: u32,
    pub warning_threshold_pct: u8,
}

impl QuotaService {
    /// Preflight: pick the effective model with the downgrade cascade and
    /// check the daily tool quotas. Writes nothing.
    ///
    /// # Errors
    /// `QuotaExceeded` (tokens / `web_search` / `code_interpreter`), DB errors.
    #[allow(clippy::too_many_arguments, reason = "preflight inputs")]
    pub async fn preflight(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        selected_model: &str,
        policy: &PolicyView,
        limits: UserLimits,
        inputs: &QuotaInputs,
    ) -> Result<PreflightDecision, DomainError> {
        let now = clock::now();
        let daily_start = period_start(PeriodType::Daily, clock::to_time(now));
        let monthly_start = period_start(PeriodType::Monthly, clock::to_time(now));
        let rows = load_rows(runner, scope, tenant_id, user_id, daily_start, monthly_start).await?;
        let ks = policy.snapshot.kill_switches;
        let selected = policy.find(selected_model);
        let (start_tier, mut reason) = match selected {
            Some(m) if m.enabled => (m.tier, None),
            Some(m) => (m.tier, Some("model_disabled")),
            None => (ModelTier::Premium, Some("model_disabled")),
        };
        let cascade: &[ModelTier] = if start_tier == ModelTier::Premium {
            &[ModelTier::Premium, ModelTier::Standard]
        } else {
            &[ModelTier::Standard]
        };
        for &tier in cascade {
            if tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
                if reason.is_none() {
                    reason = Some(if ks.force_standard_tier { "force_standard_tier" } else { "disable_premium_tier" });
                }
                continue;
            }
            let Some(c) = candidate(policy, tier, selected_model) else {
                continue;
            };
            let tools = tool_flags(c, ks, inputs);
            let mot = i64::from(std::cmp::min(c.max_output_tokens, self.max_output_tokens_cap));
            let est = reserve_estimate(
                inputs.message_bytes,
                inputs.prior_context_tokens,
                Surcharges {
                    file_search: tools.file_search,
                    web_search: tools.web_search,
                    code_interpreter: tools.code_interpreter,
                    images: inputs.images,
                },
                &c.estimation_budgets,
                mot,
                c.input_tokens_credit_multiplier_micro,
                c.output_tokens_credit_multiplier_micro,
            );
            let credits = est.reserved_credits_micro.unwrap_or_else(|| {
                tracing::warn!(model = %c.id, "reserve cannot be computed; candidate treated as unavailable");
                i64::MAX
            });
            if rows.tier_available(tier == ModelTier::Premium, credits, &limits) {
                if tools.web_search
                    && rows.total_daily.web_search_calls >= i64::from(self.web_search_daily_quota)
                {
                    return Err(DomainError::QuotaExceeded(quota_scope::WEB_SEARCH));
                }
                if tools.code_interpreter
                    && rows.total_daily.code_interpreter_calls >= i64::from(self.code_interpreter_daily_quota)
                {
                    return Err(DomainError::QuotaExceeded(quota_scope::CODE_INTERPRETER));
                }
                let floor = std::cmp::min(i64::from(self.minimal_generation_floor), mot);
                return Ok(PreflightDecision {
                    selected_model: selected_model.to_owned(),
                    effective: c.clone(),
                    downgrade_reason: reason,
                    reserve: est,
                    reserved_credits_micro: credits,
                    policy_version: policy.version,
                    daily_start,
                    monthly_start,
                    tools,
                    floor_applied: floor,
                    limits,
                });
            }
            if tier == ModelTier::Premium && reason.is_none() {
                reason = Some("premium_quota_exhausted");
            }
        }
        Err(DomainError::QuotaExceeded(quota_scope::TOKENS))
    }

    /// Book the reserve of `decision` and re-check every bucket/period of the
    /// decision. Run inside the reserve transaction; an error rolls it back.
    ///
    /// # Errors
    /// `QuotaExceeded(tokens)` when a bucket is over its limit, DB errors.
    pub async fn reserve(
        &self,
        tx: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        decision: &PreflightDecision,
    ) -> Result<(), DomainError> {
        let premium = decision.is_premium();
        let credits = decision.reserved_credits_micro;
        for (bucket, include) in [(BUCKET_TOTAL, true), (BUCKET_PREMIUM, premium)] {
            if !include {
                continue;
            }
            for (pt, start) in [("daily", decision.daily_start), ("monthly", decision.monthly_start)] {
                ensure_row(tx, scope, tenant_id, user_id, pt, start, bucket).await?;
                quota_usage::Entity::update_many()
                    .secure()
                    .col_expr(
                        quota_usage::Column::ReservedCreditsMicro,
                        Expr::col(quota_usage::Column::ReservedCreditsMicro).add(credits),
                    )
                    .col_expr(quota_usage::Column::UpdatedAt, Expr::value(clock::now()))
                    .filter(row_key(tenant_id, user_id, pt, start, bucket))
                    .scope_with(scope)
                    .exec(tx)
                    .await?;
            }
        }
        let rows = load_rows(tx, scope, tenant_id, user_id, decision.daily_start, decision.monthly_start).await?;
        if !rows.tier_available(premium, 0, &decision.limits) {
            return Err(DomainError::QuotaExceeded(quota_scope::TOKENS));
        }
        Ok(())
    }

    /// Settle one terminal outcome (inside the finalization transaction).
    ///
    /// # Errors
    /// DB errors.
    pub async fn settle(&self, tx: &impl DBRunner, scope: &AccessScope, p: &SettleParams) -> Result<(), DomainError> {
        let now = clock::now();
        for (bucket, include) in [(BUCKET_TOTAL, true), (BUCKET_PREMIUM, p.premium)] {
            if !include {
                continue;
            }
            for (pt, start) in [("daily", p.daily_start), ("monthly", p.monthly_start)] {
                ensure_row(tx, scope, p.tenant_id, p.user_id, pt, start, bucket).await?;
                let mut upd = quota_usage::Entity::update_many()
                    .secure()
                    .col_expr(
                        quota_usage::Column::ReservedCreditsMicro,
                        Expr::col(quota_usage::Column::ReservedCreditsMicro).sub(p.reserved_credits),
                    )
                    .col_expr(
                        quota_usage::Column::SpentCreditsMicro,
                        Expr::col(quota_usage::Column::SpentCreditsMicro).add(p.committed_credits),
                    )
                    .col_expr(quota_usage::Column::Calls, Expr::col(quota_usage::Column::Calls).add(1))
                    .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now));
                if bucket == BUCKET_TOTAL {
                    if let Some((i, o)) = p.actual_tokens {
                        upd = upd
                            .col_expr(quota_usage::Column::InputTokens, Expr::col(quota_usage::Column::InputTokens).add(std::cmp::max(i, 0)))
                            .col_expr(quota_usage::Column::OutputTokens, Expr::col(quota_usage::Column::OutputTokens).add(std::cmp::max(o, 0)));
                    }
                    if p.web_search_calls > 0 {
                        upd = upd.col_expr(
                            quota_usage::Column::WebSearchCalls,
                            Expr::col(quota_usage::Column::WebSearchCalls).add(p.web_search_calls),
                        );
                    }
                    if p.code_interpreter_calls > 0 {
                        upd = upd.col_expr(
                            quota_usage::Column::CodeInterpreterCalls,
                            Expr::col(quota_usage::Column::CodeInterpreterCalls).add(p.code_interpreter_calls),
                        );
                    }
                }
                upd.filter(row_key(p.tenant_id, p.user_id, pt, start, bucket))
                    .scope_with(scope)
                    .exec(tx)
                    .await?;
            }
        }
        Ok(())
    }

    /// Per-tier, per-period status (`premium` then `total`).
    ///
    /// # Errors
    /// DB errors.
    pub async fn status(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
    ) -> Result<Vec<PeriodStatus>, DomainError> {
        let now = clock::to_time(clock::now());
        let daily = period_start(PeriodType::Daily, now);
        let monthly = period_start(PeriodType::Monthly, now);
        let rows = load_rows(runner, scope, tenant_id, user_id, daily, monthly).await?;
        let mut out = Vec::new();
        let entries: [(&'static str, TierLimits, Bucket, Bucket); 2] = [
            ("premium", limits.premium, rows.premium_daily, rows.premium_monthly),
            ("total", limits.standard, rows.total_daily, rows.total_monthly),
        ];
        for (tier, lim, d, m) in entries {
            for (period, limit, b) in [
                (PeriodType::Daily, lim.limit_daily_credits_micro, d),
                (PeriodType::Monthly, lim.limit_monthly_credits_micro, m),
            ] {
                if limit <= 0 {
                    continue;
                }
                let used = std::cmp::max(b.spent.saturating_add(b.reserved), 0);
                let remaining = std::cmp::max(limit.saturating_sub(used), 0);
                let pct = remaining_pct(remaining, limit);
                let warning = pct <= 100 - u32::from(self.warning_threshold_pct);
                out.push(PeriodStatus {
                    tier,
                    period,
                    limit,
                    used,
                    remaining,
                    remaining_percentage: pct,
                    next_reset: next_reset(period, now),
                    warning,
                    exhausted: pct == 0,
                });
            }
        }
        Ok(out)
    }
}

/// Floored integer percentage of the remaining credits (0..=100).
#[must_use]
#[allow(clippy::integer_division, reason = "deliberately floored percentage")]
pub fn remaining_pct(remaining: i64, limit: i64) -> u32 {
    if limit <= 0 {
        return 0;
    }
    let pct = i128::from(std::cmp::max(remaining, 0)) * 100 / i128::from(limit);
    u32::try_from(pct.clamp(0, 100)).unwrap_or(0)
}

fn row_key(tenant_id: Uuid, user_id: Uuid, pt: &str, start: Date, bucket: &str) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant_id))
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(pt))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

async fn ensure_row(
    tx: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    pt: &str,
    start: Date,
    bucket: &str,
) -> Result<(), DomainError> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::now_v7()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set(pt.to_owned()),
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
        updated_at: Set(clock::now()),
    };
    let oc = OnConflict::columns([
        quota_usage::Column::TenantId,
        quota_usage::Column::UserId,
        quota_usage::Column::PeriodType,
        quota_usage::Column::PeriodStart,
        quota_usage::Column::Bucket,
    ])
    .do_nothing()
    .to_owned();
    match quota_usage::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .on_conflict_raw(oc)
        .exec(tx)
        .await
    {
        Ok(_) | Err(ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_floors() {
        assert_eq!(remaining_pct(0, 100), 0);
        assert_eq!(remaining_pct(9, 1000), 0);
        assert_eq!(remaining_pct(10, 1000), 1);
        assert_eq!(remaining_pct(1000, 1000), 100);
        assert_eq!(remaining_pct(-5, 1000), 0);
    }

    #[test]
    fn bucket_fit() {
        let b = Bucket { spent: 20, reserved: 2, ..Bucket::default() };
        assert!(fits(b, 3, 25));
        assert!(!fits(b, 4, 25));
    }
}
