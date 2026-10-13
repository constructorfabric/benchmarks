//! Quota service: preflight cascade, reserve with re-check, settlement and status (DESIGN §5).

use std::collections::HashMap;
use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::chats::tenant_scope;
use super::{Core, now};
use crate::domain::authz;
use crate::domain::error::DomainError;
use crate::domain::quota_math::{
    Period, credits_micro_checked, estimate_text_tokens, mult, remaining_percentage, warning_flags,
};
use crate::infra::db::entities::quota_usage;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";

/// Inputs that gate tools (and their surcharges) independent of the candidate model.
#[derive(Debug, Clone, Copy, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-tool gate flags, not a state machine"
)]
pub struct ToolGates {
    pub has_ready_documents: bool,
    pub has_ready_code_interpreter_files: bool,
    pub web_search_requested: bool,
}

/// Tools sent to the provider for the effective model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-tool enable flags, not a state machine"
)]
pub struct ToolPlan {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

#[derive(Debug, Clone)]
pub struct PreflightInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub selected_model: String,
    pub message_bytes: usize,
    pub image_count: u32,
    pub prior_context_tokens: i64,
    pub gates: ToolGates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    Allow,
    Downgrade,
}

impl QuotaDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// Result of a successful preflight.
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub snapshot: Arc<PolicySnapshot>,
    pub limits: UserLimits,
    pub effective: ModelCatalogEntry,
    pub selected_model: String,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<&'static str>,
    pub tools: ToolPlan,
    pub max_output_tokens_applied: u32,
    pub est_input_tokens: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub floor_applied: u32,
    pub daily_start: Date,
    pub monthly_start: Date,
}

impl PreflightDecision {
    #[must_use]
    pub fn tier(&self) -> ModelTier {
        self.effective.tier
    }
}

/// Snapshot of the user's bucket rows for both periods.
#[derive(Debug, Clone, Default)]
pub struct UsageRows {
    rows: HashMap<(&'static str, &'static str), quota_usage::Model>,
}

impl UsageRows {
    fn get(&self, period: Period, bucket: &'static str) -> Option<&quota_usage::Model> {
        self.rows.get(&(period.as_str(), bucket))
    }

    fn used(&self, period: Period, bucket: &'static str) -> i64 {
        self.get(period, bucket).map_or(0, |r| {
            r.spent_credits_micro
                .saturating_add(r.reserved_credits_micro)
        })
    }

    fn daily_calls(&self, f: impl Fn(&quota_usage::Model) -> i32) -> i64 {
        self.get(Period::Daily, BUCKET_TOTAL)
            .map_or(0, |r| i64::from(f(r)))
    }
}

fn bucket_key(bucket: &str) -> &'static str {
    if bucket == BUCKET_PREMIUM {
        BUCKET_PREMIUM
    } else {
        BUCKET_TOTAL
    }
}

fn period_key(p: &str) -> &'static str {
    if p == "monthly" { "monthly" } else { "daily" }
}

/// Reads the user's rows for the given period starts.
///
/// # Errors
/// DB errors.
pub async fn load_usage_rows(
    db: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    daily: Date,
    monthly: Date,
) -> Result<UsageRows, DomainError> {
    let rows = quota_usage::Entity::find()
        .filter(
            Condition::all()
                .add(quota_usage::Column::TenantId.eq(tenant_id))
                .add(quota_usage::Column::UserId.eq(user_id))
                .add(
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
                ),
        )
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(db)
        .await?;
    let mut out = UsageRows::default();
    for r in rows {
        out.rows
            .insert((period_key(&r.period_type), bucket_key(&r.bucket)), r);
    }
    Ok(out)
}

