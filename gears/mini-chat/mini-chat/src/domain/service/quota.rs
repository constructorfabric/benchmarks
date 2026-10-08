//! Quota service: preflight downgrade cascade, reserve, settlement and the
//! quota status endpoint (DESIGN §5.4).

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{ColumnTrait, Condition, Set};
#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::Service;
use crate::domain::billing::{self, Reserve, ReserveInputs};
use crate::domain::clock;
use crate::domain::error::{DomainError, DomainResult, QuotaScope};
use crate::infra::storage::entity::quota_usage;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";

/// Period starts of a turn (fixed at preflight).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periods {
    pub daily: Date,
    pub monthly: Date,
}

impl Periods {
    #[must_use]
    pub fn at(ts: OffsetDateTime) -> Self {
        Self {
            daily: clock::utc_date(ts),
            monthly: clock::utc_month_start(ts),
        }
    }

    fn list(self) -> [(&'static str, Date); 2] {
        [(PERIOD_DAILY, self.daily), (PERIOD_MONTHLY, self.monthly)]
    }
}

/// One bucket row (zero when missing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketState {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// The four bucket rows of the current periods.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageState {
    pub total_daily: BucketState,
    pub total_monthly: BucketState,
    pub premium_daily: BucketState,
    pub premium_monthly: BucketState,
}

/// Context-independent inputs of the cascade.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct PreflightInputs<'a> {
    pub selected_model: &'a str,
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub image_count: u32,
    pub has_ready_documents: bool,
    pub has_ready_code_files: bool,
    pub web_search_requested: bool,
}

/// Tools a candidate model would get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolPlan {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Outcome of the preflight cascade.
#[derive(Debug, Clone)]
pub struct QuotaDecision {
    pub effective: ModelCatalogEntry,
    pub selected_model: String,
    pub downgrade_reason: Option<String>,
    pub reserve: Reserve,
    pub tools: ToolPlan,
    pub policy_version: u64,
    pub limits: UserLimits,
    pub periods: Periods,
    pub minimal_generation_floor_applied: i64,
}

impl QuotaDecision {
    /// `true` for a downgrade (other model or a downgrade reason).
    #[must_use]
    pub fn is_downgrade(&self) -> bool {
        self.effective.id != self.selected_model || self.downgrade_reason.is_some()
    }

    #[must_use]
    pub fn is_premium(&self) -> bool {
        self.effective.tier == ModelTier::Premium
    }
}

/// Tools a model gets for a chat (kill switches and `tool_support`).
#[must_use]
pub fn tool_plan(
    model: &ModelCatalogEntry,
    snapshot: &PolicySnapshot,
    has_ready_documents: bool,
    has_ready_code_files: bool,
    web_search_requested: bool,
) -> ToolPlan {
    let ts = model.tool_support();
    let ks = &snapshot.kill_switches;
    ToolPlan {
        file_search: has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: web_search_requested && ts.web_search && !ks.disable_web_search,
        code_interpreter: has_ready_code_files
            && ts.code_interpreter
            && !ks.disable_code_interpreter,
    }
}

/// `min(model.max_output_tokens, streaming.max_output_tokens)`.
#[must_use]
pub fn max_output_applied(model: &ModelCatalogEntry, config_max: u32) -> i64 {
    let m = if model.max_output_tokens == 0 {
        config_max
    } else {
        model.max_output_tokens.min(config_max)
    };
    i64::from(m)
}

fn limit_of(limits: &UserLimits, bucket: &str, period: &str) -> i64 {
    let tier = if bucket == BUCKET_PREMIUM {
        &limits.premium
    } else {
        &limits.standard
    };
    if period == PERIOD_DAILY {
        tier.limit_daily_credits_micro
    } else {
        tier.limit_monthly_credits_micro
    }
}

fn fits(state: BucketState, extra: i64, limit: i64) -> bool {
    state
        .spent
        .saturating_add(state.reserved)
        .saturating_add(extra)
        <= limit
}

