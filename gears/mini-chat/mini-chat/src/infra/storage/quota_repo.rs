//! `quota_usage` access: preflight reads, reserve increments with the limit re-check and
//! settlement updates (DESIGN section 3.7 `quota_usage`, 5.4).

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, Func, OnConflict};
use sea_orm::{ActiveValue, ColumnTrait, Condition, DbErr, EntityTrait, ExprTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use tracing::error;
use uuid::Uuid;

use super::entity::quota_usage;
use super::map_scope_err;
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::quota::{
    Bucket, BucketUsage, PeriodType, QuotaPeriod, QuotaRepository, QuotaSnapshot, ReservePlan,
};

/// Reads the bucket rows of a user.
pub struct QuotaRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
}

impl QuotaRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>) -> Self {
        Self { db }
    }
}

fn bucket_of(value: &str) -> Option<Bucket> {
    match value {
        "total" => Some(Bucket::Total),
        "tier:premium" => Some(Bucket::PremiumTier),
        _ => None,
    }
}

fn period_of(value: &str) -> Option<PeriodType> {
    match value {
        "daily" => Some(PeriodType::Daily),
        "monthly" => Some(PeriodType::Monthly),
        _ => None,
    }
}

fn period_condition(periods: &[QuotaPeriod; 2]) -> Condition {
    periods.iter().fold(Condition::any(), |any, period| {
        any.add(
            Condition::all()
                .add(quota_usage::Column::PeriodType.eq(period.kind.as_str()))
                .add(quota_usage::Column::PeriodStart.eq(period.start)),
        )
    })
}

/// The bucket rows of a user for `periods`, a missing row counting as zero.
pub async fn load_snapshot(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &[QuotaPeriod; 2],
) -> Result<QuotaSnapshot, DomainError> {
    let rows = quota_usage::Entity::find()
        .filter(quota_usage::Column::TenantId.eq(tenant_id))
        .filter(quota_usage::Column::UserId.eq(user_id))
        .filter(period_condition(periods))
        .secure()
        .scope_with(scope)
        .all(runner)
        .await
        .map_err(map_scope_err)?;
    Ok(QuotaSnapshot::from_rows(rows.into_iter().filter_map(
        |row| {
            let key = (period_of(&row.period_type)?, bucket_of(&row.bucket)?);
            Some((
                key,
                BucketUsage {
                    spent_credits_micro: row.spent_credits_micro,
                    reserved_credits_micro: row.reserved_credits_micro,
                    web_search_calls: i64::from(row.web_search_calls),
                    code_interpreter_calls: i64::from(row.code_interpreter_calls),
                },
            ))
        },
    )))
}

#[async_trait]
impl QuotaRepository for QuotaRepositoryImpl {
    async fn usage(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        periods: &[QuotaPeriod; 2],
    ) -> Result<QuotaSnapshot, DomainError> {
        let conn = self.db.conn()?;
        load_snapshot(&conn, scope, tenant_id, user_id, periods).await
    }
}

/// Creates the bucket row when it does not exist yet.
async fn ensure_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    period: &QuotaPeriod,
    bucket: Bucket,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let row = quota_usage::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant_id),
        user_id: ActiveValue::Set(user_id),
        period_type: ActiveValue::Set(period.kind.as_str().to_owned()),
        period_start: ActiveValue::Set(period.start),
        bucket: ActiveValue::Set(bucket.as_str().to_owned()),
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
    let on_conflict = OnConflict::columns([
        quota_usage::Column::TenantId,
        quota_usage::Column::UserId,
        quota_usage::Column::PeriodType,
        quota_usage::Column::PeriodStart,
        quota_usage::Column::Bucket,
    ])
    .do_nothing()
    .to_owned();
    let insert = quota_usage::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(map_scope_err)?
        .on_conflict_raw(on_conflict);
    match insert.exec(runner).await {
        Ok(_) | Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
        Err(err) => Err(map_scope_err(err)),
    }
}

fn row_filter(tenant_id: Uuid, user_id: Uuid, period: QuotaPeriod, bucket: Bucket) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant_id))
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(period.kind.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(period.start))
        .add(quota_usage::Column::Bucket.eq(bucket.as_str()))
}

/// Books the turn's reserve in every bucket row of the plan and re-reads the rows: a bucket over
/// its limit is `QuotaExceeded` (the caller's transaction rolls back).
pub async fn reserve(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    plan: &ReservePlan,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    for limit in &plan.limits {
        ensure_row(
            runner,
            scope,
            tenant_id,
            user_id,
            &limit.period,
            limit.bucket,
            now,
        )
        .await?;
        quota_usage::Entity::update_many()
            .col_expr(
                quota_usage::Column::ReservedCreditsMicro,
                Expr::col(quota_usage::Column::ReservedCreditsMicro)
                    .add(plan.reserved_credits_micro),
            )
            .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now))
            .filter(row_filter(tenant_id, user_id, limit.period, limit.bucket))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await
            .map_err(map_scope_err)?;
    }
    let snapshot = load_snapshot(runner, scope, tenant_id, user_id, &plan.periods).await?;
    for limit in &plan.limits {
        let row = snapshot.usage(limit.period.kind, limit.bucket);
        if row
            .spent_credits_micro
            .saturating_add(row.reserved_credits_micro)
            > limit.limit_credits_micro
        {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens,
            });
        }
    }
    Ok(())
}