fn limit_for(limits: &UserLimits, bucket: &str, period: Period) -> i64 {
    let t = if bucket == BUCKET_PREMIUM {
        limits.premium
    } else {
        limits.standard
    };
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

/// Reserve figures a candidate model would book.
#[derive(Debug, Clone, Copy)]
pub struct CandidateReserve {
    pub est_input_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
}

impl Core {
    /// Tool plan for a model given the gates and kill switches.
    #[must_use]
    pub fn tool_plan(m: &ModelCatalogEntry, snap: &PolicySnapshot, gates: ToolGates) -> ToolPlan {
        let ts = m.tool_support();
        let ks = snap.kill_switches;
        ToolPlan {
            file_search: gates.has_ready_documents && ts.file_search && !ks.disable_file_search,
            web_search: gates.web_search_requested && ts.web_search,
            code_interpreter: gates.has_ready_code_interpreter_files
                && ts.code_interpreter
                && !ks.disable_code_interpreter,
        }
    }

    /// `min(catalog max_output_tokens, streaming.max_output_tokens)`.
    #[must_use]
    pub fn max_output_applied(&self, m: &ModelCatalogEntry) -> u32 {
        std::cmp::max(
            std::cmp::min(m.max_output_tokens, self.cfg.streaming.max_output_tokens),
            1,
        )
    }

    /// Reserve of a candidate (DESIGN §5.4.1).
    #[must_use]
    pub fn candidate_reserve(
        &self,
        m: &ModelCatalogEntry,
        plan: ToolPlan,
        input: &PreflightInput,
    ) -> CandidateReserve {
        let b = &m.estimation_budgets;
        let mut est = estimate_text_tokens(input.message_bytes, b)
            .saturating_add(input.prior_context_tokens)
            .saturating_add(i64::from(input.image_count) * i64::from(b.image_token_budget));
        if plan.file_search {
            est = est.saturating_add(i64::from(b.tool_surcharge_tokens));
        }
        if plan.web_search {
            est = est.saturating_add(i64::from(b.web_search_surcharge_tokens));
        }
        if plan.code_interpreter {
            est = est.saturating_add(i64::from(b.code_interpreter_surcharge_tokens));
        }
        let max_out = self.max_output_applied(m);
        let credits = credits_micro_checked(
            est,
            i64::from(max_out),
            mult(m.input_tokens_credit_multiplier_micro),
            mult(m.output_tokens_credit_multiplier_micro),
        )
        .unwrap_or_else(|e| {
            tracing::warn!(model = %m.id, error = %e, "mini-chat: candidate reserve cannot be computed");
            i64::MAX
        });
        CandidateReserve {
            est_input_tokens: est,
            max_output_tokens_applied: max_out,
            reserve_tokens: est.saturating_add(i64::from(max_out)),
            reserved_credits_micro: credits,
        }
    }

    /// Preflight: snapshot, kill switches, cascade, daily tool quotas (DESIGN "Downgrade Decision Flow").
    ///
    /// # Errors
    /// 400 web search disabled, 429 quota, 500 policy.
    pub async fn preflight(
        &self,
        input: &PreflightInput,
    ) -> Result<PreflightDecision, DomainError> {
        let snapshot = Arc::new(self.policy.current_snapshot(input.user_id).await?);
        if input.gates.web_search_requested && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::feature_disabled("web_search"));
        }
        let limits = self
            .policy
            .user_limits(input.user_id, snapshot.policy_version)
            .await?;
        let ts = now();
        let daily = Period::Daily.start(ts);
        let monthly = Period::Monthly.start(ts);
        let conn = self.db.conn()?;
        let rows = load_usage_rows(&conn, input.tenant_id, input.user_id, daily, monthly).await?;

        let ks = snapshot.kill_switches;
        let selected = snapshot.find_model(&input.selected_model);
        let (start_tier, mut reason) = match selected {
            Some(m) if m.enabled => (m.tier, None),
            Some(m) => (m.tier, Some("model_disabled")),
            None => (ModelTier::Premium, Some("model_disabled")),
        };
        let cascade: &[ModelTier] = match start_tier {
            ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
            ModelTier::Standard => &[ModelTier::Standard],
        };
        let mut chosen: Option<(ModelCatalogEntry, CandidateReserve, ToolPlan)> = None;
        for &tier in cascade {
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
            let candidate = snapshot
                .model_catalog
                .iter()
                .find(|m| m.enabled && m.tier == tier && m.id == input.selected_model)
                .or_else(|| {
                    snapshot
                        .model_catalog
                        .iter()
                        .find(|m| m.enabled && m.tier == tier && m.is_default())
                })
                .or_else(|| {
                    snapshot
                        .model_catalog
                        .iter()
                        .find(|m| m.enabled && m.tier == tier)
                });
            let Some(candidate) = candidate else { continue };
            let plan = Self::tool_plan(candidate, &snapshot, input.gates);
            let r = self.candidate_reserve(candidate, plan, input);
            let available = buckets_for(tier).iter().all(|b| {
                Period::ALL.iter().all(|p| {
                    let limit = limit_for(&limits, b, *p);
                    rows.used(*p, b)
                        .checked_add(r.reserved_credits_micro)
                        .is_some_and(|need| need <= limit)
                })
            });
            if available {
                chosen = Some((candidate.clone(), r, plan));
                break;
            }
            if tier == ModelTier::Premium && reason.is_none() {
                reason = Some("premium_quota_exhausted");
            }
        }
        let Some((effective, r, tools)) = chosen else {
            self.metrics.quota_rejected("tokens");
            return Err(DomainError::quota_exceeded("tokens"));
        };
        if tools.web_search
            && rows.daily_calls(|r| r.web_search_calls)
                >= i64::from(self.cfg.quota.web_search_daily_quota)
        {
            self.metrics.quota_rejected("web_search");
            return Err(DomainError::quota_exceeded("web_search"));
        }
        if tools.code_interpreter
            && rows.daily_calls(|r| r.code_interpreter_calls)
                >= i64::from(self.cfg.quota.code_interpreter_daily_quota)
        {
            self.metrics.quota_rejected("code_interpreter");
            return Err(DomainError::quota_exceeded("code_interpreter"));
        }
        let decision = if effective.id == input.selected_model && reason.is_none() {
            QuotaDecision::Allow
        } else {
            QuotaDecision::Downgrade
        };
        if decision == QuotaDecision::Downgrade {
            self.metrics.downgraded(reason.unwrap_or("model_disabled"));
        }
        let floor_applied = std::cmp::min(
            self.cfg.estimation_budgets.minimal_generation_floor,
            r.max_output_tokens_applied,
        );
        Ok(PreflightDecision {
            snapshot,
            limits,
            effective,
            selected_model: input.selected_model.clone(),
            decision,
            downgrade_reason: if decision == QuotaDecision::Downgrade {
                reason.or(Some("model_disabled"))
            } else {
                None
            },
            tools,
            max_output_tokens_applied: r.max_output_tokens_applied,
            est_input_tokens: r.est_input_tokens,
            reserve_tokens: r.reserve_tokens,
            reserved_credits_micro: r.reserved_credits_micro,
            floor_applied,
            daily_start: daily,
            monthly_start: monthly,
        })
    }
}