/// Pick the candidate of a tier (enabled models only).
fn candidate<'a>(
    snapshot: &'a PolicySnapshot,
    tier: ModelTier,
    selected: &str,
) -> Option<&'a ModelCatalogEntry> {
    let enabled = || {
        snapshot
            .model_catalog
            .iter()
            .filter(move |m| m.enabled && m.tier == tier)
    };
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Run the downgrade cascade (DESIGN §"Downgrade Decision Flow").
///
/// # Errors
/// `QuotaExceeded{tokens}` when no tier is available; `Internal` on credit
/// overflow.
pub fn resolve_effective_model(
    inputs: &PreflightInputs<'_>,
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    usage: &UsageState,
    config_max_output: u32,
    minimal_floor: u32,
    periods: Periods,
) -> DomainResult<QuotaDecision> {
    let (start_tier, mut reason) = match snapshot.find_model(inputs.selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled".to_owned())),
        None => (ModelTier::Premium, Some("model_disabled".to_owned())),
    };
    let cascade: &[ModelTier] = if start_tier == ModelTier::Premium {
        &[ModelTier::Premium, ModelTier::Standard]
    } else {
        &[ModelTier::Standard]
    };
    let ks = &snapshot.kill_switches;
    for &tier in cascade {
        if tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
            if reason.is_none() {
                reason = Some(if ks.force_standard_tier {
                    "force_standard_tier".to_owned()
                } else {
                    "disable_premium_tier".to_owned()
                });
            }
            continue;
        }
        let Some(model) = candidate(snapshot, tier, inputs.selected_model) else {
            continue;
        };
        let tools = tool_plan(
            model,
            snapshot,
            inputs.has_ready_documents,
            inputs.has_ready_code_files,
            inputs.web_search_requested,
        );
        let max_out = max_output_applied(model, config_max_output);
        let reserve = billing::compute_reserve(
            &ReserveInputs {
                message_bytes: inputs.message_bytes,
                prior_context_tokens: inputs.prior_context_tokens,
                image_count: inputs.image_count,
                file_search: tools.file_search,
                web_search: tools.web_search,
                code_interpreter: tools.code_interpreter,
            },
            &model.estimation_budgets,
            max_out,
            model.input_tokens_credit_multiplier_micro,
            model.output_tokens_credit_multiplier_micro,
        )
        .map_err(|e| DomainError::internal(format!("reserve computation: {e}")))?;
        let extra = reserve.reserved_credits_micro;
        let total_ok = fits(
            usage.total_daily,
            extra,
            limit_of(limits, BUCKET_TOTAL, PERIOD_DAILY),
        ) && fits(
            usage.total_monthly,
            extra,
            limit_of(limits, BUCKET_TOTAL, PERIOD_MONTHLY),
        );
        let available = if tier == ModelTier::Premium {
            total_ok
                && fits(
                    usage.premium_daily,
                    extra,
                    limit_of(limits, BUCKET_PREMIUM, PERIOD_DAILY),
                )
                && fits(
                    usage.premium_monthly,
                    extra,
                    limit_of(limits, BUCKET_PREMIUM, PERIOD_MONTHLY),
                )
        } else {
            total_ok
        };
        if !available {
            if tier == ModelTier::Premium && reason.is_none() {
                reason = Some("premium_quota_exhausted".to_owned());
            }
            continue;
        }
        let floor = i64::from(minimal_floor).min(max_out);
        return Ok(QuotaDecision {
            effective: model.clone(),
            selected_model: inputs.selected_model.to_owned(),
            downgrade_reason: reason,
            reserve,
            tools,
            policy_version: snapshot.policy_version,
            limits: limits.clone(),
            periods,
            minimal_generation_floor_applied: floor,
        });
    }
    Err(DomainError::QuotaExceeded {
        scope: QuotaScope::Tokens,
    })
}

/// Deltas applied to one bucket row.
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

fn add(col: quota_usage::Column, delta: impl Into<sea_orm::Value>) -> SimpleExpr {
    sea_orm::ExprTrait::add(Expr::col((quota_usage::Entity, col)), delta.into())
}

/// Scope of a user's quota rows.
#[must_use]
pub fn user_quota_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    use toolkit_security::{ScopeConstraint, ScopeFilter, pep_properties};
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant_id),
        ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
    ]))
}

