//! The SQL implementation of the [`QuotaLedger`] port over `quota_usage`, `chat_turns` and
//! `messages`. Repository functions take the caller's transaction, so the reserve write and
//! the settlement join the transaction of the streaming pipeline.
//!
//! Every statement goes through the Secure ORM with an explicit scope: the `quota_usage` rows
//! through the tenant and owner predicates of the user, the turn and message rows through the
//! tenant. Every counter change is one atomic statement (an upsert or a conditional update), and
//! every addition is guarded so that it can neither wrap nor turn into a floating-point value on
//! `SQLite`. Timestamps are never ordered or compared through their stored text (it is variable
//! length on `SQLite`): the chronological decisions are taken on the decoded instants.

use async_trait::async_trait;
use sea_orm::sea_query::{Alias, Expr, ExprTrait, OnConflict};
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait, Order, QueryFilter, Set};
use time::{Date, Duration, OffsetDateTime, UtcOffset};
use toolkit_db::secure::{DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::quota::{
    Bucket, BucketKey, BucketRow, PeriodStarts, PeriodType, QuotaLedger, QuotaRows,
    ReserveIncrement, SettleDelta, TurnPreflightColumns, TurnRecord, TurnTerminalUpdate,
};
use crate::infra::storage::entity::{chat_turn, message, quota_usage};

/// The `chat_turns.state` of a turn that holds its reserve and has no terminal state yet.
const STATE_RUNNING: &str = "running";

/// The ledger over the gear schema (`SQLite` and `PostgreSQL`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlQuotaLedger;

/// The scope of the rows of one user: the tenant predicate and the owner predicate on `user_id`.
fn user_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// The current instant in UTC, truncated to microseconds (the precision every supported
/// database keeps).
fn now_micros() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}

/// A stored timestamp as an instant in UTC.
fn utc(instant: OffsetDateTime) -> OffsetDateTime {
    instant.to_offset(UtcOffset::UTC)
}

fn period_of(value: &str) -> Option<PeriodType> {
    PeriodType::ALL
        .into_iter()
        .find(|period| period.as_str() == value)
}

fn bucket_of(value: &str) -> Option<Bucket> {
    [Bucket::Total, Bucket::TierPremium]
        .into_iter()
        .find(|bucket| bucket.as_str() == value)
}

/// The condition that selects the row of `key`.
fn row_of(key: &BucketKey) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(key.tenant_id))
        .add(quota_usage::Column::UserId.eq(key.user_id))
        .add(quota_usage::Column::PeriodType.eq(key.period.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(key.period_start))
        .add(quota_usage::Column::Bucket.eq(key.bucket.as_str()))
}

/// The condition of one period of the current period starts.
fn period_condition(period: PeriodType, start: Date) -> Condition {
    Condition::all()
        .add(quota_usage::Column::PeriodType.eq(period.as_str()))
        .add(quota_usage::Column::PeriodStart.eq(start))
}

fn turn_record(row: chat_turn::Model) -> TurnRecord {
    TurnRecord {
        id: row.id,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        request_id: row.request_id,
        requester_user_id: row.requester_user_id,
        started_at: row.started_at,
        reserve_tokens: row.reserve_tokens,
        max_output_tokens_applied: row.max_output_tokens_applied.map(i64::from),
        reserved_credits_micro: row.reserved_credits_micro,
        policy_version_applied: row.policy_version_applied,
        effective_model: row.effective_model,
        minimal_generation_floor_applied: row.minimal_generation_floor_applied.map(i64::from),
    }
}

/// A 32-bit column value of the turn from its 64-bit domain value.
fn narrow(value: i64, column: &str) -> Result<i32, DomainError> {
    i32::try_from(value)
        .map_err(|_| DomainError::internal(format!("{column} {value} does not fit its column")))
}

