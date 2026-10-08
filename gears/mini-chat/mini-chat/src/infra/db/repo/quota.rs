//! `quota_usage` repository (bucket rows per user, period and bucket).

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::quota_usage;

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";

/// Key of one bucket row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub period_type: &'static str,
    pub period_start: Date,
    pub bucket: &'static str,
}

/// Additive deltas applied to one bucket row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

#[must_use]
pub fn user_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// All bucket rows of a user for the given `(period_type, period_start)` keys.
pub async fn rows_for_periods(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &[(&'static str, Date)],
) -> Result<Vec<quota_usage::Model>, DomainError> {
    let mut any = Condition::any();
    for (pt, ps) in periods {
        any = any.add(
            Condition::all()
                .add(quota_usage::Column::PeriodType.eq(*pt))
                .add(quota_usage::Column::PeriodStart.eq(*ps)),
        );
    }
    Ok(quota_usage::Entity::find()
        .filter(
            Condition::all()
                .add(quota_usage::Column::TenantId.eq(tenant_id))
                .add(quota_usage::Column::UserId.eq(user_id))
                .add(any),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Apply additive deltas to a bucket row, creating it when missing.
pub async fn apply_delta(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    key: &BucketKey,
    d: Delta,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set(key.period_type.to_owned()),
        period_start: Set(key.period_start),
        bucket: Set(key.bucket.to_owned()),
        spent_credits_micro: Set(d.spent_credits_micro),
        reserved_credits_micro: Set(d.reserved_credits_micro),
        calls: Set(d.calls),
        input_tokens: Set(d.input_tokens),
        output_tokens: Set(d.output_tokens),
        file_search_calls: Set(0),
        web_search_calls: Set(d.web_search_calls),
        code_interpreter_calls: Set(d.code_interpreter_calls),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(now),
    };
    let add = |col: &str| Expr::cust(format!("quota_usage.{col} + excluded.{col}"));
    let on_conflict = SecureOnConflict::<quota_usage::Entity>::columns([
        quota_usage::Column::TenantId,
        quota_usage::Column::UserId,
        quota_usage::Column::PeriodType,
        quota_usage::Column::PeriodStart,
        quota_usage::Column::Bucket,
    ])
    .value(
        quota_usage::Column::SpentCreditsMicro,
        add("spent_credits_micro"),
    )?
    .value(
        quota_usage::Column::ReservedCreditsMicro,
        add("reserved_credits_micro"),
    )?
    .value(quota_usage::Column::Calls, add("calls"))?
    .value(quota_usage::Column::InputTokens, add("input_tokens"))?
    .value(quota_usage::Column::OutputTokens, add("output_tokens"))?
    .value(quota_usage::Column::WebSearchCalls, add("web_search_calls"))?
    .value(
        quota_usage::Column::CodeInterpreterCalls,
        add("code_interpreter_calls"),
    )?
    .value(
        quota_usage::Column::UpdatedAt,
        Expr::cust("excluded.updated_at"),
    )?;
    quota_usage::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .on_conflict(on_conflict)
        .exec(runner)
        .await?;
    Ok(())
}
