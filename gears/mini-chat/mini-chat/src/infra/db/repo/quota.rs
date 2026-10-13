//! `quota_usage` repository (DESIGN section 3.7 `quota_usage`, "Commit
//! semantics", section 5.4.2 "Tier Availability Evaluation Atomicity",
//! "TOCTOU").
//!
//! Every query is confined to one `(tenant_id, user_id)`: the scope (tenant +
//! owner) is applied and the two ids are added as explicit predicates. Rows
//! are keyed by `uq_quota_usage_bucket` `(tenant_id, user_id, period_type,
//! period_start, bucket)`; `period_start` is a `DATE` bound as [`time::Date`].
//!
//! Writes are single upserts per row: a missing row is inserted with the
//! turn's values, an existing row is updated with atomic `col = col + x`
//! expressions, so parallel streams never lose an update. The reserve release
//! is clamped at 0 (`CASE WHEN reserved >= x THEN reserved - x ELSE 0 END`)
//! so a drifted counter cannot violate the non-negative CHECK and fail a
//! finalization.

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait, SimpleExpr};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::DbRepository;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    BucketSettlement, QuotaBucketKey, QuotaOwner, QuotaRepository, QuotaUsageRecord,
};
use crate::domain::quota::{QuotaBucket, QuotaPeriod};
use crate::infra::db::entities::{TimestampForm, quota_usage};

fn keys_condition(owner: QuotaOwner, keys: &[QuotaBucketKey]) -> Condition {
    let any_key = keys.iter().fold(Condition::any(), |any, key| {
        any.add(
            Condition::all()
                .add(quota_usage::Column::PeriodType.eq(key.period.as_str()))
                .add(quota_usage::Column::PeriodStart.eq(key.period_start))
                .add(quota_usage::Column::Bucket.eq(key.bucket.as_str())),
        )
    });
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(owner.tenant_id))
        .add(quota_usage::Column::UserId.eq(owner.user_id))
        .add(any_key)
}

fn to_record(model: &quota_usage::Model) -> Result<QuotaUsageRecord, DomainError> {
    let period = QuotaPeriod::parse(&model.period_type).ok_or_else(|| {
        DomainError::internal(format!(
            "quota_usage {} has an unknown period_type `{}`",
            model.id, model.period_type
        ))
    })?;
    let bucket = QuotaBucket::parse(&model.bucket).ok_or_else(|| {
        DomainError::internal(format!(
            "quota_usage {} has an unexpected bucket `{}`",
            model.id, model.bucket
        ))
    })?;
    Ok(QuotaUsageRecord {
        key: QuotaBucketKey {
            period,
            period_start: model.period_start,
            bucket,
        },
        spent_credits_micro: model.spent_credits_micro,
        reserved_credits_micro: model.reserved_credits_micro,
        calls: model.calls,
        input_tokens: model.input_tokens,
        output_tokens: model.output_tokens,
        web_search_calls: model.web_search_calls,
        code_interpreter_calls: model.code_interpreter_calls,
    })
}

/// The existing rows of `keys` of `owner` in one statement;
/// with `for_update` the rows are locked (`FOR UPDATE` on PostgreSQL, a no-op
/// on `SQLite`).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored value outside the
/// value sets.
pub async fn read_buckets(
    runner: &impl DBRunner,
    scope: &AccessScope,
    owner: QuotaOwner,
    keys: &[QuotaBucketKey],
    for_update: bool,
) -> Result<Vec<QuotaUsageRecord>, DomainError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows = quota_usage::Entity::find()
        .filter(keys_condition(owner, keys))
        .secure()
        .scope_with(scope)
        .project_all(runner, |select| {
            let select = if for_update {
                select.lock_exclusive()
            } else {
                select
            };
            select.into_model::<quota_usage::Model>()
        })
        .await?;
    rows.iter().map(to_record).collect()
}

fn col(column: quota_usage::Column) -> Expr {
    Expr::col((quota_usage::Entity, column))
}

fn plus(column: quota_usage::Column, value: i64) -> SimpleExpr {
    col(column).add(value)
}

/// `reserved - x` clamped at 0.
fn release(value: i64) -> SimpleExpr {
    let reserved = quota_usage::Column::ReservedCreditsMicro;
    Expr::case(col(reserved).gte(value), col(reserved).sub(value))
        .finally(0_i64)
        .into()
}

fn saturating_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// The row to insert when `key` has no row yet.
#[derive(Default)]
struct RowValues {
    spent: i64,
    reserved: i64,
    calls: i32,
    input_tokens: i64,
    output_tokens: i64,
    web_search_calls: i32,
    code_interpreter_calls: i32,
}

