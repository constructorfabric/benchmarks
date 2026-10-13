//! Quota persistence: the Secure ORM implementation of the
//! [`QuotaRepository`] port (`cpt-cf-mini-chat-feature-quota-billing`).
//!
//! Two operations take a caller transaction through a closure: the reserve
//! write ([`SeaQuotaRepository::reserve_with`]) and the CAS-guarded
//! finalization ([`SeaQuotaRepository::finalize_turn_with`]). The closure runs
//! the caller's extra steps (the user message and the `chat_turns` INSERT of a
//! send; the assistant message, the audit event and the thread-summary task of
//! a finalization) inside the same transaction, after the quota statements, so
//! that a failure of any step rolls the quota change back with them. A lock or
//! serialization conflict anywhere in the transaction re-runs the whole
//! closure, at most `MAX_TX_ATTEMPTS` attempts in total, so the closure must
//! not keep state across attempts.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::{Date, OffsetDateTime};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{
    DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureOnConflict, SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, QuotaSubject};
use crate::domain::service::quota::events::{TurnUsageEventArgs, build_turn_usage_event};
use crate::domain::service::quota::finalize::{
    FinalizeRequest, FinalizeResult, FinalizeVariant, FinalizeWon, derive_billing_outcome,
};
use crate::domain::service::quota::periods::{PeriodKeys, period_keys};
use crate::domain::service::quota::ports::{PreflightRead, PreflightRows, QuotaRepository};
use crate::domain::service::quota::reserve::{
    CONFLICT_BACKOFF, MAX_TX_ATTEMPTS, ReserveMode, ReserveRequest, ReservedColumns,
    reserve_buckets, reserve_exceeds_limit,
};
use crate::domain::service::quota::settle::{
    BucketDelta, FallbackCause, SettleInput, ToolCallCounts, bucket_deltas, plan_settlement,
};
use crate::domain::service::quota::types::{BucketRow, QuotaBucket, QuotaPeriod};
use crate::infra::db::entities::{chat_turn, message, quota_usage};
use crate::infra::db::mapping::scope_error;
use crate::infra::outbox::queues::OutboxEnqueuer;
use crate::infra::outbox::usage::enqueue_usage_event;
use mini_chat_sdk::{BillingOutcome, SettlementMethod, TerminalState};
use toolkit_db::contention::is_retryable_contention;

/// The future of the caller's extra steps.
pub type TxFuture<'a> = Pin<Box<dyn Future<Output = Result<(), DomainError>> + Send + 'a>>;

/// The future of one transaction attempt.
type AttemptFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>;

/// Secure ORM quota store.
pub struct SeaQuotaRepository {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
    overshoot_tolerance_factor: f64,
}

