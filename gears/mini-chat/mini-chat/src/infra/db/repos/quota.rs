//! `quota_usage` queries: the status endpoint reads, the preflight read (with
//! row locks on `PostgreSQL`) and the reserve / settlement upserts.

use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter, QuerySelect,
};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{Bucket, PeriodType};
use crate::infra::db::entities::quota_usage::{ActiveModel, Column, Entity, Model};

/// Increments applied to one bucket row (negative values decrement).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketDelta {
    pub reserved_credits_micro: i64,
    pub spent_credits_micro: i64,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// One `(period_type, period_start, bucket)` row key of a user.
#[derive(Debug, Clone, Copy)]
pub struct BucketKey {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub period: PeriodType,
    pub period_start: NaiveDate,
    pub bucket: Bucket,
}

/// Queries over `quota_usage`.
pub struct QuotaRepo;

fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

fn periods_condition(user_id: Uuid, daily: NaiveDate, monthly: NaiveDate) -> Condition {
    let period = |kind: PeriodType, start: NaiveDate| {
        Condition::all()
            .add(Column::PeriodType.eq(kind.as_str()))
            .add(Column::PeriodStart.eq(start))
    };
    Condition::all().add(Column::UserId.eq(user_id)).add(
        Condition::any()
            .add(period(PeriodType::Daily, daily))
            .add(period(PeriodType::Monthly, monthly)),
    )
}

fn to_i32(name: &str, v: i64) -> DomainResult<i32> {
    i32::try_from(v).map_err(|_| DomainError::internal(format!("{name} delta out of range: {v}")))
}

impl QuotaRepo {
    /// Every bucket row of `user_id` in the current daily period (`daily` start)
    /// and the current monthly period (`monthly` start).
    ///
    /// # Errors
    /// Database failures.
    pub async fn rows_for_periods(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        daily: NaiveDate,
        monthly: NaiveDate,
    ) -> DomainResult<Vec<Model>> {
        let scope = owner_scope(tenant_id, user_id);
        Self::rows_in_scope(runner, &scope, user_id, daily, monthly).await
    }

    /// Like [`Self::rows_for_periods`], with `SELECT ... FOR UPDATE` when
    /// `for_update` is set (`PostgreSQL` only; callers pass `false` on `SQLite`).
    ///
    /// # Errors
    /// Database failures.
    pub async fn rows_for_periods_locked(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        daily: NaiveDate,
        monthly: NaiveDate,
        for_update: bool,
    ) -> DomainResult<Vec<Model>> {
        let mut select = Entity::find().filter(periods_condition(user_id, daily, monthly));
        if for_update {
            select = select.lock_exclusive();
        }
        Ok(select
            .secure()
            .scope_with(&owner_scope(tenant_id, user_id))
            .all(runner)
            .await?)
    }

    /// Like [`Self::rows_for_periods`], narrowed by an already compiled `scope`
    /// (the PDP decision of the request).
    ///
    /// # Errors
    /// Database failures.
    pub async fn rows_in_scope(
        runner: &impl DBRunner,
        scope: &AccessScope,
        user_id: Uuid,
        daily: NaiveDate,
        monthly: NaiveDate,
    ) -> DomainResult<Vec<Model>> {
        Ok(Entity::find()
            .filter(periods_condition(user_id, daily, monthly))
            .secure()
            .scope_with(scope)
            .all(runner)
            .await?)
    }

    /// Apply `delta` to the row `key` in one `INSERT .. ON CONFLICT (tenant_id,
    /// user_id, period_type, period_start, bucket) DO UPDATE` statement: every
    /// counter is incremented by its delta; a missing row is created (new v4 id)
    /// with the deltas as values, a negative reserve delta stored as 0.
    ///
    /// # Errors
    /// Database failures; `Internal` when a call-counter delta exceeds `i32`.
    pub async fn apply_delta(
        runner: &impl DBRunner,
        key: BucketKey,
        delta: BucketDelta,
        now: DateTime<Utc>,
    ) -> DomainResult<()> {
        let calls = to_i32("calls", delta.calls)?;
        let web = to_i32("web_search_calls", delta.web_search_calls)?;
        let ci = to_i32("code_interpreter_calls", delta.code_interpreter_calls)?;
        let am = ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            tenant_id: ActiveValue::Set(key.tenant_id),
            user_id: ActiveValue::Set(key.user_id),
            period_type: ActiveValue::Set(key.period.as_str().to_owned()),
            period_start: ActiveValue::Set(key.period_start),
            bucket: ActiveValue::Set(key.bucket.as_str().to_owned()),
            spent_credits_micro: ActiveValue::Set(delta.spent_credits_micro),
            reserved_credits_micro: ActiveValue::Set(std::cmp::max(
                delta.reserved_credits_micro,
                0,
            )),
            calls: ActiveValue::Set(calls),
            input_tokens: ActiveValue::Set(delta.input_tokens),
            output_tokens: ActiveValue::Set(delta.output_tokens),
            file_search_calls: ActiveValue::Set(0),
            web_search_calls: ActiveValue::Set(web),
            code_interpreter_calls: ActiveValue::Set(ci),
            rag_retrieval_calls: ActiveValue::Set(0),
            image_inputs: ActiveValue::Set(0),
            image_upload_bytes: ActiveValue::Set(0),
            updated_at: ActiveValue::Set(Some(now)),
        };
        // Qualified with the table name: an unqualified column is ambiguous with
        // `excluded` on PostgreSQL.
        let inc = |col: Column, by: i64| Expr::col((Entity, col)).add(by);
        let on_conflict = SecureOnConflict::<Entity>::columns([
            Column::TenantId,
            Column::UserId,
            Column::PeriodType,
            Column::PeriodStart,
            Column::Bucket,
        ])
        .value(
            Column::ReservedCreditsMicro,
            inc(Column::ReservedCreditsMicro, delta.reserved_credits_micro),
        )?
        .value(
            Column::SpentCreditsMicro,
            inc(Column::SpentCreditsMicro, delta.spent_credits_micro),
        )?
        .value(Column::Calls, inc(Column::Calls, delta.calls))?
        .value(
            Column::InputTokens,
            inc(Column::InputTokens, delta.input_tokens),
        )?
        .value(
            Column::OutputTokens,
            inc(Column::OutputTokens, delta.output_tokens),
        )?
        .value(
            Column::WebSearchCalls,
            inc(Column::WebSearchCalls, delta.web_search_calls),
        )?
        .value(
            Column::CodeInterpreterCalls,
            inc(Column::CodeInterpreterCalls, delta.code_interpreter_calls),
        )?
        .value(Column::UpdatedAt, Expr::value(now))?;
        Entity::insert(am.clone())
            .secure()
            .scope_with_model(&owner_scope(key.tenant_id, key.user_id), &am)?
            .on_conflict(on_conflict)
            .exec_with_returning(runner)
            .await?;
        Ok(())
    }
}