async fn ensure_row(
    tx: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: Period,
    start: Date,
    bucket: &str,
) -> Result<(), DomainError> {
    let am = quota_usage::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant_id),
        user_id: ActiveValue::Set(user_id),
        period_type: ActiveValue::Set(period.as_str().to_owned()),
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
        updated_at: ActiveValue::Set(now()),
    };
    let res = quota_usage::Entity::insert(am)
        .secure()
        .scope_unchecked(&tenant_scope(tenant_id))?
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
        .exec(tx)
        .await;
    match res {
        Ok(_) | Err(toolkit_db::secure::ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn row_filter(
    tenant_id: Uuid,
    user_id: Uuid,
    period: Period,
    start: Date,
    bucket: &str,
) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant_id))
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(period.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

/// Reserve parameters persisted on the turn.
#[derive(Debug, Clone, Copy)]
pub struct ReserveSpec {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub tier: ModelTier,
    pub reserved_credits_micro: i64,
    pub daily_start: Date,
    pub monthly_start: Date,
}

/// Writes the reserve and re-checks the limits in the same transaction (429 on overflow).
///
/// # Errors
/// 429 `quota_exceeded` (tokens) or DB errors.
pub async fn reserve_in_tx(
    tx: &impl DBRunner,
    spec: ReserveSpec,
    limits: &UserLimits,
) -> Result<(), DomainError> {
    let scope = tenant_scope(spec.tenant_id);
    for (period, start) in [
        (Period::Daily, spec.daily_start),
        (Period::Monthly, spec.monthly_start),
    ] {
        for bucket in buckets_for(spec.tier) {
            ensure_row(tx, spec.tenant_id, spec.user_id, period, start, bucket).await?;
            quota_usage::Entity::update_many()
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro)
                        .add(spec.reserved_credits_micro),
                )
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now()))
                .filter(row_filter(
                    spec.tenant_id,
                    spec.user_id,
                    period,
                    start,
                    bucket,
                ))
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?;
        }
    }
    let rows = load_usage_rows(
        tx,
        spec.tenant_id,
        spec.user_id,
        spec.daily_start,
        spec.monthly_start,
    )
    .await?;
    for p in Period::ALL {
        for b in buckets_for(spec.tier) {
            if rows.used(p, b) > limit_for(limits, b, p) {
                return Err(DomainError::quota_exceeded("tokens"));
            }
        }
    }
    Ok(())
}