impl SeaQuotaRepository {
    /// Creates the store over the database provider, the shared outbox
    /// enqueuer (the usage queue) and `quota.overshoot_tolerance_factor`.
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        outbox: Arc<OutboxEnqueuer>,
        overshoot_tolerance_factor: f64,
    ) -> Self {
        Self {
            db,
            outbox,
            overshoot_tolerance_factor,
        }
    }

    /// Runs `attempt` in a transaction; a lock or serialization conflict
    /// re-runs the whole transaction, at most `MAX_TX_ATTEMPTS` attempts in
    /// total with a short backoff.
    async fn with_retry<T, F>(&self, attempt: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> Fn(&'a DbTx<'a>) -> AttemptFuture<'a, T> + Send + Sync + 'static,
    {
        let backend = self.db.db().backend();
        let attempt = Arc::new(attempt);
        let mut number = 0_u32;
        loop {
            number += 1;
            let run = Arc::clone(&attempt);
            let result = self
                .db
                .transaction(move |tx| Box::pin(async move { run(tx).await }))
                .await;
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-reserve-conflict
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-finalize-conflict
            match result {
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-retry-reserve
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-retry-finalize
                Err(DomainError::Database(message))
                    if number < MAX_TX_ATTEMPTS && is_conflict(backend, &message) =>
                {
                    tracing::warn!(attempt = number, "quota transaction conflict, retrying");
                    let pause = CONFLICT_BACKOFF
                        .get(usize::try_from(number - 1).unwrap_or(0))
                        .copied()
                        .unwrap_or_default();
                    tokio::time::sleep(pause).await;
                }
                // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-retry-finalize
                // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-retry-reserve
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-return-reserve-conflict
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-return-finalize-conflict
                other => return other,
                // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-return-finalize-conflict
                // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-return-reserve-conflict
            }
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-finalize-conflict
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-reserve-conflict
        }
    }

    /// Reserve write and limit re-check in one write-first transaction
    /// (`cpt-cf-mini-chat-algo-quota-billing-reserve-write`): the daily and the
    /// monthly `total` rows (and `tier:premium` for a premium turn) are
    /// upserted in the canonical order, the rows are re-read and checked
    /// against the limits, a fill updates the six columns of the new turn, and
    /// then `extra` runs with the six values. The transaction commits when
    /// `extra` returns `Ok`.
    ///
    /// A conflict (`SQLITE_BUSY`, PostgreSQL `40001` / `40P01`) re-runs the
    /// whole transaction, at most 3 attempts in total with a short backoff; the
    /// third conflict is [`DomainError::Database`] (500), never a 429.
    ///
    /// # Errors
    ///
    /// `QuotaExceeded(Tokens)` when a bucket is over its limit at the re-check
    /// (`extra` is not called, nothing is committed), [`DomainError::Internal`]
    /// when a fill updates no row, the error of `extra`, or
    /// [`DomainError::Database`].
    // @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-reserve-write:p1
    pub async fn reserve_with<F>(
        &self,
        request: &ReserveRequest<'_>,
        extra: F,
    ) -> Result<ReservedColumns, DomainError>
    where
        F: for<'a> Fn(&'a DbTx<'a>, &'a ReservedColumns) -> TxFuture<'a> + Send + Sync + 'static,
    {
        let plan = Arc::new(ReservePlan::from_request(request));
        let extra = Arc::new(extra);
        let columns = plan.columns.clone();
        // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-in-progress
        self.with_retry(move |tx| {
            let plan = Arc::clone(&plan);
            let extra = Arc::clone(&extra);
            Box::pin(async move {
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-reserve-write-first
                reserve_in_tx(tx, &plan).await?;
                // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-reserve-write-first
                extra(tx, &plan.columns).await
            })
        })
        .await?;
        // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-in-progress
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-return-reserve
        Ok(columns)
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-return-reserve
    }

    /// CAS-guarded finalization in one write-first transaction
    /// (`cpt-cf-mini-chat-algo-quota-billing-finalize-turn`): the CAS UPDATE of
    /// `chat_turns`, the settlement of the `quota_usage` rows, the usage event
    /// enqueue, and then `extra` with the [`FinalizeWon`]. A lost CAS returns
    /// [`FinalizeResult::Lost`] without calling `extra` and without writing.
    /// The transaction commits when `extra` returns `Ok`; the outbox is woken
    /// after the commit.
    ///
    /// The same bounded conflict retry as [`Self::reserve_with`] applies; a
    /// re-run whose CAS finds the turn already terminal is `Lost`. When the
    /// retry budget runs out the turn stays `running` and the error is
    /// [`DomainError::Database`].
    ///
    /// # Errors
    ///
    /// The error of `extra` or [`DomainError::Database`] /
    /// [`DomainError::Internal`]; the transaction is rolled back.
    // @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-settlement:p1
    pub async fn finalize_turn_with<F>(
        &self,
        request: &FinalizeRequest,
        extra: F,
    ) -> Result<FinalizeResult, DomainError>
    where
        F: for<'a> Fn(&'a DbTx<'a>, &'a FinalizeWon) -> TxFuture<'a> + Send + Sync + 'static,
    {
        let request = Arc::new(request.clone());
        let extra = Arc::new(extra);
        let outbox = Arc::clone(&self.outbox);
        let tolerance = self.overshoot_tolerance_factor;
        let (result, wake, cost) = self
            .with_retry(move |tx| {
                let request = Arc::clone(&request);
                let extra = Arc::clone(&extra);
                let outbox = Arc::clone(&outbox);
                Box::pin(async move {
                    let (result, wake, cost) =
                        finalize_in_tx(tx, &outbox, tolerance, &request).await?;
                    if let FinalizeResult::Won(won) = &result {
                        extra(tx, won).await?;
                    }
                    Ok((result, wake, cost))
                })
            })
            .await?;
        wake.fire();
        if let Some(cost) = cost {
            cost.log();
        }
        Ok(result)
    }
}

/// Whether a database error message is a lock or serialization conflict.
fn is_conflict(backend: sea_orm::DbBackend, message: &str) -> bool {
    is_retryable_contention(backend, &sea_orm::DbErr::Custom(message.to_owned()))
}

