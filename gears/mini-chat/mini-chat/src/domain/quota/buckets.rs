//! `quota_usage` bucket row access (DESIGN §3.7 `quota_usage`, "Bucket Semantics").

use mini_chat_sdk::{TierLimits, UserLimits};
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, SimpleExpr};
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait, QueryFilter, Set};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{BUCKET_PREMIUM, BUCKET_TOTAL, PERIOD_DAILY, PERIOD_MONTHLY, PeriodStarts};
use crate::domain::error::DomainError;
use crate::infra::db::entities::quota_usage::{self, Column};

/// Scope of one user's quota rows.
pub(super) fn user_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// `(period_type, period_start)` pairs of the enforced periods.
pub(super) fn periods_of(periods: PeriodStarts) -> [(&'static str, Date); 2] {
    [(PERIOD_DAILY, periods.daily), (PERIOD_MONTHLY, periods.monthly)]
}

/// Limit of a bucket for a period (`total` → standard limits, `tier:premium` → premium limits).
pub(super) fn limit_of(limits: &UserLimits, bucket: &str, period_type: &str) -> i64 {
    let tier: &TierLimits = if bucket == BUCKET_PREMIUM { &limits.premium } else { &limits.standard };
    if period_type == PERIOD_DAILY { tier.limit_daily_credits_micro } else { tier.limit_monthly_credits_micro }
}

/// Rows of the user for the given periods (all buckets).
#[derive(Debug, Clone, Default)]
pub(super) struct Rows(pub Vec<quota_usage::Model>);

impl Rows {
    pub(super) fn get(&self, period_type: &str, bucket: &str) -> Option<&quota_usage::Model> {
        self.0.iter().find(|r| r.period_type == period_type && r.bucket == bucket)
    }

    /// `spent + reserved` of a bucket row (0 when missing), widened to avoid overflow.
    pub(super) fn used(&self, period_type: &str, bucket: &str) -> i128 {
        self.get(period_type, bucket)
            .map_or(0, |r| i128::from(r.spent_credits_micro) + i128::from(r.reserved_credits_micro))
    }

    /// `true` when `spent + reserved + extra <= limit` for the bucket in every period.
    pub(super) fn fits(&self, limits: &UserLimits, bucket: &str, periods: PeriodStarts, extra: i64) -> bool {
        periods_of(periods).iter().all(|(period_type, _)| {
            self.used(period_type, bucket) + i128::from(extra) <= i128::from(limit_of(limits, bucket, period_type))
        })
    }
}

fn key_condition(tenant_id: Uuid, user_id: Uuid, period_type: &str, period_start: Date, bucket: &str) -> Condition {
    Condition::all()
        .add(Column::TenantId.eq(tenant_id))
        .add(Column::UserId.eq(user_id))
        .add(Column::PeriodType.eq(period_type))
        .add(Column::PeriodStart.eq(period_start))
        .add(Column::Bucket.eq(bucket))
}

/// Loads the user's rows of the daily and monthly periods (both buckets).
pub(super) async fn load_rows(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: PeriodStarts,
) -> Result<Rows, DomainError> {
    load_rows_scoped(runner, &user_scope(tenant_id, user_id), tenant_id, user_id, periods).await
}

/// Same as `load_rows` under an explicit (PEP) scope.
pub(super) async fn load_rows_scoped(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: PeriodStarts,
) -> Result<Rows, DomainError> {
    let mut by_period = Condition::any();
    for (period_type, start) in periods_of(periods) {
        by_period = by_period
            .add(Condition::all().add(Column::PeriodType.eq(period_type)).add(Column::PeriodStart.eq(start)));
    }
    let rows = quota_usage::Entity::find()
        .filter(
            Condition::all()
                .add(Column::TenantId.eq(tenant_id))
                .add(Column::UserId.eq(user_id))
                .add(Column::Bucket.is_in([BUCKET_TOTAL, BUCKET_PREMIUM]))
                .add(by_period),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    Ok(Rows(rows))
}

/// Creates the bucket row when missing (concurrent creation is absorbed by `ON CONFLICT DO NOTHING`).
pub(super) async fn ensure_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period_type: &str,
    period_start: Date,
    bucket: &str,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = user_scope(tenant_id, user_id);
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
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
    let on_conflict = OnConflict::columns([
        Column::TenantId,
        Column::UserId,
        Column::PeriodType,
        Column::PeriodStart,
        Column::Bucket,
    ])
    .do_nothing()
    .to_owned();
    let res = quota_usage::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await;
    match res {
        Ok(_) | Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
        Err(e) if e.is_unique_violation() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Applies `exprs` to one bucket row; returns the number of updated rows.
pub(super) async fn update_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period_type: &str,
    period_start: Date,
    bucket: &str,
    exprs: Vec<(Column, SimpleExpr)>,
) -> Result<u64, DomainError> {
    let scope = user_scope(tenant_id, user_id);
    let mut upd = quota_usage::Entity::update_many().secure();
    for (col, expr) in exprs {
        upd = upd.col_expr(col, expr);
    }
    let res = upd
        .filter(key_condition(tenant_id, user_id, period_type, period_start, bucket))
        .scope_with(&scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected)
}

/// `col = col + v`.
pub(super) fn add_expr<V: Into<sea_orm::Value>>(col: Column, v: V) -> (Column, SimpleExpr) {
    (col, Expr::col(col).add(v))
}

/// `col = CASE WHEN col >= v THEN col - v ELSE 0 END` (floored at zero).
pub(super) fn sub_floor_expr(col: Column, v: i64) -> (Column, SimpleExpr) {
    let case = Expr::case(Expr::col(col).gte(v), Expr::col(col).sub(v)).finally(Expr::value(0_i64));
    (col, case.into())
}
