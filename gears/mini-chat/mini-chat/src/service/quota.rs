//! Quota service: usage reads, preflight cascade, reserve writes with the
//! limit re-check, settlement and the status endpoint.

use mini_chat_sdk::{ModelTier, PolicySnapshot, UserLimits};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use crate::domain::authz;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::quota::{
    BUCKET_PREMIUM, BUCKET_TOTAL, BucketUsage, Period, PeriodStarts, PreflightDecision,
    RequestFacts, Settlement, TierStatus, UsageView, bucket_limit, quota_status,
    resolve_effective_model,
};
use crate::infra::db::entity::quota_usage;
use crate::infra::repo::now_utc;

fn bucket_static(b: &str) -> &'static str {
    if b == BUCKET_PREMIUM {
        BUCKET_PREMIUM
    } else {
        BUCKET_TOTAL
    }
}

/// Read the usage rows of a user for the given period starts.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn load_usage(
    r: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    user: Uuid,
    periods: &PeriodStarts,
) -> DomainResult<UsageView> {
    let rows = quota_usage::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(quota_usage::Column::TenantId.eq(tenant))
                .add(quota_usage::Column::UserId.eq(user))
                .add(
                    Condition::any()
                        .add(
                            Condition::all()
                                .add(quota_usage::Column::PeriodType.eq("daily"))
                                .add(quota_usage::Column::PeriodStart.eq(periods.daily)),
                        )
                        .add(
                            Condition::all()
                                .add(quota_usage::Column::PeriodType.eq("monthly"))
                                .add(quota_usage::Column::PeriodStart.eq(periods.monthly)),
                        ),
                ),
        )
        .all(r)
        .await?;
    let mut view = UsageView::default();
    for row in rows {
        let p = if row.period_type == "daily" {
            Period::Daily
        } else {
            Period::Monthly
        };
        if row.bucket != BUCKET_TOTAL && row.bucket != BUCKET_PREMIUM {
            continue;
        }
        view.rows.insert(
            (p, bucket_static(&row.bucket)),
            BucketUsage {
                spent: row.spent_credits_micro,
                reserved: row.reserved_credits_micro,
                web_search_calls: i64::from(row.web_search_calls),
                code_interpreter_calls: i64::from(row.code_interpreter_calls),
            },
        );
    }
    Ok(view)
}

async fn ensure_row(
    r: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    user: Uuid,
    p: Period,
    start: time::Date,
    bucket: &str,
) -> DomainResult<()> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::now_v7()),
        tenant_id: Set(tenant),
        user_id: Set(user),
        period_type: Set(p.as_str().to_owned()),
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
        updated_at: Set(now_utc()),
    };
    let res = quota_usage::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
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
        .exec(r)
        .await;
    match res {
        Ok(_) | Err(toolkit_db::secure::ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn row_filter(tenant: Uuid, user: Uuid, p: Period, start: time::Date, bucket: &str) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant))
        .add(quota_usage::Column::UserId.eq(user))
        .add(quota_usage::Column::PeriodType.eq(p.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

fn buckets_for(premium: bool) -> &'static [&'static str] {
    if premium {
        &[BUCKET_TOTAL, BUCKET_PREMIUM]
    } else {
        &[BUCKET_TOTAL]
    }
}

/// Book a reserve and re-check the limits in the same transaction.
///
/// # Errors
/// 429 `quota_exceeded` (`tokens`) when a bucket is over its limit after the
/// increment.
#[allow(clippy::too_many_arguments)]
pub async fn reserve_in_tx(
    r: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    user: Uuid,
    periods: &PeriodStarts,
    premium: bool,
    credits: i64,
    limits: &UserLimits,
) -> DomainResult<()> {
    let now = now_utc();
    for bucket in buckets_for(premium) {
        for p in Period::ALL {
            let start = periods.get(p);
            ensure_row(r, scope, tenant, user, p, start, bucket).await?;
            quota_usage::Entity::update_many()
                .secure()
                .scope_with(scope)
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro).add(credits),
                )
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now))
                .filter(row_filter(tenant, user, p, start, bucket))
                .exec(r)
                .await?;
        }
    }
    let usage = load_usage(r, scope, tenant, user, periods).await?;
    for bucket in buckets_for(premium) {
        for p in Period::ALL {
            let u = usage.get(p, bucket_static(bucket));
            if u.spent.saturating_add(u.reserved) > bucket_limit(limits, bucket, p) {
                return Err(DomainError::quota_exceeded("tokens"));
            }
        }
    }
    Ok(())
}