#[async_trait]
impl QuotaRepository for SeaQuotaRepository {
    async fn read_preflight(&self, read: &PreflightRead) -> Result<PreflightRows, DomainError> {
        let read = *read;
        let lock = self.db.db().backend() == sea_orm::DbBackend::Postgres;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-commit-read
        self.db
            .transaction(move |tx| {
                Box::pin(async move { read_preflight_in_tx(tx, &read, lock).await })
            })
            .await
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-commit-read
    }

    async fn read_buckets(
        &self,
        scope: &AccessScope,
        keys: PeriodKeys,
    ) -> Result<Vec<BucketRow>, DomainError> {
        let conn = self.db.conn()?;
        read_rows(&conn, scope, &keys, &QuotaBucket::ALL, false).await
    }

    async fn reserve(&self, request: &ReserveRequest<'_>) -> Result<ReservedColumns, DomainError> {
        self.reserve_with(request, |_tx, _reserved| Box::pin(async { Ok(()) }))
            .await
    }

    async fn finalize_turn(
        &self,
        request: &FinalizeRequest,
    ) -> Result<FinalizeResult, DomainError> {
        self.finalize_turn_with(request, |_tx, _won| Box::pin(async { Ok(()) }))
            .await
    }
}

// ---- rows ---------------------------------------------------------------------------------------

fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

fn bucket_row(model: &quota_usage::Model) -> Option<BucketRow> {
    let period = match model.period_type.as_str() {
        "daily" => QuotaPeriod::Daily,
        "monthly" => QuotaPeriod::Monthly,
        _ => return None,
    };
    let bucket = match model.bucket.as_str() {
        "total" => QuotaBucket::Total,
        "tier:premium" => QuotaBucket::TierPremium,
        _ => return None,
    };
    Some(BucketRow {
        period,
        bucket,
        spent_credits_micro: model.spent_credits_micro,
        reserved_credits_micro: model.reserved_credits_micro,
        web_search_calls: i64::from(model.web_search_calls),
        code_interpreter_calls: i64::from(model.code_interpreter_calls),
    })
}

/// The preflight read in its read transaction: the four bucket rows in one
/// statement (`FOR UPDATE` on PostgreSQL), then the prior context tokens.
/// Nothing is written.
async fn read_preflight_in_tx(
    tx: &DbTx<'_>,
    read: &PreflightRead,
    lock: bool,
) -> Result<PreflightRows, DomainError> {
    let scope = owner_scope(read.tenant_id, read.user_id);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-read-buckets
    let buckets = read_rows(tx, &scope, &read.keys, &QuotaBucket::ALL, lock).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-read-buckets
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-prior-context
    let prior = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(read.chat_id))
                .add(message::Column::Role.eq("assistant"))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(message::Column::InputTokens.gt(0))
                        .add(message::Column::OutputTokens.gt(0)),
                ),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(read.tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-prior-context
    Ok(PreflightRows {
        prior_context_tokens: prior
            .map_or(0, |row| row.input_tokens.saturating_add(row.output_tokens)),
        buckets,
    })
}

/// The rows of `buckets` for both period starts in one statement, ordered in
/// the canonical quota row order (`period_type` `daily` before `monthly`, then
/// bucket `total` before `tier:premium`; the text order of the bucket would put
/// `tier:premium` first). With `lock` (PostgreSQL preflight only) the statement
/// is `FOR UPDATE`, so the row locks are taken in that order; SQLite has no row
/// locks and reads plainly.
async fn read_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    keys: &PeriodKeys,
    buckets: &[QuotaBucket],
    lock: bool,
) -> Result<Vec<BucketRow>, DomainError> {
    use quota_usage::Column as C;
    let mut by_period = Condition::any();
    for period in QuotaPeriod::ALL {
        by_period = by_period.add(
            Condition::all()
                .add(C::PeriodType.eq(period.as_str()))
                .add(C::PeriodStart.eq(keys.start(period))),
        );
    }
    let condition = Condition::all()
        .add(by_period)
        .add(C::Bucket.is_in(buckets.iter().map(|bucket| bucket.as_str())));
    let models = quota_usage::Entity::find()
        .filter(condition)
        .secure()
        .scope_with(scope)
        .project_all(runner, |select| {
            let bucket_rank: Expr = Expr::case(C::Bucket.eq(QuotaBucket::Total.as_str()), 0)
                .finally(1)
                .into();
            let select = select.order_by_asc(C::PeriodType).order_by_asc(bucket_rank);
            let select = if lock {
                select.lock_exclusive()
            } else {
                select
            };
            select.into_model::<quota_usage::Model>()
        })
        .await
        .map_err(|error| scope_error(&error))?;
    Ok(models.iter().filter_map(bucket_row).collect())
}

