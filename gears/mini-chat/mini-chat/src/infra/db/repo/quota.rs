//! `QuotaRepository` over the Secure ORM.
//!
//! Reserve and settlement are single-statement upserts on the unique key
//! `(tenant_id, user_id, period_type, period_start, bucket)`: the row is
//! created on first use and incremented atomically otherwise, so concurrent
//! first turns of one user never race into a unique violation (the reserve
//! transaction classifies a `Conflict` as a lost `chat_turns` insert) and no
//! read-modify-write is needed.

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::{QuotaOwner, QuotaRepository, QuotaSettlement};
use crate::domain::quota::{Bucket, Period, PeriodStarts, QuotaRow};
use crate::infra::db::entity::quota_usage::{self, Column};
use crate::infra::db::timestamp::to_db;

/// The persisted `period_start` of a domain date.
fn db_date(date: Date) -> sea_orm::prelude::Date {
    sea_orm::prelude::Date::from_ymd_opt(
        date.year(),
        u32::from(u8::from(date.month())),
        u32::from(date.day()),
    )
    .unwrap_or_default()
}

fn to_row(model: &quota_usage::Model, starts: PeriodStarts) -> Option<QuotaRow> {
    let bucket = Bucket::parse(&model.bucket)?;
    let period = Period::parse(&model.period_type)?;
    // Rows of other periods or unknown buckets are not enforcement state.
    (model.period_start == db_date(starts.of(period))).then_some(QuotaRow {
        bucket,
        period,
        spent_credits_micro: model.spent_credits_micro,
        reserved_credits_micro: model.reserved_credits_micro,
        web_search_calls: i64::from(model.web_search_calls),
        code_interpreter_calls: i64::from(model.code_interpreter_calls),
    })
}

/// The additive effect of one write on a bucket row.
#[derive(Clone, Copy, Default)]
struct Delta {
    spent: i64,
    reserved: i64,
    calls: i32,
    input_tokens: i64,
    output_tokens: i64,
    web_search_calls: i32,
    code_interpreter_calls: i32,
}

fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn clamp_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// The unique key of a bucket row.
#[derive(Clone, Copy)]
struct RowKey {
    owner: QuotaOwner,
    bucket: Bucket,
    period: Period,
    start: Date,
}

/// Inserts the row `delta` describes, or adds `delta` to the existing one.
async fn upsert<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    key: RowKey,
    delta: Delta,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let model = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(key.owner.tenant_id),
        user_id: Set(key.owner.user_id),
        period_type: Set(key.period.as_str().to_owned()),
        period_start: Set(db_date(key.start)),
        bucket: Set(key.bucket.as_str().to_owned()),
        // A row created by a settlement has nothing reserved to release.
        spent_credits_micro: Set(delta.spent),
        reserved_credits_micro: Set(Ord::max(delta.reserved, 0)),
        calls: Set(delta.calls),
        input_tokens: Set(delta.input_tokens),
        output_tokens: Set(delta.output_tokens),
        file_search_calls: Set(0),
        web_search_calls: Set(delta.web_search_calls),
        code_interpreter_calls: Set(delta.code_interpreter_calls),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(Some(to_db(now))),
    };
    let table = quota_usage::Entity;
    let add = |column: Column, by: i64| Expr::col((table, column)).add(by);
    let on_conflict = SecureOnConflict::<quota_usage::Entity>::columns([
        Column::TenantId,
        Column::UserId,
        Column::PeriodType,
        Column::PeriodStart,
        Column::Bucket,
    ])
    .value(
        Column::SpentCreditsMicro,
        add(Column::SpentCreditsMicro, delta.spent),
    )?
    .value(
        Column::ReservedCreditsMicro,
        add(Column::ReservedCreditsMicro, delta.reserved),
    )?
    .value(Column::Calls, add(Column::Calls, i64::from(delta.calls)))?
    .value(
        Column::InputTokens,
        add(Column::InputTokens, delta.input_tokens),
    )?
    .value(
        Column::OutputTokens,
        add(Column::OutputTokens, delta.output_tokens),
    )?
    .value(
        Column::WebSearchCalls,
        add(Column::WebSearchCalls, i64::from(delta.web_search_calls)),
    )?
    .value(
        Column::CodeInterpreterCalls,
        add(
            Column::CodeInterpreterCalls,
            i64::from(delta.code_interpreter_calls),
        ),
    )?
    .value(Column::UpdatedAt, Expr::value(to_db(now)))?;
    quota_usage::Entity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)?
        .on_conflict(on_conflict)
        .exec(runner)
        .await?;
    Ok(())
}

#[async_trait]
impl<R: DBRunner> QuotaRepository for R {
    async fn quota_rows(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        starts: PeriodStarts,
    ) -> Result<Vec<QuotaRow>, DomainError> {
        let mut periods = Condition::any();
        for period in Period::ALL {
            periods = periods.add(
                Condition::all()
                    .add(Column::PeriodType.eq(period.as_str()))
                    .add(Column::PeriodStart.eq(db_date(starts.of(period)))),
            );
        }
        let models = quota_usage::Entity::find()
            .filter(Column::TenantId.eq(owner.tenant_id))
            .filter(Column::UserId.eq(owner.user_id))
            .filter(periods)
            .secure()
            .scope_with(scope)
            .all(self)
            .await?;
        Ok(models
            .iter()
            .filter_map(|model| to_row(model, starts))
            .collect())
    }

    async fn reserve_quota(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        buckets: &[Bucket],
        starts: PeriodStarts,
        reserve_credits_micro: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        // Fixed order (bucket, then period): concurrent reserves lock the rows
        // in the same order.
        for bucket in buckets {
            for period in Period::ALL {
                let key = RowKey {
                    owner,
                    bucket: *bucket,
                    period,
                    start: starts.of(period),
                };
                let delta = Delta {
                    reserved: reserve_credits_micro,
                    ..Delta::default()
                };
                upsert(self, scope, key, delta, now).await?;
            }
        }
        Ok(())
    }

    async fn settle_quota(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        buckets: &[Bucket],
        starts: PeriodStarts,
        settlement: &QuotaSettlement,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        for bucket in buckets {
            // Tokens and tool calls are telemetry of bucket `total` only.
            let total = *bucket == Bucket::Total;
            let delta = Delta {
                spent: settlement.spent_credits_micro,
                reserved: -settlement.release_credits_micro,
                calls: 1,
                input_tokens: if total {
                    clamp_i64(settlement.input_tokens)
                } else {
                    0
                },
                output_tokens: if total {
                    clamp_i64(settlement.output_tokens)
                } else {
                    0
                },
                web_search_calls: if total {
                    clamp_i32(settlement.web_search_calls)
                } else {
                    0
                },
                code_interpreter_calls: if total {
                    clamp_i32(settlement.code_interpreter_calls)
                } else {
                    0
                },
            };
            for period in Period::ALL {
                let key = RowKey {
                    owner,
                    bucket: *bucket,
                    period,
                    start: starts.of(period),
                };
                upsert(self, scope, key, delta, now).await?;
            }
        }
        Ok(())
    }
}