/// Tool-call counters added to bucket `total` at settlement.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCallCounts {
    pub web_search: i64,
    pub code_interpreter: i64,
}

/// Apply a settlement to the bucket rows of the turn's periods.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
#[allow(clippy::too_many_arguments)]
pub async fn settle_in_tx(
    r: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    user: Uuid,
    periods: &PeriodStarts,
    premium: bool,
    turn_reserved: i64,
    s: &Settlement,
    tools: ToolCallCounts,
) -> DomainResult<()> {
    let now = now_utc();
    for bucket in buckets_for(premium) {
        for p in Period::ALL {
            let start = periods.get(p);
            ensure_row(r, scope, tenant, user, p, start, bucket).await?;
            let mut upd = quota_usage::Entity::update_many()
                .secure()
                .scope_with(scope)
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    Expr::col(quota_usage::Column::ReservedCreditsMicro).sub(turn_reserved),
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
                            Expr::col(quota_usage::Column::CodeInterpreterCalls)
                                .add(tools.code_interpreter),
                        );
                }
            }
            upd.filter(row_filter(tenant, user, p, start, bucket))
                .exec(r)
                .await?;
        }
    }
    Ok(())
}

/// Outcome of the quota preflight.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub snapshot: PolicySnapshot,
    pub limits: UserLimits,
    pub decision: PreflightDecision,
    pub periods: PeriodStarts,
}

impl Preflight {
    #[must_use]
    pub fn effective_is_premium(&self) -> bool {
        self.decision.effective.tier == ModelTier::Premium
    }
}

impl AppState {
    /// Run the cascade and the daily tool-quota checks.
    ///
    /// # Errors
    /// 429 `quota_exceeded` with scope `tokens`, `web_search` or
    /// `code_interpreter`; 500 on policy failure.
    pub async fn quota_preflight(
        &self,
        ctx: &SecurityContext,
        snapshot: PolicySnapshot,
        selected_model: &str,
        facts: &RequestFacts,
    ) -> DomainResult<Preflight> {
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let periods = PeriodStarts::at(OffsetDateTime::now_utc());
        let scope = AccessScope::for_tenant(ctx.subject_tenant_id());
        let conn = self.conn()?;
        let usage = load_usage(
            &conn,
            &scope,
            ctx.subject_tenant_id(),
            ctx.subject_id(),
            &periods,
        )
        .await?;
        let decision = resolve_effective_model(
            &snapshot,
            &limits,
            &usage,
            selected_model,
            facts,
            self.max_output_cap(),
        )
        .ok_or_else(|| DomainError::quota_exceeded("tokens"))?;
        let daily = usage.get(Period::Daily, BUCKET_TOTAL);
        if decision.reserve.tools.web_search
            && daily.web_search_calls >= i64::from(self.cfg.quota.web_search_daily_quota)
        {
            return Err(DomainError::quota_exceeded("web_search"));
        }
        if decision.reserve.tools.code_interpreter
            && daily.code_interpreter_calls
                >= i64::from(self.cfg.quota.code_interpreter_daily_quota)
        {
            return Err(DomainError::quota_exceeded("code_interpreter"));
        }
        Ok(Preflight {
            snapshot,
            limits,
            decision,
            periods,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn get_quota_status(&self, ctx: &SecurityContext) -> DomainResult<Vec<TierStatus>> {
        let scope = authz::quota_scope(&self.enforcer, ctx).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let now = OffsetDateTime::now_utc();
        let periods = PeriodStarts::at(now);
        let conn = self.conn()?;
        let usage = load_usage(
            &conn,
            &scope,
            ctx.subject_tenant_id(),
            ctx.subject_id(),
            &periods,
        )
        .await?;
        Ok(quota_status(
            &limits,
            &usage,
            self.cfg.quota.warning_threshold_pct,
            now,
        ))
    }
}