/// The key of one quota row.
#[derive(Clone, Copy)]
struct RowKey {
    tenant_id: Uuid,
    user_id: Uuid,
    period: QuotaPeriod,
    start: Date,
    bucket: QuotaBucket,
}

/// Inserts the row with `delta` applied to zero, or adds `delta` to the
/// existing row, in one statement.
async fn upsert_delta(
    runner: &impl DBRunner,
    key: RowKey,
    delta: &BucketDelta,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    use quota_usage::Column as C;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-reserve-scope
    let scope = owner_scope(key.tenant_id, key.user_id);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-reserve-scope
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-reserved-counters
    // @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-deferred:p1
    let active = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(key.tenant_id),
        user_id: Set(key.user_id),
        period_type: Set(key.period.as_str().to_owned()),
        period_start: Set(key.start),
        bucket: Set(key.bucket.as_str().to_owned()),
        spent_credits_micro: Set(delta.spent_credits_micro),
        reserved_credits_micro: Set(delta.reserved_credits_micro),
        calls: Set(delta.calls),
        input_tokens: Set(delta.input_tokens),
        output_tokens: Set(delta.output_tokens),
        file_search_calls: Set(0),
        web_search_calls: Set(delta.web_search_calls),
        code_interpreter_calls: Set(delta.code_interpreter_calls),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(now),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-reserved-counters
    let mut on_conflict = SecureOnConflict::<quota_usage::Entity>::columns([
        C::TenantId,
        C::UserId,
        C::PeriodType,
        C::PeriodStart,
        C::Bucket,
    ]);
    let additions: [(C, i64); 7] = [
        (C::ReservedCreditsMicro, delta.reserved_credits_micro),
        (C::SpentCreditsMicro, delta.spent_credits_micro),
        (C::Calls, i64::from(delta.calls)),
        (C::InputTokens, delta.input_tokens),
        (C::OutputTokens, delta.output_tokens),
        (C::WebSearchCalls, i64::from(delta.web_search_calls)),
        (
            C::CodeInterpreterCalls,
            i64::from(delta.code_interpreter_calls),
        ),
    ];
    // The right side is qualified with the table: PostgreSQL also knows the
    // pseudo-table `excluded` in `DO UPDATE`, so a bare column is ambiguous.
    for (column, amount) in additions {
        if amount != 0 {
            on_conflict = on_conflict
                .value(column, Expr::col((quota_usage::Entity, column)).add(amount))
                .map_err(|error| scope_error(&error))?;
        }
    }
    on_conflict = on_conflict
        .update_columns([C::UpdatedAt])
        .map_err(|error| scope_error(&error))?;
    quota_usage::Entity::insert(active.clone())
        .secure()
        .scope_with_model(&scope, &active)
        .map_err(|error| match error {
            ScopeError::Denied(_) | ScopeError::TenantNotInScope { .. } => DomainError::AuthzDenied,
            other => scope_error(&other),
        })?
        .on_conflict(on_conflict)
        .exec(runner)
        .await
        .map(|_| ())
        .map_err(|error| scope_error(&error))
}

// ---- reserve ------------------------------------------------------------------------------------

/// What one reserve attempt needs, owned so that every attempt can start from it.
struct ReservePlan {
    tenant_id: Uuid,
    user_id: Uuid,
    mode: ReserveMode,
    tier: mini_chat_sdk::ModelTier,
    limits: mini_chat_sdk::UserLimits,
    keys: PeriodKeys,
    now: OffsetDateTime,
    columns: ReservedColumns,
}

impl ReservePlan {
    fn from_request(request: &ReserveRequest<'_>) -> Self {
        Self {
            tenant_id: request.tenant_id,
            user_id: request.user_id,
            mode: request.mode,
            tier: request.decision.effective_tier,
            limits: request.decision.limits,
            keys: request.decision.period_keys,
            now: request.now,
            columns: ReservedColumns::from_decision(request.decision),
        }
    }
}