fn new_row(
    ts: TimestampForm,
    owner: QuotaOwner,
    key: QuotaBucketKey,
    values: &RowValues,
    now: OffsetDateTime,
) -> quota_usage::ActiveModel {
    quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(owner.tenant_id),
        user_id: Set(owner.user_id),
        period_type: Set(key.period.as_str().to_owned()),
        period_start: Set(key.period_start),
        bucket: Set(key.bucket.as_str().to_owned()),
        spent_credits_micro: Set(values.spent),
        reserved_credits_micro: Set(values.reserved),
        calls: Set(values.calls),
        input_tokens: Set(values.input_tokens),
        output_tokens: Set(values.output_tokens),
        file_search_calls: Set(0),
        web_search_calls: Set(values.web_search_calls),
        code_interpreter_calls: Set(values.code_interpreter_calls),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(ts.stamp(now)),
    }
}

fn bucket_conflict() -> SecureOnConflict<quota_usage::Entity> {
    SecureOnConflict::<quota_usage::Entity>::columns([
        quota_usage::Column::TenantId,
        quota_usage::Column::UserId,
        quota_usage::Column::PeriodType,
        quota_usage::Column::PeriodStart,
        quota_usage::Column::Bucket,
    ])
}

async fn upsert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    model: quota_usage::ActiveModel,
    on_conflict: SecureOnConflict<quota_usage::Entity>,
) -> Result<(), DomainError> {
    quota_usage::Entity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)?
        .on_conflict(on_conflict)
        .exec(runner)
        .await?;
    Ok(())
}

/// Adds `credits_micro` to `reserved_credits_micro` of every row of `keys`
/// (one upsert per row; `updated_at` bound as `ts`).
///
/// # Errors
///
/// [`DomainError::Internal`] when a row is outside `scope` or a write fails.
pub async fn add_reserve(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    owner: QuotaOwner,
    keys: &[QuotaBucketKey],
    credits_micro: i64,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let values = RowValues {
        reserved: credits_micro,
        ..RowValues::default()
    };
    for key in keys {
        let model = new_row(ts, owner, *key, &values, now);
        let on_conflict = bucket_conflict()
            .value(
                quota_usage::Column::ReservedCreditsMicro,
                plus(quota_usage::Column::ReservedCreditsMicro, credits_micro),
            )?
            .value(quota_usage::Column::UpdatedAt, Expr::value(ts.stamp(now)))?;
        upsert(runner, scope, model, on_conflict).await?;
    }
    Ok(())
}

/// Applies every settlement to its row (one upsert per row; `calls += 1`;
/// `updated_at` bound as `ts`).
///
/// # Errors
///
/// [`DomainError::Internal`] when a row is outside `scope` or a write fails.
pub async fn settle(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    owner: QuotaOwner,
    settlements: &[BucketSettlement],
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    use quota_usage::Column as C;
    for settlement in settlements {
        let web_search_calls = saturating_i32(settlement.web_search_calls);
        let code_interpreter_calls = saturating_i32(settlement.code_interpreter_calls);
        let values = RowValues {
            spent: settlement.spent_credits_micro,
            reserved: 0,
            calls: 1,
            input_tokens: settlement.input_tokens,
            output_tokens: settlement.output_tokens,
            web_search_calls,
            code_interpreter_calls,
        };
        let model = new_row(ts, owner, settlement.key, &values, now);
        let on_conflict = bucket_conflict()
            .value(
                C::ReservedCreditsMicro,
                release(settlement.release_reserved_credits_micro),
            )?
            .value(
                C::SpentCreditsMicro,
                plus(C::SpentCreditsMicro, settlement.spent_credits_micro),
            )?
            .value(C::Calls, plus(C::Calls, 1))?
            .value(
                C::InputTokens,
                plus(C::InputTokens, settlement.input_tokens),
            )?
            .value(
                C::OutputTokens,
                plus(C::OutputTokens, settlement.output_tokens),
            )?
            .value(
                C::WebSearchCalls,
                plus(C::WebSearchCalls, i64::from(web_search_calls)),
            )?
            .value(
                C::CodeInterpreterCalls,
                plus(C::CodeInterpreterCalls, i64::from(code_interpreter_calls)),
            )?
            .value(C::UpdatedAt, Expr::value(ts.stamp(now)))?;
        upsert(runner, scope, model, on_conflict).await?;
    }
    Ok(())
}

#[async_trait]
impl QuotaRepository for DbRepository {
    async fn read_buckets(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        for_update: bool,
    ) -> Result<Vec<QuotaUsageRecord>, DomainError> {
        let db = self.db().db();
        let scope = scope.clone();
        let keys = keys.to_vec();
        db.transaction_ref_mapped(move |tx| {
            Box::pin(async move { read_buckets(tx, &scope, owner, &keys, for_update).await })
        })
        .await
    }

    async fn read_buckets_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
    ) -> Result<Vec<QuotaUsageRecord>, DomainError> {
        read_buckets(tx, scope, owner, keys, false).await
    }

    async fn add_reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        credits_micro: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        add_reserve(
            tx,
            self.timestamps(),
            scope,
            owner,
            keys,
            credits_micro,
            now,
        )
        .await
    }

    async fn settle_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        settlements: &[BucketSettlement],
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        settle(tx, self.timestamps(), scope, owner, settlements, now).await
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
