//! `quota_usage` repository (owner-scoped).

use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait, ExprTrait, QueryFilter,
};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::quota_usage;

/// Unique key of a bucket row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketKey {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// `daily` | `monthly`.
    pub period_type: &'static str,
    pub period_start: Date,
    /// `total` | `tier:premium`.
    pub bucket: &'static str,
}

/// Signed deltas applied to a bucket row in one atomic `UPDATE`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaIncrement {
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

/// Repository for `quota_usage` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotaUsageRepo;

impl QuotaUsageRepo {
    /// Insert a complete row.
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error (unique/CHECK
    /// violations included).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: quota_usage::Model,
    ) -> Result<quota_usage::Model, ScopeError> {
        insert_model::<quota_usage::Entity>(runner, scope, row).await
    }

    /// Load a row by id within the scope.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<quota_usage::Model>, ScopeError> {
        quota_usage::Entity::find_by_id(id)
            .secure()
            .scope_with(scope)
            .one(runner)
            .await
    }

    /// Rows in scope whose `(period_type, period_start)` is one of `periods`
    /// (every bucket).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_period_rows(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        periods: &[(&str, Date)],
    ) -> Result<Vec<quota_usage::Model>, ScopeError> {
        let any = periods
            .iter()
            .fold(Condition::any(), |cond, &(period_type, start)| {
                cond.add(
                    Condition::all()
                        .add(quota_usage::Column::PeriodType.eq(period_type))
                        .add(quota_usage::Column::PeriodStart.eq(start)),
                )
            });
        quota_usage::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(any)
            .all(runner)
            .await
    }

    /// Apply `inc` to the row `key` atomically (`SET x = x + :d`), inserting
    /// a zero row first when it does not exist.
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error.
    pub async fn increment(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        key: &BucketKey,
        inc: &QuotaIncrement,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        use quota_usage::Column as C;
        Self::insert_if_missing(runner, scope, key, now).await?;
        let add = |col: C, delta: i64| (col, Expr::col(col).add(delta));
        let deltas = [
            add(C::SpentCreditsMicro, inc.spent_credits_micro),
            add(C::ReservedCreditsMicro, inc.reserved_credits_micro),
            add(C::Calls, i64::from(inc.calls)),
            add(C::InputTokens, inc.input_tokens),
            add(C::OutputTokens, inc.output_tokens),
            add(C::WebSearchCalls, i64::from(inc.web_search_calls)),
            add(
                C::CodeInterpreterCalls,
                i64::from(inc.code_interpreter_calls),
            ),
        ];
        let mut update = quota_usage::Entity::update_many()
            .filter(key_condition(key))
            .secure()
            .scope_with(scope)
            .col_expr(C::UpdatedAt, Expr::value(now));
        for (col, expr) in deltas {
            update = update.col_expr(col, expr);
        }
        Ok(update.exec(runner).await?.rows_affected)
    }

    /// `INSERT .. ON CONFLICT (unique key) DO NOTHING` of a zero row.
    async fn insert_if_missing(
        runner: &impl DBRunner,
        scope: &AccessScope,
        key: &BucketKey,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError> {
        use quota_usage::Column as C;
        let am = quota_usage::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(key.tenant_id),
            user_id: Set(key.user_id),
            period_type: Set(key.period_type.to_owned()),
            period_start: Set(key.period_start),
            bucket: Set(key.bucket.to_owned()),
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
        // DO NOTHING updates no column, so tenant immutability cannot be
        // violated by the raw conflict clause.
        let on_conflict = OnConflict::columns([
            C::TenantId,
            C::UserId,
            C::PeriodType,
            C::PeriodStart,
            C::Bucket,
        ])
        .do_nothing()
        .to_owned();
        let res = quota_usage::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)?
            .on_conflict_raw(on_conflict)
            .exec(runner)
            .await;
        match res {
            Ok(_) | Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn key_condition(key: &BucketKey) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(key.tenant_id))
        .add(quota_usage::Column::UserId.eq(key.user_id))
        .add(quota_usage::Column::PeriodType.eq(key.period_type))
        .add(quota_usage::Column::PeriodStart.eq(key.period_start))
        .add(quota_usage::Column::Bucket.eq(key.bucket))
}