/// Settlement method.
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

/// One settlement against the reserve's bucket rows.
#[derive(Debug, Clone, Copy)]
pub struct Settlement {
    pub reserve: ReserveSpec,
    pub committed_credits_micro: i64,
    pub method: SettlementMethod,
    /// Actual token telemetry (actual settlements only).
    pub actual_tokens: Option<(i64, i64)>,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

/// Applies a settlement (reserve release + commit) to the bucket rows.
///
/// # Errors
/// DB errors.
pub async fn settle_in_tx(tx: &impl DBRunner, s: Settlement) -> Result<(), DomainError> {
    let r = s.reserve;
    let scope = tenant_scope(r.tenant_id);
    for (period, start) in [
        (Period::Daily, r.daily_start),
        (Period::Monthly, r.monthly_start),
    ] {
        for bucket in buckets_for(r.tier) {
            ensure_row(tx, r.tenant_id, r.user_id, period, start, bucket).await?;
            let mut upd = quota_usage::Entity::update_many()
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro)
                        .sub(r.reserved_credits_micro),
                )
                .col_expr(
                    quota_usage::Column::SpentCreditsMicro,
                    Expr::col(quota_usage::Column::SpentCreditsMicro)
                        .add(s.committed_credits_micro),
                )
                .col_expr(
                    quota_usage::Column::Calls,
                    Expr::col(quota_usage::Column::Calls).add(1),
                )
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now()));
            if *bucket == BUCKET_TOTAL {
                if let Some((i, o)) = s.actual_tokens {
                    upd = upd
                        .col_expr(
                            quota_usage::Column::InputTokens,
                            Expr::col(quota_usage::Column::InputTokens).add(i),
                        )
                        .col_expr(
                            quota_usage::Column::OutputTokens,
                            Expr::col(quota_usage::Column::OutputTokens).add(o),
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
                            Expr::col(quota_usage::Column::CodeInterpreterCalls)
                                .add(s.code_interpreter_calls),
                        );
                }
            }
            upd.filter(row_filter(r.tenant_id, r.user_id, period, start, bucket))
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?;
        }
    }
    Ok(())
}

/// One period of one tier in the status / warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: Period,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

impl Core {
    /// Computes per-tier, per-period status for a user (periods with limit <= 0 are skipped).
    ///
    /// # Errors
    /// Policy / DB errors.
    pub async fn quota_periods(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<PeriodStatus>, DomainError> {
        let snapshot = self.policy.current_snapshot(user_id).await?;
        let limits = self
            .policy
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        let ts = now();
        let conn = self.db.conn()?;
        let rows = load_usage_rows(
            &conn,
            tenant_id,
            user_id,
            Period::Daily.start(ts),
            Period::Monthly.start(ts),
        )
        .await?;
        let mut out = Vec::new();
        for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
            for p in Period::ALL {
                let limit = limit_for(&limits, bucket, p);
                if limit <= 0 {
                    continue;
                }
                let used = rows.used(p, bucket);
                let pct = remaining_percentage(limit, used);
                let (warning, exhausted) = warning_flags(pct, self.cfg.quota.warning_threshold_pct);
                out.push(PeriodStatus {
                    tier,
                    period: p,
                    limit,
                    used,
                    remaining: std::cmp::max(limit - used, 0),
                    remaining_percentage: pct,
                    next_reset: p.next_reset(ts),
                    warning,
                    exhausted,
                });
            }
        }
        Ok(out)
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// PEP / policy / DB errors.
    pub async fn quota_status(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<PeriodStatus>, DomainError> {
        authz::quota_scope(&self.enforcer, ctx).await?;
        self.quota_periods(ctx.subject_tenant_id(), ctx.subject_id())
            .await
    }
}