#[async_trait]
impl QuotaLedger for SqlQuotaLedger {
    async fn read_rows(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        periods: &PeriodStarts,
    ) -> Result<QuotaRows, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-read-rows
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-read-rows
        // both buckets of both current periods in one statement, so the rows are one
        // consistent read; the scope supplies the tenant and the owner predicate on `user_id`
        let periods_condition = Condition::any()
            .add(period_condition(PeriodType::Daily, periods.daily))
            .add(period_condition(PeriodType::Monthly, periods.monthly));
        let buckets = [Bucket::Total, Bucket::TierPremium].map(Bucket::as_str);
        let found = quota_usage::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(periods_condition)
                    .add(quota_usage::Column::Bucket.is_in(buckets)),
            )
            .all(tx)
            .await?;
        let mut rows = QuotaRows::new();
        for row in found {
            let (Some(period), Some(bucket)) =
                (period_of(&row.period_type), bucket_of(&row.bucket))
            else {
                continue;
            };
            rows.insert(
                period,
                bucket,
                BucketRow {
                    spent_credits_micro: row.spent_credits_micro,
                    reserved_credits_micro: row.reserved_credits_micro,
                    web_search_calls: i64::from(row.web_search_calls),
                    code_interpreter_calls: i64::from(row.code_interpreter_calls),
                },
            );
        }
        Ok(rows)
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-read-rows
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-read-rows
    }

    async fn prior_context_tokens(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<i64, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-prior-context
        let scope = AccessScope::for_tenant(tenant_id);
        let live_assistant = || {
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::Role.eq("assistant"))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(message::Column::InputTokens.ne(0))
                        .add(message::Column::OutputTokens.ne(0)),
                )
        };
        // the stored text of an instant is variable length on `SQLite`, so its order is
        // chronological only across different seconds: the latest row by the stored order
        // names the latest second, and the rows of that second and the one before it are then
        // ordered by their decoded `(created_at, id)`
        let Some(top) = message::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(live_assistant())
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .one(tx)
            .await?
        else {
            return Ok(0);
        };
        let latest = utc(top.created_at);
        let from = latest
            .replace_nanosecond(0)
            .unwrap_or(latest)
            .checked_sub(Duration::seconds(1))
            .unwrap_or(latest);
        let candidates = message::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(live_assistant().add(message::Column::CreatedAt.gte(from)))
            .all(tx)
            .await?;
        let newest = candidates
            .into_iter()
            .max_by_key(|row| (utc(row.created_at), row.id))
            .unwrap_or(top);
        Ok(newest.input_tokens.saturating_add(newest.output_tokens))
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-prior-context
    }

    async fn add_reserved(
        &self,
        tx: &DbTx<'_>,
        key: &BucketKey,
        credits: i64,
    ) -> Result<ReserveIncrement, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-increment-total
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-increment-premium
        if credits < 0 {
            return Err(DomainError::internal(format!(
                "a negative reserve {credits} cannot be added to a bucket"
            )));
        }
        let scope = user_scope(key.tenant_id, key.user_id);
        let now = now_micros();
        let fresh = quota_usage::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(key.tenant_id),
            user_id: Set(key.user_id),
            period_type: Set(key.period.as_str().to_owned()),
            period_start: Set(key.period_start),
            bucket: Set(key.bucket.as_str().to_owned()),
            spent_credits_micro: Set(0),
            reserved_credits_micro: Set(credits),
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
        let reserved = Expr::col((
            quota_usage::Entity,
            quota_usage::Column::ReservedCreditsMicro,
        ));
        let proposed = Expr::col((
            Alias::new("excluded"),
            quota_usage::Column::ReservedCreditsMicro,
        ));
        // one atomic statement: the row is created when absent, otherwise the proposed credits
        // are added, but only while the counter cannot overflow (`reserved <= MAX - proposed`);
        // a refused addition returns no row, which is the overflow result
        let upsert = OnConflict::columns([
            quota_usage::Column::TenantId,
            quota_usage::Column::UserId,
            quota_usage::Column::PeriodType,
            quota_usage::Column::PeriodStart,
            quota_usage::Column::Bucket,
        ])
        .value(
            quota_usage::Column::ReservedCreditsMicro,
            reserved.clone().add(proposed.clone()),
        )
        .value(
            quota_usage::Column::UpdatedAt,
            Expr::col((Alias::new("excluded"), quota_usage::Column::UpdatedAt)),
        )
        .action_and_where(reserved.lte(Expr::val(i64::MAX).sub(proposed)))
        .to_owned();
        let inserted = quota_usage::Entity::insert(fresh.clone())
            .secure()
            .scope_with_model(&scope, &fresh)?
            .on_conflict_raw(upsert)
            .exec(tx)
            .await;
        match inserted {
            Ok(_) => Ok(ReserveIncrement::Applied),
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(ReserveIncrement::Overflow),
            Err(other) => Err(other.into()),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-increment-premium
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-increment-total
    }

    async fn fill_preflight_columns(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        turn_id: Uuid,
        columns: &TurnPreflightColumns,
    ) -> Result<bool, DomainError> {
        let max_output = narrow(
            columns.max_output_tokens_applied,
            "max_output_tokens_applied",
        )?;
        let floor = narrow(
            columns.minimal_generation_floor_applied,
            "minimal_generation_floor_applied",
        )?;
        // one conditional update: only the NULL columns of a `running` turn are filled, so the
        // columns are written once and never changed; `started_at` becomes the preflight
        // instant (the turn is `running` and not yet reserved, so this is allowed)
        let affected = chat_turn::Entity::update_many()
            .col_expr(
                chat_turn::Column::PolicyVersionApplied,
                Expr::value(columns.policy_version_applied),
            )
            .col_expr(
                chat_turn::Column::EffectiveModel,
                Expr::value(columns.effective_model.clone()),
            )
            .col_expr(
                chat_turn::Column::ReserveTokens,
                Expr::value(columns.reserve_tokens),
            )
            .col_expr(
                chat_turn::Column::MaxOutputTokensApplied,
                Expr::value(max_output),
            )
            .col_expr(
                chat_turn::Column::ReservedCreditsMicro,
                Expr::value(columns.reserved_credits_micro),
            )
            .col_expr(
                chat_turn::Column::MinimalGenerationFloorApplied,
                Expr::value(floor),
            )
            .col_expr(
                chat_turn::Column::StartedAt,
                Expr::value(utc(columns.started_at)),
            )
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now_micros()))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(turn_id))
                    .add(chat_turn::Column::State.eq(STATE_RUNNING))
                    .add(chat_turn::Column::ReserveTokens.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(tx)
            .await?
            .rows_affected;
        Ok(affected == 1)
    }

    async fn cas_terminal(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        turn_id: Uuid,
        update: &TurnTerminalUpdate,
        stale_cutoff: Option<OffsetDateTime>,
    ) -> Result<bool, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-cas
        let scope = AccessScope::for_tenant(tenant_id);
        // the guard of every finalizer: the turn is still `running`
        let mut guard = Condition::all()
            .add(chat_turn::Column::Id.eq(turn_id))
            .add(chat_turn::Column::State.eq(STATE_RUNNING));
        if let Some(cutoff) = stale_cutoff {
            // the orphan watchdog adds its predicates: not soft-deleted and no progress since
            // the cutoff (`COALESCE(last_progress_at, started_at) <= cutoff`). The comparison
            // is made on the decoded instants (the stored text does not sort chronologically on
            // `SQLite`) and the update is then guarded by the values that were compared, so a
            // concurrent progress write or soft delete makes the update lose
            let Some(observed) = chat_turn::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(guard.clone())
                .one(tx)
                .await?
            else {
                return Ok(false);
            };
            let alive_since = observed.last_progress_at.unwrap_or(observed.started_at);
            if observed.deleted_at.is_some() || utc(alive_since) > utc(cutoff) {
                return Ok(false);
            }
            guard = guard
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(chat_turn::Column::StartedAt.eq(observed.started_at));
            guard = match observed.last_progress_at {
                Some(progress) => guard.add(chat_turn::Column::LastProgressAt.eq(progress)),
                None => guard.add(chat_turn::Column::LastProgressAt.is_null()),
            };
        }
        // one conditional UPDATE, no in-memory lock: `rows_affected` tells the winner
        let completed_at = utc(update.completed_at);
        let affected = chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::State, Expr::value(update.state.as_str()))
            .col_expr(
                chat_turn::Column::ErrorCode,
                Expr::value(update.error_code.clone()),
            )
            .col_expr(
                chat_turn::Column::ErrorDetail,
                Expr::value(update.error_detail.clone()),
            )
            .col_expr(
                chat_turn::Column::AssistantMessageId,
                Expr::value(update.assistant_message_id),
            )
            .col_expr(
                chat_turn::Column::CompletedAt,
                Expr::value(Some(completed_at)),
            )
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now_micros()))
            .filter(guard)
            .secure()
            .scope_with(&scope)
            .exec(tx)
            .await?
            .rows_affected;
        Ok(affected == 1)
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-cas
    }

    async fn load_turn(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        // the billing columns of the turn inside its tenant; a reserve exists only when the
        // reserve columns and the requester are not NULL, which the caller decides
        let found = chat_turn::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
            .one(tx)
            .await?;
        Ok(found.map(turn_record))
    }

    async fn settle_bucket(
        &self,
        tx: &DbTx<'_>,
        key: &BucketKey,
        delta: &SettleDelta,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-update-total
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-update-premium
        let column = |column: quota_usage::Column| Expr::col((quota_usage::Entity, column));
        let spent = column(quota_usage::Column::SpentCreditsMicro);
        // every counter changes in this one statement; the spent addition is guarded so that it
        // cannot overflow (a refused update is reported below instead of corrupting the row)
        let affected =
            quota_usage::Entity::update_many()
                .col_expr(
                    quota_usage::Column::ReservedCreditsMicro,
                    column(quota_usage::Column::ReservedCreditsMicro)
                        .sub(Expr::value(delta.release_reserved_credits_micro)),
                )
                .col_expr(
                    quota_usage::Column::SpentCreditsMicro,
                    spent.clone().add(Expr::value(delta.spent_credits_micro)),
                )
                .col_expr(
                    quota_usage::Column::Calls,
                    column(quota_usage::Column::Calls).add(Expr::value(1_i32)),
                )
                .col_expr(
                    quota_usage::Column::InputTokens,
                    column(quota_usage::Column::InputTokens).add(Expr::value(delta.input_tokens)),
                )
                .col_expr(
                    quota_usage::Column::OutputTokens,
                    column(quota_usage::Column::OutputTokens).add(Expr::value(delta.output_tokens)),
                )
                .col_expr(
                    quota_usage::Column::WebSearchCalls,
                    column(quota_usage::Column::WebSearchCalls).add(Expr::value(narrow(
                        delta.web_search_calls,
                        "web_search_calls",
                    )?)),
                )
                .col_expr(
                    quota_usage::Column::CodeInterpreterCalls,
                    column(quota_usage::Column::CodeInterpreterCalls).add(Expr::value(narrow(
                        delta.code_interpreter_calls,
                        "code_interpreter_calls",
                    )?)),
                )
                .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now_micros()))
                .filter(row_of(key).add(
                    spent.lte(Expr::val(i64::MAX).sub(Expr::value(delta.spent_credits_micro))),
                ))
                .secure()
                .scope_with(&user_scope(key.tenant_id, key.user_id))
                .exec(tx)
                .await?
                .rows_affected;
        if affected == 1 {
            Ok(())
        } else {
            Err(DomainError::internal(format!(
                "the {} {} row of {} does not exist or its counter cannot take the settlement",
                key.period.as_str(),
                key.bucket.as_str(),
                key.period_start
            )))
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-update-premium
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-update-total
    }
}

#[cfg(test)]
#[path = "quota_ledger_tests.rs"]
mod quota_ledger_tests;