async fn reserve_in_tx(tx: &DbTx<'_>, plan: &ReservePlan) -> Result<(), DomainError> {
    let buckets = reserve_buckets(plan.tier);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-for-each-reserve-period
    for period in QuotaPeriod::ALL {
        for bucket in buckets {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-upsert-total
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-upsert-premium
            let delta = BucketDelta {
                bucket: *bucket,
                reserved_credits_micro: plan.columns.reserved_credits_micro,
                spent_credits_micro: 0,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                web_search_calls: 0,
                code_interpreter_calls: 0,
            };
            upsert_delta(
                tx,
                RowKey {
                    tenant_id: plan.tenant_id,
                    user_id: plan.user_id,
                    period,
                    start: plan.keys.start(period),
                    bucket: *bucket,
                },
                &delta,
                plan.now,
            )
            .await?;
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-upsert-premium
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-upsert-total
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-for-each-reserve-period
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-reread-buckets
    let scope = owner_scope(plan.tenant_id, plan.user_id);
    let rows = read_rows(tx, &scope, &plan.keys, buckets, false).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-reread-buckets
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-if-reserve-rejected
    if reserve_exceeds_limit(&rows, &plan.limits) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-return-recheck-over
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-return-reserve-rejected
        return Err(DomainError::QuotaExceeded(QuotaSubject::Tokens));
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-return-reserve-rejected
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-return-recheck-over
    }
    // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-if-reserve-rejected
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-fill-mode
    if let ReserveMode::Fill { turn_id } = plan.mode {
        fill_turn(tx, plan, turn_id).await?;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-fill-mode
    Ok(())
}

async fn fill_turn(tx: &DbTx<'_>, plan: &ReservePlan, turn_id: Uuid) -> Result<(), DomainError> {
    use chat_turn::Column as C;
    let columns = &plan.columns;
    let max_output = i32::try_from(columns.max_output_tokens_applied)
        .map_err(|_| DomainError::Internal("max_output_tokens_applied out of range".to_owned()))?;
    let floor = i32::try_from(columns.minimal_generation_floor_applied)
        .map_err(|_| DomainError::Internal("minimal_generation_floor out of range".to_owned()))?;
    let version = i64::try_from(columns.policy_version_applied)
        .map_err(|_| DomainError::Internal("policy_version out of range".to_owned()))?;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-fill-turn
    let result = chat_turn::Entity::update_many()
        .col_expr(C::ReserveTokens, Expr::value(columns.reserve_tokens))
        .col_expr(C::MaxOutputTokensApplied, Expr::value(max_output))
        .col_expr(
            C::ReservedCreditsMicro,
            Expr::value(columns.reserved_credits_micro),
        )
        .col_expr(C::PolicyVersionApplied, Expr::value(version))
        .col_expr(
            C::EffectiveModel,
            Expr::value(columns.effective_model.clone()),
        )
        .col_expr(C::MinimalGenerationFloorApplied, Expr::value(floor))
        .filter(
            Condition::all()
                .add(C::Id.eq(turn_id))
                .add(C::State.eq("running"))
                .add(C::ReserveTokens.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(plan.tenant_id))
        .exec(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-fill-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-fill-missed
    if result.rows_affected == 0 {
        return Err(DomainError::Internal(
            "the turn to fill is missing, not running or already reserved".to_owned(),
        ));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-reserve-write:p1:inst-if-fill-missed
    Ok(())
}

// ---- finalization ------------------------------------------------------------------------------

fn state_text(state: TerminalState) -> &'static str {
    match state {
        TerminalState::Completed => "completed",
        TerminalState::Failed => "failed",
        TerminalState::Cancelled => "cancelled",
    }
}

/// The CAS UPDATE; `true` when this call owns the turn.
// @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-cas-update
async fn cas_turn(tx: &DbTx<'_>, request: &FinalizeRequest) -> Result<bool, DomainError> {
    use chat_turn::Column as C;
    let columns = &request.columns;
    let mut condition = Condition::all()
        .add(C::Id.eq(request.turn_id))
        .add(C::State.eq("running"));
    if let FinalizeVariant::Orphan { stale_cutoff } = &request.variant {
        condition = condition.add(C::DeletedAt.is_null()).add(
            Expr::expr(Func::coalesce([
                Expr::col(C::LastProgressAt),
                Expr::col(C::StartedAt),
            ]))
            .lte(*stale_cutoff),
        );
    }
    let result = chat_turn::Entity::update_many()
        .col_expr(C::State, Expr::value(state_text(columns.state)))
        .col_expr(C::CompletedAt, Expr::value(request.now))
        .col_expr(C::UpdatedAt, Expr::value(request.now))
        .col_expr(C::ErrorCode, Expr::value(columns.error_code.clone()))
        .col_expr(C::ErrorDetail, Expr::value(columns.error_detail.clone()))
        .col_expr(
            C::AssistantMessageId,
            Expr::value(columns.assistant_message_id),
        )
        .col_expr(
            C::ProviderResponseId,
            Expr::value(columns.provider_response_id.clone()),
        )
        .filter(condition)
        .secure()
        .scope_with(&finalize_scope(&request.variant))
        .exec(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    Ok(result.rows_affected > 0)
}
// @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-cas-update

/// The access scope of the finalization statements on `chat_turns`.
///
/// The stream variant is scoped to the tenant of the request, so a foreign
/// tenant's turn matches no row and the finalization is `Lost`.
///
/// The orphan variant keeps `allow_all` as a trusted system boundary: the
/// watchdog runs system-scoped, there is no user or tenant context, and it
/// must finalize the stale turns of every tenant. Owner: the mini-chat gear
/// maintainers; the CAS still requires `state = 'running'`, `deleted_at IS
/// NULL` and the stale-progress cutoff, and the turn id comes from the
/// watchdog's own scan, never from a request.
fn finalize_scope(variant: &FinalizeVariant) -> AccessScope {
    match variant {
        FinalizeVariant::Stream { tenant_id, .. } => AccessScope::for_tenant(*tenant_id),
        FinalizeVariant::Orphan { .. } => AccessScope::allow_all(),
    }
}

async fn load_turn(
    tx: &DbTx<'_>,
    turn_id: Uuid,
    variant: &FinalizeVariant,
) -> Result<chat_turn::Model, DomainError> {
    chat_turn::Entity::find()
        .filter(chat_turn::Column::Id.eq(turn_id))
        .secure()
        .scope_with(&finalize_scope(variant))
        .one(tx)
        .await
        .map_err(|error| scope_error(&error))?
        .ok_or_else(|| DomainError::Internal("the finalized turn is not readable".to_owned()))
}

fn non_negative(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// The six preflight columns when all are set.
#[allow(clippy::struct_field_names)]
struct Reserve {
    reserve_tokens: i64,
    max_output_tokens: i64,
    reserved_credits_micro: i64,
    policy_version: u64,
    effective_model: String,
    floor: i64,
}

fn reserve_of(turn: &chat_turn::Model) -> Option<Reserve> {
    Some(Reserve {
        reserve_tokens: turn.reserve_tokens?,
        max_output_tokens: i64::from(turn.max_output_tokens_applied?),
        reserved_credits_micro: turn.reserved_credits_micro?,
        policy_version: u64::try_from(turn.policy_version_applied?).ok()?,
        effective_model: turn.effective_model.clone()?,
        floor: i64::from(turn.minimal_generation_floor_applied?),
    })
}

/// The per-request cost record `quota settled` of one won settlement; written
/// after the transaction commits, so a rolled-back finalization leaves no record.
struct CostRecord {
    chat_id: Uuid,
    turn_id: Uuid,
    request_id: Uuid,
    effective_model: String,
    input_tokens: i64,
    output_tokens: i64,
    reserved_credits_micro: i64,
    committed_credits_micro: i64,
    billing_outcome: BillingOutcome,
    settlement_method: SettlementMethod,
    tools: ToolCallCounts,
    orphan: bool,
}

impl CostRecord {
    fn log(&self) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-cost-log
        tracing::info!(
            chat_id = %self.chat_id,
            turn_id = %self.turn_id,
            request_id = %self.request_id,
            effective_model = %self.effective_model,
            input_tokens = self.input_tokens,
            output_tokens = self.output_tokens,
            reserved_credits_micro = self.reserved_credits_micro,
            committed_credits_micro = self.committed_credits_micro,
            released_credits_micro = Ord::max(
                self.reserved_credits_micro
                    .saturating_sub(self.committed_credits_micro),
                0
            ),
            billing_outcome = ?self.billing_outcome,
            settlement_method = ?self.settlement_method,
            web_search_calls = self.tools.web_search,
            code_interpreter_calls = self.tools.code_interpreter,
            file_search_calls = self.tools.file_search,
            orphan = self.orphan,
            "quota settled"
        );
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-cost-log
    }
}

#[allow(clippy::cognitive_complexity)]
async fn finalize_in_tx(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    tolerance: f64,
    request: &FinalizeRequest,
) -> Result<(FinalizeResult, Wake, Option<CostRecord>), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-write-first-finalize
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-cas-zero
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-if-cas-lost
    if !cas_turn(tx, request).await? {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-return-lost
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-return-cas-lost
        // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-terminal-noop
        return Ok((FinalizeResult::Lost, Wake::empty(), None));
        // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-terminal-noop
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-return-cas-lost
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-return-lost
    }
    // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-if-cas-lost
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-cas-zero
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-write-first-finalize
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-load-turn
    let turn = load_turn(tx, request.turn_id, &request.variant).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-load-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-derive-outcome
    let derivation = derive_billing_outcome(&request.classification, request.usage.as_ref());
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-derive-outcome
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-unknown-code
    if derivation.unknown_error_code {
        tracing::error!(
            turn_id = %turn.id,
            error_code = request.columns.error_code.as_deref().unwrap_or(""),
            "unknown error code classified as failed and settled estimated"
        );
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-unknown-code
    let mut won = FinalizeWon {
        terminal_state: derivation.terminal_state,
        billing_outcome: derivation.billing_outcome,
        settlement_method: derivation.settlement_method,
        committed_credits_micro: 0,
        settled: false,
        usage_event_enqueued: false,
        actual_input_tokens: 0,
        actual_output_tokens: 0,
        reserve_tokens: turn.reserve_tokens.unwrap_or(0),
        overshoot_capped: false,
        fallback: None,
        effective_model: turn.effective_model.clone(),
        code_interpreter_calls: 0,
        summary_trigger: None,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-no-requester
    let Some(user_id) = turn.requester_user_id else {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-skip-no-requester
        tracing::error!(
            turn_id = %turn.id,
            chat_id = %turn.chat_id,
            tenant_id = %turn.tenant_id,
            "turn without requester_user_id finalized without settlement and usage event"
        );
        return Ok((FinalizeResult::Won(Box::new(won)), Wake::empty(), None));
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-skip-no-requester
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-no-requester
    let orphan = matches!(request.variant, FinalizeVariant::Orphan { .. });
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-orphan
    let (usage, tools, selected_model, keys) = match &request.variant {
        FinalizeVariant::Stream {
            period_keys,
            selected_model,
            ..
        } => (
            request.usage,
            request.tool_calls,
            selected_model.clone(),
            *period_keys,
        ),
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-orphan-inputs
        FinalizeVariant::Orphan { .. } => (
            None,
            ToolCallCounts {
                web_search: non_negative(turn.web_search_completed_count),
                code_interpreter: non_negative(turn.code_interpreter_completed_count),
                file_search: non_negative(turn.file_search_completed_count),
            },
            turn.effective_model.clone().unwrap_or_default(),
            period_keys(turn.started_at),
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-orphan-inputs
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-orphan
    won.code_interpreter_calls = tools.code_interpreter;
    let mut event = TurnUsageEventArgs {
        tenant_id: turn.tenant_id,
        user_id,
        chat_id: turn.chat_id,
        turn_id: turn.id,
        request_id: turn.request_id,
        effective_model: turn.effective_model.clone().unwrap_or_default(),
        selected_model,
        terminal_state: derivation.terminal_state,
        billing_outcome: derivation.billing_outcome,
        settlement_method: derivation.settlement_method,
        committed_credits_micro: 0,
        policy_version_applied: turn
            .policy_version_applied
            .and_then(|version| u64::try_from(version).ok())
            .unwrap_or(0),
        usage,
        tool_calls: tools,
        now: request.now,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-no-reserve
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-unreserved
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-unreserved-orphaned
    let Some(reserve) = reserve_of(&turn) else {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-skip-settlement
        tracing::warn!(
            turn_id = %turn.id,
            "turn without reserve fields: no settlement, estimated aborted usage event with 0 credits"
        );
        event.billing_outcome = BillingOutcome::Aborted;
        event.settlement_method = SettlementMethod::Estimated;
        event.effective_model = String::new();
        event.selected_model = String::new();
        event.policy_version_applied = 0;
        event.usage = None;
        won.billing_outcome = BillingOutcome::Aborted;
        won.settlement_method = SettlementMethod::Estimated;
        won.usage_event_enqueued = true;
        let wake = enqueue_usage_event(tx, outbox, &build_turn_usage_event(&event)).await?;
        return Ok((FinalizeResult::Won(Box::new(won)), wake, None));
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-skip-settlement
    };
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-unreserved-orphaned
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-unreserved
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-if-no-reserve
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-else-has-reserve
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-load-settlement-snapshot
    let snapshot = request.snapshot.usable_for(reserve.policy_version);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-load-settlement-snapshot
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-settle
    let plan = plan_settlement(&SettleInput {
        reserve_tokens: reserve.reserve_tokens,
        max_output_tokens_applied: reserve.max_output_tokens,
        reserved_credits_micro: reserve.reserved_credits_micro,
        minimal_generation_floor_applied: reserve.floor,
        effective_model: &reserve.effective_model,
        snapshot,
        method: derivation.settlement_method,
        usage: usage.as_ref(),
        overshoot_tolerance_factor: tolerance,
    });
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-log
    if let Some(cause) = &plan.fallback {
        tracing::error!(
            turn_id = %turn.id,
            chat_id = %turn.chat_id,
            request_id = %turn.request_id,
            effective_model = %reserve.effective_model,
            policy_version_applied = reserve.policy_version,
            cause = %describe(cause),
            original_method = ?derivation.settlement_method,
            committed_credits_micro = plan.committed_credits_micro,
            "quota settlement fallback"
        );
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-log
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-tier
    let premium = match plan.tier {
        Some(tier) => tier == mini_chat_sdk::ModelTier::Premium,
        None => premium_by_rows(tx, &turn, user_id, &keys, reserve.reserved_credits_micro).await?,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-tier
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-for-each-settle-period
    for period in QuotaPeriod::ALL {
        for delta in bucket_deltas(&plan, reserve.reserved_credits_micro, tools, premium) {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-settle-total
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-settle-premium
            upsert_delta(
                tx,
                RowKey {
                    tenant_id: turn.tenant_id,
                    user_id,
                    period,
                    start: keys.start(period),
                    bucket: delta.bucket,
                },
                &delta,
                request.now,
            )
            .await?;
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-settle-premium
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-settle-total
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-for-each-settle-period
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-settle
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-enqueue-usage
    event.settlement_method = plan.method;
    event.committed_credits_micro = plan.committed_credits_micro;
    let wake = enqueue_usage_event(tx, outbox, &build_turn_usage_event(&event)).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-enqueue-usage
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-cost-record
    let cost = CostRecord {
        chat_id: turn.chat_id,
        turn_id: turn.id,
        request_id: turn.request_id,
        effective_model: reserve.effective_model.clone(),
        input_tokens: plan.telemetry_input_tokens,
        output_tokens: plan.telemetry_output_tokens,
        reserved_credits_micro: reserve.reserved_credits_micro,
        committed_credits_micro: plan.committed_credits_micro,
        billing_outcome: derivation.billing_outcome,
        settlement_method: plan.method,
        tools,
        orphan,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-cost-record
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-return-settlement
    won.settlement_method = plan.method;
    won.committed_credits_micro = plan.committed_credits_micro;
    won.settled = true;
    won.usage_event_enqueued = true;
    won.actual_input_tokens = plan.telemetry_input_tokens;
    won.actual_output_tokens = plan.telemetry_output_tokens;
    won.reserve_tokens = reserve.reserve_tokens;
    won.overshoot_capped = plan.overshoot_capped;
    won.fallback = plan.fallback;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-return-settlement
    won.effective_model = Some(reserve.effective_model);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-else-has-reserve
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-return-won
    Ok((FinalizeResult::Won(Box::new(won)), wake, Some(cost)))
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-return-won
}

fn describe(cause: &FallbackCause) -> String {
    match cause {
        FallbackCause::SnapshotUnavailable => "snapshot unavailable".to_owned(),
        FallbackCause::ModelMissing => "effective model missing from the snapshot".to_owned(),
        FallbackCause::Credit(error) => format!("credit error: {error}"),
    }
}

/// The turn is premium when both `tier:premium` rows of its periods exist and
/// each holds at least the turn's reserve.
async fn premium_by_rows(
    tx: &DbTx<'_>,
    turn: &chat_turn::Model,
    user_id: Uuid,
    keys: &PeriodKeys,
    turn_reserved: i64,
) -> Result<bool, DomainError> {
    let scope = owner_scope(turn.tenant_id, user_id);
    let rows = read_rows(tx, &scope, keys, &[QuotaBucket::TierPremium], false).await?;
    Ok(QuotaPeriod::ALL.iter().all(|period| {
        rows.iter().any(|row| {
            row.period == *period
                && row.bucket == QuotaBucket::TierPremium
                && row.reserved_credits_micro >= turn_reserved
        })
    }))
}

#[cfg(test)]
#[path = "quota_preflight_tests.rs"]
mod preflight_tests;

#[cfg(test)]
#[path = "quota_reserve_tests.rs"]
mod reserve_tests;

#[cfg(test)]
#[path = "quota_finalize_tests.rs"]
mod finalize_tests;

#[cfg(test)]
#[path = "quota_fallback_tests.rs"]
mod fallback_tests;