/// The counters a settlement adds to a bucket row.
pub struct SettleDelta {
    /// The turn's reserve, released.
    pub reserved_credits_micro: i64,
    /// Credits charged.
    pub committed_credits_micro: i64,
    /// Telemetry: input tokens (bucket `total` only).
    pub input_tokens: i64,
    /// Telemetry: output tokens (bucket `total` only).
    pub output_tokens: i64,
    /// Web search calls (bucket `total` only).
    pub web_search_calls: i32,
    /// Code interpreter calls (bucket `total` only).
    pub code_interpreter_calls: i32,
    /// The release never takes `reserved_credits_micro` below 0 (the orphan release path): a row
    /// holding less than the turn's reserve is clamped to 0 and logged instead of failing the
    /// `CHECK (reserved_credits_micro >= 0)` and blocking the finalization for good.
    pub clamp_reserved_at_zero: bool,
}

/// One bucket row of one user.
#[derive(Debug, Clone, Copy)]
pub struct RowKey {
    /// The tenant.
    pub tenant_id: Uuid,
    /// The user.
    pub user_id: Uuid,
    /// The period instance.
    pub period: QuotaPeriod,
    /// The bucket.
    pub bucket: Bucket,
}

/// Settles the turn in one bucket row: reserve released, credits charged, `calls += 1`; the token
/// and tool counters only in bucket `total`.
pub async fn settle(
    runner: &impl DBRunner,
    scope: &AccessScope,
    key: RowKey,
    delta: &SettleDelta,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let RowKey {
        tenant_id,
        user_id,
        period,
        bucket,
    } = key;
    let released =
        Expr::col(quota_usage::Column::ReservedCreditsMicro).sub(delta.reserved_credits_micro);
    let reserved_after = if delta.clamp_reserved_at_zero {
        log_clamped_release(runner, scope, key, delta).await?;
        Func::greatest([Expr::value(0_i64), released]).into()
    } else {
        released
    };
    let mut update = quota_usage::Entity::update_many()
        .col_expr(quota_usage::Column::ReservedCreditsMicro, reserved_after)
        .col_expr(
            quota_usage::Column::SpentCreditsMicro,
            Expr::col(quota_usage::Column::SpentCreditsMicro).add(delta.committed_credits_micro),
        )
        .col_expr(
            quota_usage::Column::Calls,
            Expr::col(quota_usage::Column::Calls).add(1),
        )
        .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now));
    if bucket == Bucket::Total {
        update = update
            .col_expr(
                quota_usage::Column::InputTokens,
                Expr::col(quota_usage::Column::InputTokens).add(delta.input_tokens),
            )
            .col_expr(
                quota_usage::Column::OutputTokens,
                Expr::col(quota_usage::Column::OutputTokens).add(delta.output_tokens),
            )
            .col_expr(
                quota_usage::Column::WebSearchCalls,
                Expr::col(quota_usage::Column::WebSearchCalls).add(delta.web_search_calls),
            )
            .col_expr(
                quota_usage::Column::CodeInterpreterCalls,
                Expr::col(quota_usage::Column::CodeInterpreterCalls)
                    .add(delta.code_interpreter_calls),
            );
    }
    let affected = update
        .filter(row_filter(tenant_id, user_id, period, bucket))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    if affected == 0 {
        // The reserve created the row; a missing one means the turn has no reserve to settle.
        return Err(DomainError::internal("quota row to settle is missing"));
    }
    Ok(())
}

/// Logs (at error) a release that would take the row below zero: the clamp of the update then
/// keeps the finalization alive. Advisory read; the update clamps regardless of it.
async fn log_clamped_release(
    runner: &impl DBRunner,
    scope: &AccessScope,
    key: RowKey,
    delta: &SettleDelta,
) -> Result<(), DomainError> {
    let row = quota_usage::Entity::find()
        .filter(row_filter(
            key.tenant_id,
            key.user_id,
            key.period,
            key.bucket,
        ))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await
        .map_err(map_scope_err)?;
    if let Some(row) = row
        && row.reserved_credits_micro < delta.reserved_credits_micro
    {
        error!(
            tenant_id = %key.tenant_id,
            user_id = %key.user_id,
            period = key.period.kind.as_str(),
            bucket = key.bucket.as_str(),
            held_credits_micro = row.reserved_credits_micro,
            released_credits_micro = delta.reserved_credits_micro,
            "the reserve to release exceeds the reserve the quota row holds; clamping it to 0"
        );
    }
    Ok(())
}
