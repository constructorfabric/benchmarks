//! `quota_usage` repository (bucket rows per user, period and bucket).

use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, EntityTrait, ExprTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::quota_usage::{ActiveModel, Column, Entity, Model};

/// Bucket names.
pub mod bucket {
    pub const TOTAL: &str = "total";
    pub const PREMIUM: &str = "tier:premium";
}

/// Period type names.
pub mod period {
    pub const DAILY: &str = "daily";
    pub const MONTHLY: &str = "monthly";
}

fn scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// Key of a bucket row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketKey {
    pub period_type: &'static str,
    pub period_start: NaiveDate,
    pub bucket: &'static str,
}

/// All bucket rows of a user for the given period starts.
///
/// # Errors
/// Database errors.
pub async fn rows_for_periods(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    daily: NaiveDate,
    monthly: NaiveDate,
) -> Result<Vec<Model>, DomainError> {
    let cond = sea_orm::Condition::any()
        .add(
            sea_orm::Condition::all()
                .add(Column::PeriodType.eq(period::DAILY))
                .add(Column::PeriodStart.eq(daily)),
        )
        .add(
            sea_orm::Condition::all()
                .add(Column::PeriodType.eq(period::MONTHLY))
                .add(Column::PeriodStart.eq(monthly)),
        );
    Ok(Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::UserId.eq(user_id))
        .filter(cond)
        .secure()
        .scope_with(&scope(tenant_id, user_id))
        .all(runner)
        .await?)
}

/// Inserts a zero row if absent.
///
/// # Errors
/// Database errors.
pub async fn ensure_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    key: BucketKey,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    let am = ActiveModel {
        id: sea_orm::ActiveValue::Set(Uuid::new_v4()),
        tenant_id: sea_orm::ActiveValue::Set(tenant_id),
        user_id: sea_orm::ActiveValue::Set(user_id),
        period_type: sea_orm::ActiveValue::Set(key.period_type.to_owned()),
        period_start: sea_orm::ActiveValue::Set(key.period_start),
        bucket: sea_orm::ActiveValue::Set(key.bucket.to_owned()),
        spent_credits_micro: sea_orm::ActiveValue::Set(0),
        reserved_credits_micro: sea_orm::ActiveValue::Set(0),
        calls: sea_orm::ActiveValue::Set(0),
        input_tokens: sea_orm::ActiveValue::Set(0),
        output_tokens: sea_orm::ActiveValue::Set(0),
        file_search_calls: sea_orm::ActiveValue::Set(0),
        web_search_calls: sea_orm::ActiveValue::Set(0),
        code_interpreter_calls: sea_orm::ActiveValue::Set(0),
        rag_retrieval_calls: sea_orm::ActiveValue::Set(0),
        image_inputs: sea_orm::ActiveValue::Set(0),
        image_upload_bytes: sea_orm::ActiveValue::Set(0),
        updated_at: sea_orm::ActiveValue::Set(now),
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
    let res = Entity::insert(am)
        .secure()
        .scope_unchecked(&scope(tenant_id, user_id))?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await;
    match res {
        Ok(_) | Err(toolkit_db::secure::ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Increment set applied to a bucket row.
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

/// Applies an increment to one bucket row (atomic single UPDATE).
///
/// # Errors
/// Database errors.
pub async fn apply_delta(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    key: BucketKey,
    d: BucketDelta,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(
            Column::ReservedCreditsMicro,
            Expr::col(Column::ReservedCreditsMicro).add(d.reserved),
        )
        .col_expr(
            Column::SpentCreditsMicro,
            Expr::col(Column::SpentCreditsMicro).add(d.spent),
        )
        .col_expr(Column::Calls, Expr::col(Column::Calls).add(d.calls))
        .col_expr(
            Column::InputTokens,
            Expr::col(Column::InputTokens).add(d.input_tokens),
        )
        .col_expr(
            Column::OutputTokens,
            Expr::col(Column::OutputTokens).add(d.output_tokens),
        )
        .col_expr(
            Column::WebSearchCalls,
            Expr::col(Column::WebSearchCalls).add(d.web_search_calls),
        )
        .col_expr(
            Column::CodeInterpreterCalls,
            Expr::col(Column::CodeInterpreterCalls).add(d.code_interpreter_calls),
        )
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::UserId.eq(user_id))
        .filter(Column::PeriodType.eq(key.period_type))
        .filter(Column::PeriodStart.eq(key.period_start))
        .filter(Column::Bucket.eq(key.bucket))
        .secure()
        .scope_with(&scope(tenant_id, user_id))
        .exec(runner)
        .await?
        .rows_affected)
}
