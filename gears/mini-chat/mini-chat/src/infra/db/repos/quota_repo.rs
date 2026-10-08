//! `quota_usage` queries (DESIGN section 3.7, commit semantics).
//!
//! Every function takes an explicit `tenant_id` / `user_id` and filters on both
//! in addition to the caller's [`AccessScope`]. User-facing reads pass the
//! scope returned by `AuthzPort::quota_scope`; internal paths (preflight,
//! reserve, settlement from finalization or the orphan watchdog, which may have
//! no request context) pass [`user_scope`], the same tenant + owner narrowing
//! built from the turn's stored identity.
//!
//! Writes are single statements that are atomic per row: an
//! `INSERT ... ON CONFLICT DO NOTHING` creates a missing bucket row, then an
//! `UPDATE ... SET col = col + ?` applies the delta. Callers in a transaction
//! write before they read (Ruling R5) so `SQLite` takes the write lock first.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict};
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait, QueryFilter, QuerySelect};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::enums::{PeriodType, QuotaBucket};
use crate::domain::error::DomainError;
use crate::infra::db::entities::quota_usage::{self, Column};

/// Tenant + owner scope for the quota rows of `user_id` (internal paths).
#[must_use]
pub fn user_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// One bucket row key `(tenant_id, user_id, period_type, period_start, bucket)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketKey {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub period_type: PeriodType,
    pub period_start: Date,
    pub bucket: QuotaBucket,
}

/// Counter deltas applied by one settlement to one bucket row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettleDelta {
    /// Released from `reserved_credits_micro` (never below 0).
    pub release_reserved: i64,
    pub spent: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

fn key_condition(key: &BucketKey) -> Condition {
    Condition::all()
        .add(Column::TenantId.eq(key.tenant_id))
        .add(Column::UserId.eq(key.user_id))
        .add(Column::PeriodType.eq(key.period_type.as_str()))
        .add(Column::PeriodStart.eq(key.period_start))
        .add(Column::Bucket.eq(key.bucket.as_str()))
}

/// The user's rows for the given period starts (both buckets, both periods).
/// `lock` adds `FOR UPDATE` (rendered on Postgres only).
///
/// # Errors
/// Database failure.
pub async fn find_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    daily: Date,
    monthly: Date,
    lock: bool,
) -> Result<Vec<quota_usage::Model>, DomainError> {
    let periods = Condition::any()
        .add(
            Condition::all()
                .add(Column::PeriodType.eq(PeriodType::Daily.as_str()))
                .add(Column::PeriodStart.eq(daily)),
        )
        .add(
            Condition::all()
                .add(Column::PeriodType.eq(PeriodType::Monthly.as_str()))
                .add(Column::PeriodStart.eq(monthly)),
        );
    let mut query = quota_usage::Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::UserId.eq(user_id))
        .filter(periods);
    if lock {
        query = query.lock_exclusive();
    }
    Ok(query.secure().scope_with(scope).all(runner).await?)
}

/// Creates the bucket row with zero counters unless it exists.
///
/// # Errors
/// Scope violation or database failure.
pub async fn ensure_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    key: &BucketKey,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(key.tenant_id),
        user_id: Set(key.user_id),
        period_type: Set(key.period_type.as_str().to_owned()),
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
        updated_at: Set(now),
    };
    // Untargeted DO NOTHING (any unique key); the primary key column keeps
    // the MySQL polyfill harmless.
    let on_conflict = OnConflict::new().do_nothing_on([Column::Id]).to_owned();
    match quota_usage::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await
    {
        // The conflict was swallowed: the row already exists.
        Ok(_) | Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn update_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    key: &BucketKey,
    exprs: Vec<(Column, Expr)>,
) -> Result<(), DomainError> {
    let mut update = quota_usage::Entity::update_many().secure();
    for (col, expr) in exprs {
        update = update.col_expr(col, expr);
    }
    let res = update
        .filter(key_condition(key))
        .scope_with(scope)
        .exec(runner)
        .await?;
    if res.rows_affected == 1 {
        Ok(())
    } else {
        Err(DomainError::Internal(format!(
            "quota_usage row {:?} {} {} not updated ({} rows)",
            key.period_type.as_str(),
            key.period_start,
            key.bucket.as_str(),
            res.rows_affected
        )))
    }
}

/// `reserved_credits_micro += amount` on an existing row.
///
/// # Errors
/// The row is missing, scope violation or database failure.
pub async fn add_reserved(
    runner: &impl DBRunner,
    scope: &AccessScope,
    key: &BucketKey,
    amount: i64,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    update_row(
        runner,
        scope,
        key,
        vec![
            (
                Column::ReservedCreditsMicro,
                Expr::col(Column::ReservedCreditsMicro).add(amount),
            ),
            (Column::UpdatedAt, Expr::value(now)),
        ],
    )
    .await
}

/// Applies one settlement to an existing row: releases the turn's reserve
/// (clamped at 0 so a drifted accumulator cannot violate the non-negative
/// CHECK and block finalization), adds the spend and telemetry counters and
/// `calls += 1`.
///
/// # Errors
/// The row is missing, scope violation or database failure.
pub async fn apply_settlement(
    runner: &impl DBRunner,
    scope: &AccessScope,
    key: &BucketKey,
    d: &SettleDelta,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let reserved = Column::ReservedCreditsMicro;
    let released: Expr = Expr::case(
        Expr::col(reserved).gte(d.release_reserved),
        Expr::col(reserved).sub(d.release_reserved),
    )
    .finally(0_i64)
    .into();
    let add = |col: Column, v: i64| (col, Expr::col(col).add(v));
    update_row(
        runner,
        scope,
        key,
        vec![
            (reserved, released),
            add(Column::SpentCreditsMicro, d.spent),
            add(Column::Calls, 1),
            add(Column::InputTokens, d.input_tokens),
            add(Column::OutputTokens, d.output_tokens),
            add(Column::WebSearchCalls, i64::from(d.web_search_calls)),
            add(
                Column::CodeInterpreterCalls,
                i64::from(d.code_interpreter_calls),
            ),
            (Column::UpdatedAt, Expr::value(now)),
        ],
    )
    .await
}