impl Service {
    /// Read the user's four bucket rows of the periods.
    pub(crate) async fn usage_state<R: DBRunner>(
        runner: &R,
        tenant_id: Uuid,
        user_id: Uuid,
        periods: Periods,
    ) -> DomainResult<UsageState> {
        let rows = quota_usage::Entity::find()
            .secure()
            .scope_with(&user_quota_scope(tenant_id, user_id))
            .filter(
                Condition::all()
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
        let mut state = UsageState::default();
        for r in rows {
            let b = BucketState {
                spent: r.spent_credits_micro,
                reserved: r.reserved_credits_micro,
                web_search_calls: i64::from(r.web_search_calls),
                code_interpreter_calls: i64::from(r.code_interpreter_calls),
            };
            match (r.bucket.as_str(), r.period_type.as_str()) {
                (BUCKET_TOTAL, PERIOD_DAILY) => state.total_daily = b,
                (BUCKET_TOTAL, PERIOD_MONTHLY) => state.total_monthly = b,
                (BUCKET_PREMIUM, PERIOD_DAILY) => state.premium_daily = b,
                (BUCKET_PREMIUM, PERIOD_MONTHLY) => state.premium_monthly = b,
                _ => {}
            }
        }
        Ok(state)
    }

    /// Apply a delta to one bucket row (upsert).
    pub(crate) async fn bump_bucket<R: DBRunner>(
        runner: &R,
        tenant_id: Uuid,
        user_id: Uuid,
        period_type: &str,
        period_start: Date,
        bucket: &str,
        d: BucketDelta,
    ) -> DomainResult<()> {
        use quota_usage::Column as C;
        let now = clock::now();
        let am = quota_usage::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_id),
            period_type: Set(period_type.to_owned()),
            period_start: Set(period_start),
            bucket: Set(bucket.to_owned()),
            spent_credits_micro: Set(d.spent.max(0)),
            reserved_credits_micro: Set(d.reserved.max(0)),
            calls: Set(d.calls.max(0)),
            input_tokens: Set(d.input_tokens.max(0)),
            output_tokens: Set(d.output_tokens.max(0)),
            file_search_calls: Set(0),
            web_search_calls: Set(d.web_search_calls.max(0)),
            code_interpreter_calls: Set(d.code_interpreter_calls.max(0)),
            rag_retrieval_calls: Set(0),
            image_inputs: Set(0),
            image_upload_bytes: Set(0),
            updated_at: Set(now),
        };
        let oc = SecureOnConflict::<quota_usage::Entity>::columns([
            C::TenantId,
            C::UserId,
            C::PeriodType,
            C::PeriodStart,
            C::Bucket,
        ])
        .value(
            C::ReservedCreditsMicro,
            add(C::ReservedCreditsMicro, d.reserved),
        )?
        .value(C::SpentCreditsMicro, add(C::SpentCreditsMicro, d.spent))?
        .value(C::Calls, add(C::Calls, d.calls))?
        .value(C::InputTokens, add(C::InputTokens, d.input_tokens))?
        .value(C::OutputTokens, add(C::OutputTokens, d.output_tokens))?
        .value(
            C::WebSearchCalls,
            add(C::WebSearchCalls, d.web_search_calls),
        )?
        .value(
            C::CodeInterpreterCalls,
            add(C::CodeInterpreterCalls, d.code_interpreter_calls),
        )?
        .value(C::UpdatedAt, Expr::value(now))?;
        let scope = user_quota_scope(tenant_id, user_id);
        quota_usage::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict(oc)
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Book the reserve of a decision and re-check the limits in the same
    /// transaction (TOCTOU guard).
    ///
    /// # Errors
    /// `QuotaExceeded{tokens}` when a bucket is over its limit after the
    /// increment (the caller rolls back).
    pub(crate) async fn write_reserve<R: DBRunner>(
        runner: &R,
        tenant_id: Uuid,
        user_id: Uuid,
        decision: &QuotaDecision,
    ) -> DomainResult<()> {
        let x = decision.reserve.reserved_credits_micro;
        let mut buckets = vec![BUCKET_TOTAL];
        if decision.is_premium() {
            buckets.push(BUCKET_PREMIUM);
        }
        for bucket in &buckets {
            for (ptype, pstart) in decision.periods.list() {
                Self::bump_bucket(
                    runner,
                    tenant_id,
                    user_id,
                    ptype,
                    pstart,
                    bucket,
                    BucketDelta {
                        reserved: x,
                        ..BucketDelta::default()
                    },
                )
                .await?;
            }
        }
        let state = Self::usage_state(runner, tenant_id, user_id, decision.periods).await?;
        let check = |s: BucketState, bucket: &str, period: &str| {
            fits(s, 0, limit_of(&decision.limits, bucket, period))
        };
        let mut ok = check(state.total_daily, BUCKET_TOTAL, PERIOD_DAILY)
            && check(state.total_monthly, BUCKET_TOTAL, PERIOD_MONTHLY);
        if decision.is_premium() {
            ok = ok
                && check(state.premium_daily, BUCKET_PREMIUM, PERIOD_DAILY)
                && check(state.premium_monthly, BUCKET_PREMIUM, PERIOD_MONTHLY);
        }
        if !ok {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens,
            });
        }
        Ok(())
    }

    /// Settle a turn's reserve on its bucket rows.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn settle_buckets<R: DBRunner>(
        runner: &R,
        tenant_id: Uuid,
        user_id: Uuid,
        periods: Periods,
        premium: bool,
        turn_reserved: i64,
        charged: i64,
        tokens: Option<(i64, i64)>,
        tool_calls: Option<(i32, i32)>,
    ) -> DomainResult<()> {
        let (input_tokens, output_tokens) = tokens.unwrap_or((0, 0));
        let (ws, ci) = tool_calls.unwrap_or((0, 0));
        for (ptype, pstart) in periods.list() {
            Self::bump_bucket(
                runner,
                tenant_id,
                user_id,
                ptype,
                pstart,
                BUCKET_TOTAL,
                BucketDelta {
                    reserved: -turn_reserved,
                    spent: charged,
                    calls: 1,
                    input_tokens,
                    output_tokens,
                    web_search_calls: ws,
                    code_interpreter_calls: ci,
                },
            )
            .await?;
            if premium {
                Self::bump_bucket(
                    runner,
                    tenant_id,
                    user_id,
                    ptype,
                    pstart,
                    BUCKET_PREMIUM,
                    BucketDelta {
                        reserved: -turn_reserved,
                        spent: charged,
                        calls: 1,
                        ..BucketDelta::default()
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Daily tool quota checks (only for tools sent with the turn).
    ///
    /// # Errors
    /// `QuotaExceeded{web_search|code_interpreter}`.
    pub(crate) fn check_tool_quotas(
        &self,
        usage: &UsageState,
        tools: ToolPlan,
    ) -> DomainResult<()> {
        if tools.web_search
            && usage.total_daily.web_search_calls
                >= i64::from(self.cfg.quota.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::WebSearch,
            });
        }
        if tools.code_interpreter
            && usage.total_daily.code_interpreter_calls
                >= i64::from(self.cfg.quota.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::CodeInterpreter,
            });
        }
        Ok(())
    }

    /// Per-tier, per-period status rows (premium, then total).
    pub(crate) fn quota_status_rows(
        &self,
        limits: &UserLimits,
        usage: &UsageState,
        now: OffsetDateTime,
    ) -> Vec<TierStatus> {
        let threshold = self.cfg.quota.warning_threshold_pct;
        let mk = |bucket: &'static str, daily: BucketState, monthly: BucketState| {
            let mut periods = Vec::new();
            for (period, state, next) in [
                (PERIOD_DAILY, daily, clock::next_daily_reset(now)),
                (PERIOD_MONTHLY, monthly, clock::next_monthly_reset(now)),
            ] {
                let limit = limit_of(limits, bucket, period);
                if limit <= 0 {
                    continue;
                }
                let used = state.spent.saturating_add(state.reserved);
                let pct = billing::remaining_percentage(limit, used);
                let (warning, exhausted) = billing::warning_flags(pct, threshold);
                periods.push(PeriodStatus {
                    period,
                    limit,
                    used,
                    remaining: (limit - used).max(0),
                    remaining_percentage: pct,
                    next_reset: next,
                    warning,
                    exhausted,
                });
            }
            TierStatus {
                tier: if bucket == BUCKET_PREMIUM {
                    "premium"
                } else {
                    "total"
                },
                periods,
            }
        };
        vec![
            mk(BUCKET_PREMIUM, usage.premium_daily, usage.premium_monthly),
            mk(BUCKET_TOTAL, usage.total_daily, usage.total_monthly),
        ]
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// Authz errors, `PolicyResolution`.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> DomainResult<Vec<TierStatus>> {
        let scope = self.authz.quota_scope(ctx).await?;
        let snapshot = self.snapshot(ctx.subject_id()).await?;
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let now = clock::now();
        let periods = Periods::at(now);
        let conn = self.db.conn()?;
        // The PEP scope must allow the caller's own rows.
        if scope.is_deny_all() {
            return Err(DomainError::AccessDenied);
        }
        let usage =
            Self::usage_state(&conn, ctx.subject_tenant_id(), ctx.subject_id(), periods).await?;
        Ok(self.quota_status_rows(&limits, &usage, now))
    }
}

/// Status of one period.
#[derive(Debug, Clone)]
pub struct PeriodStatus {
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u8,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Status of one tier.
#[derive(Debug, Clone)]
pub struct TierStatus {
    pub tier: &'static str,
    pub periods: Vec<PeriodStatus>,
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
