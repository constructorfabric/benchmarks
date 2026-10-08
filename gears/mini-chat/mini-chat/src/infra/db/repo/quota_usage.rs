//! Statements on `quota_usage`: per user, period and bucket counters (DESIGN section 3.7).
//!
//! Rows are created on first use by an insert that ignores a concurrent insert of the same key
//! (`ON CONFLICT DO NOTHING` on the unique bucket key), then changed by one `UPDATE` with column
//! increments, so concurrent writers never lose an update.

use sea_orm::sea_query::{Expr, ExprTrait as _, OnConflict, SimpleExpr};
use sea_orm::{
    ColumnTrait, Condition, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};
use time::Date;
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeConstraint, ScopeError, ScopeFilter, SecureEntityExt,
    SecureInsertExt, SecureUpdateExt, pep_properties,
};
use uuid::Uuid;

use crate::domain::error::{DomainError, map_scope_err};
use crate::domain::quota::periods::{Bucket, Period, PeriodStarts};
use crate::infra::db::entity::quota_usage::{self, Column};
use crate::infra::db::ts::db_now;

/// Unique key of a bucket row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketKey {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub period: Period,
    pub period_start: Date,
    pub bucket: Bucket,
}

/// Increments applied to one bucket row. `reserved_credits_micro` may be negative (release);
/// the column never goes below 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketDelta {
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

/// Scope of one user's rows (tenant and owner), for the quota service's own reads and writes.
#[must_use]
pub fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant_id),
        ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
    ]))
}

/// Creates the row of `key` when it does not exist, then applies `delta` to it.
///
/// # Errors
/// `Internal` on a database error.
pub async fn add(
    conn: &impl DBRunner,
    key: &BucketKey,
    delta: &BucketDelta,
) -> Result<(), DomainError> {
    let scope = owner_scope(key.tenant_id, key.user_id);
    ensure_row(conn, &scope, key).await?;
    if delta.reserved_credits_micro < 0 {
        warn_if_release_exceeds_reserve(conn, &scope, key, delta.reserved_credits_micro).await?;
    }
    let mut update = quota_usage::Entity::update_many()
        .col_expr(Column::UpdatedAt, Expr::value(db_now()))
        .filter(key_condition(key));
    if delta.reserved_credits_micro != 0 {
        update = update.col_expr(
            Column::ReservedCreditsMicro,
            clamped_add(Column::ReservedCreditsMicro, delta.reserved_credits_micro),
        );
    }
    for (column, by) in [
        (Column::SpentCreditsMicro, delta.spent_credits_micro),
        (Column::Calls, i64::from(delta.calls)),
        (Column::InputTokens, delta.input_tokens),
        (Column::OutputTokens, delta.output_tokens),
        (Column::WebSearchCalls, i64::from(delta.web_search_calls)),
        (
            Column::CodeInterpreterCalls,
            i64::from(delta.code_interpreter_calls),
        ),
    ] {
        if by != 0 {
            update = update.col_expr(column, Expr::col(column).add(by));
        }
    }
    update
        .secure()
        .scope_with(&scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// Logs a release larger than the row's booked reserve (the update clamps it at 0).
async fn warn_if_release_exceeds_reserve(
    conn: &impl DBRunner,
    scope: &AccessScope,
    key: &BucketKey,
    by: i64,
) -> Result<(), DomainError> {
    let reserved = quota_usage::Entity::find()
        .filter(key_condition(key))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?
        .map_or(0, |r| r.reserved_credits_micro);
    if reserved.saturating_add(by) < 0 {
        tracing::warn!(
            period = key.period.as_str(),
            bucket = key.bucket.as_str(),
            period_start = %key.period_start,
            reserved,
            release = by.saturating_neg(),
            "quota reserve release exceeds the booked reserve; reserved_credits_micro clamped at 0"
        );
    }
    Ok(())
}

/// `column + by`, but never below 0 (a release larger than the booked reserve leaves 0).
fn clamped_add(column: Column, by: i64) -> SimpleExpr {
    if by >= 0 {
        return Expr::col(column).add(by);
    }
    Expr::case(Expr::col(column).add(by).gte(0), Expr::col(column).add(by))
        .finally(0)
        .into()
}

fn key_condition(key: &BucketKey) -> Condition {
    Condition::all()
        .add(Column::TenantId.eq(key.tenant_id))
        .add(Column::UserId.eq(key.user_id))
        .add(Column::PeriodType.eq(key.period.as_str()))
        .add(Column::PeriodStart.eq(key.period_start))
        .add(Column::Bucket.eq(key.bucket.as_str()))
}

/// Inserts a zeroed row for `key`; a row that already exists (also one inserted concurrently)
/// is left as is.
async fn ensure_row(
    conn: &impl DBRunner,
    scope: &AccessScope,
    key: &BucketKey,
) -> Result<(), DomainError> {
    let row = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(key.tenant_id),
        user_id: Set(key.user_id),
        period_type: Set(key.period.as_str().to_owned()),
        period_start: Set(key.period_start),
        bucket: Set(key.bucket.as_str().to_owned()),
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
        updated_at: Set(db_now()),
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
    let insert = quota_usage::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(map_scope_err)?
        .on_conflict_raw(on_conflict);
    match insert.exec(conn).await {
        Ok(_) | Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
        Err(err) => Err(map_scope_err(err)),
    }
}

/// The rows inside `scope` for the daily and monthly periods starting at `starts` (all buckets),
/// ordered by `(period_type, bucket)` — the canonical lock order of the quota service.
/// `lock` adds `FOR UPDATE` (`PostgreSQL`; a no-op on `SQLite`).
///
/// # Errors
/// `Internal` on a database error.
pub async fn load_current(
    conn: &impl DBRunner,
    scope: &AccessScope,
    starts: &PeriodStarts,
    lock: bool,
) -> Result<Vec<quota_usage::Model>, DomainError> {
    let current = Condition::any()
        .add(
            Condition::all()
                .add(Column::PeriodType.eq(Period::Daily.as_str()))
                .add(Column::PeriodStart.eq(starts.daily)),
        )
        .add(
            Condition::all()
                .add(Column::PeriodType.eq(Period::Monthly.as_str()))
                .add(Column::PeriodStart.eq(starts.monthly)),
        );
    let mut select = quota_usage::Entity::find()
        .filter(current)
        .order_by_asc(Column::PeriodType)
        .order_by_asc(Column::Bucket);
    if lock {
        select = select.lock_exclusive();
    }
    select
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}
