//! The Secure ORM implementation of the [`SummaryStore`] port and the
//! statements of the thread-summary trigger
//! (`cpt-cf-mini-chat-feature-background-workers`, thread summary).
//!
//! The summary commit is one write-first transaction: it locks the frozen
//! target message first (`SELECT ... FOR UPDATE` on `PostgreSQL`; a no-op write of
//! the row on `SQLite`, which has no row locks and takes its write lock with the
//! first write) and checks `deleted_at IS NULL` before any summary write. A
//! mutation soft-deletes its messages first, so whichever of the two commits
//! second sees the other's result. A lock or serialization conflict re-runs the
//! closure, at most `MAX_TX_ATTEMPTS` attempts in total; the closure is
//! deterministic (the summary id, `now` and the usage payload are captured by
//! value).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, DbBackend, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
};
use toolkit_db::contention::is_retryable_contention;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::quota::events::SystemTaskUsageArgs;
use crate::domain::service::quota::reserve::{CONFLICT_BACKOFF, MAX_TX_ATTEMPTS};
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::workers::ports::{
    CommitOutcome, StoredSummary, SummaryCommit, SummaryStore,
};
use crate::domain::service::workers::summary_prompt::RangeMessage;
use crate::domain::service::workers::summary_trigger::{
    Precheck, SummaryTask, SummaryTrigger, TriggerResult, boundary, frozen_range, precheck,
};
use crate::infra::db::entities::{chat, message, thread_summary};
use crate::infra::db::mapping::scope_error;
use crate::infra::outbox::queues::OutboxEnqueuer;
use crate::infra::outbox::thread_summary::enqueue_summary_task;
use crate::infra::outbox::usage::enqueue_system_task_usage;

type TxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>;

fn scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// `(created_at, id)` strictly after `at`.
fn after(at: &MessageBoundary) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.gt(at.created_at))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(at.created_at))
                .add(message::Column::Id.gt(at.id)),
        )
}

/// `(created_at, id)` at most `at`.
fn at_most(at: &MessageBoundary) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.lt(at.created_at))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(at.created_at))
                .add(message::Column::Id.lte(at.id)),
        )
}

/// The range predicate of the commit and of the range read: live messages of
/// the chat in `(base, target]`.
fn range_condition(
    chat_id: Uuid,
    base: Option<&MessageBoundary>,
    target: &MessageBoundary,
) -> Condition {
    let mut condition = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(at_most(target));
    if let Some(base) = base {
        condition = condition.add(after(base));
    }
    condition
}

fn summary_frontier(row: &thread_summary::Model) -> MessageBoundary {
    boundary(
        row.summarized_up_to_created_at,
        row.summarized_up_to_message_id,
    )
}

/// The trigger statements inside the finalization transaction
/// (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`, steps 5
/// to 10): the base frontier, the frozen target and, when a work item is due, its
/// enqueue. A failure fails the finalization transaction like any statement of
/// its closure.
///
/// # Errors
///
/// [`DomainError::Database`] / [`DomainError::Internal`].
pub async fn evaluate_summary_trigger(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    trigger: &SummaryTrigger,
) -> Result<(TriggerResult, Option<Wake>), DomainError> {
    if let Precheck::Done(result) = precheck(trigger) {
        return Ok((result, None));
    }
    let tenant = scope(trigger.tenant_id);
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-read-base-frontier
    let base = thread_summary::Entity::find()
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(trigger.chat_id)))
        .secure()
        .scope_with(&tenant)
        .one(tx)
        .await
        .map_err(|error| scope_error(&error))?
        .map(|row| summary_frontier(&row));
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-read-base-frontier
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-select-frozen-target
    let target = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(trigger.chat_id))
                .add(message::Column::DeletedAt.is_null())
                // NULL is "not the causing turn": `<>` alone drops such rows.
                .add(
                    Condition::any()
                        .add(message::Column::RequestId.is_null())
                        .add(message::Column::RequestId.ne(trigger.causing_request_id)),
                ),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .secure()
        .scope_with(&tenant)
        .one(tx)
        .await
        .map_err(|error| scope_error(&error))?
        .map(|row| boundary(row.created_at, row.id));
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-select-frozen-target
    let Some(range) = frozen_range(base, target) else {
        return Ok((TriggerResult::NotNeeded, None));
    };
    let wake = enqueue_summary_task(tx, outbox, &SummaryTask::new(trigger, range)).await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-return-trigger-result
    Ok((TriggerResult::Scheduled, Some(wake)))
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-return-trigger-result
}

/// Secure ORM store of the thread-summary handler.
pub struct SeaSummaryStore {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SeaSummaryStore {
    /// Creates the store over the database provider and the shared outbox
    /// enqueuer (the usage queue).
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }

    /// Runs `attempt` in a transaction; a lock or serialization conflict re-runs
    /// it, at most `MAX_TX_ATTEMPTS` attempts in total with a short backoff.
    async fn run_tx<F>(&self, attempt: F) -> Result<(CommitOutcome, Option<Wake>), DomainError>
    where
        F: for<'a> Fn(&'a DbTx<'a>) -> TxFuture<'a, (CommitOutcome, Option<Wake>)>
            + Send
            + Sync
            + 'static,
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
            match result {
                Err(DomainError::Database(message))
                    if number < MAX_TX_ATTEMPTS
                        && is_retryable_contention(
                            backend,
                            &sea_orm::DbErr::Custom(message.clone()),
                        ) =>
                {
                    let pause = CONFLICT_BACKOFF
                        .get(usize::try_from(number - 1).unwrap_or(0))
                        .copied()
                        .unwrap_or_default();
                    tokio::time::sleep(pause).await;
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-return-commit-error
                other => return other,
                // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-return-commit-error
            }
        }
    }
}

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-summary-commit:p1
/// The commit transaction body
/// (`cpt-cf-mini-chat-algo-background-workers-commit-summary`).
async fn commit_in_tx(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    commit: &SummaryCommit,
    backend: DbBackend,
) -> Result<(CommitOutcome, Option<Wake>), DomainError> {
    let tenant = scope(commit.tenant_id);
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-lock-frontier
    let target_filter = Condition::all()
        .add(message::Column::Id.eq(commit.target.id))
        .add(message::Column::ChatId.eq(commit.chat_id));
    if backend == DbBackend::Sqlite {
        // Write-first: SQLite takes its write lock with the first write.
        message::Entity::update_many()
            .col_expr(
                message::Column::IsCompressed,
                Expr::col(message::Column::IsCompressed),
            )
            .filter(target_filter.clone())
            .secure()
            .scope_with(&tenant)
            .exec(tx)
            .await
            .map_err(|error| scope_error(&error))?;
    }
    let lock = backend == DbBackend::Postgres;
    let target_row = message::Entity::find()
        .filter(target_filter)
        .secure()
        .scope_with(&tenant)
        .project_all(tx, |select| {
            let select = if lock {
                select.lock_exclusive()
            } else {
                select
            };
            select.into_model::<message::Model>()
        })
        .await
        .map_err(|error| scope_error(&error))?
        .into_iter()
        .next();
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-lock-frontier
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-if-frontier-gone
    if target_row.is_none_or(|row| row.deleted_at.is_some()) {
        return Ok((CommitOutcome::FrontierDeleted, None));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-if-frontier-gone
    match &commit.base {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-insert-summary
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-first-summary
        None => {
            let active = thread_summary::ActiveModel {
                id: Set(commit.summary_id),
                tenant_id: Set(commit.tenant_id),
                chat_id: Set(commit.chat_id),
                summary_text: Set(commit.summary_text.clone()),
                summarized_up_to_created_at: Set(commit.target.created_at),
                summarized_up_to_message_id: Set(commit.target.id),
                token_estimate: Set(commit.token_estimate),
                created_at: Set(commit.now),
                updated_at: Set(commit.now),
            };
            match secure_insert::<thread_summary::Entity>(active, &tenant, tx).await {
                Ok(_) => {}
                Err(error) => {
                    return match scope_error(&error) {
                        DomainError::UniqueViolation => Ok((CommitOutcome::CasConflict, None)),
                        other => Err(other),
                    };
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-first-summary
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-insert-summary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-cas-summary
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-advance-summary
        Some(base) => {
            let updated = thread_summary::Entity::update_many()
                .col_expr(
                    thread_summary::Column::SummaryText,
                    Expr::value(commit.summary_text.clone()),
                )
                .col_expr(
                    thread_summary::Column::SummarizedUpToCreatedAt,
                    Expr::value(commit.target.created_at),
                )
                .col_expr(
                    thread_summary::Column::SummarizedUpToMessageId,
                    Expr::value(commit.target.id),
                )
                .col_expr(
                    thread_summary::Column::TokenEstimate,
                    Expr::value(commit.token_estimate),
                )
                .col_expr(thread_summary::Column::UpdatedAt, Expr::value(commit.now))
                .filter(
                    Condition::all()
                        .add(thread_summary::Column::ChatId.eq(commit.chat_id))
                        .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(base.created_at))
                        .add(thread_summary::Column::SummarizedUpToMessageId.eq(base.id)),
                )
                .secure()
                .scope_with(&tenant)
                .exec(tx)
                .await
                .map_err(|error| scope_error(&error))?;
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-if-cas-summary-lost
            if updated.rows_affected == 0 {
                let remaining = thread_summary::Entity::find()
                    .filter(Condition::all().add(thread_summary::Column::ChatId.eq(commit.chat_id)))
                    .secure()
                    .scope_with(&tenant)
                    .one(tx)
                    .await
                    .map_err(|error| scope_error(&error))?;
                return Ok((
                    if remaining.is_none() {
                        CommitOutcome::BaseMissing
                    } else {
                        CommitOutcome::CasConflict
                    },
                    None,
                ));
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-if-cas-summary-lost
        } // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-advance-summary
          // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-cas-summary
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-mark-compressed
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(range_condition(
            commit.chat_id,
            commit.base.as_ref(),
            &commit.target,
        ))
        .secure()
        .scope_with(&tenant)
        .exec(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-mark-compressed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-enqueue-summary-usage
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-system-task-isolation
    let wake = enqueue_system_task_usage(
        tx,
        outbox,
        &SystemTaskUsageArgs {
            tenant_id: commit.tenant_id,
            chat_id: commit.chat_id,
            system_request_id: commit.system_request_id,
            summary_model: commit.summary_model.clone(),
            usage: commit.usage,
            now: commit.now,
        },
    )
    .await?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-system-task-isolation
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-enqueue-summary-usage
    Ok((CommitOutcome::Committed, Some(wake)))
}

#[async_trait]
impl SummaryStore for SeaSummaryStore {
    async fn chat_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<bool>, DomainError> {
        let conn = self.db.conn()?;
        chat::Entity::find()
            .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await
            .map(|row| row.map(|chat| chat.deleted_at.is_some()))
            .map_err(|error| scope_error(&error))
    }

    async fn summary(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoredSummary>, DomainError> {
        let conn = self.db.conn()?;
        thread_summary::Entity::find()
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await
            .map(|row| {
                row.map(|row| StoredSummary {
                    text: row.summary_text.clone(),
                    frontier: summary_frontier(&row),
                })
            })
            .map_err(|error| scope_error(&error))
    }

    async fn message_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<bool>, DomainError> {
        let conn = self.db.conn()?;
        message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::Id.eq(message_id))
                    .add(message::Column::ChatId.eq(chat_id)),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await
            .map(|row| row.map(|row| row.deleted_at.is_some()))
            .map_err(|error| scope_error(&error))
    }

    async fn range(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<MessageBoundary>,
        target: MessageBoundary,
    ) -> Result<Vec<RangeMessage>, DomainError> {
        let conn = self.db.conn()?;
        let rows = message::Entity::find()
            .filter(range_condition(chat_id, base.as_ref(), &target))
            .order_by(message::Column::CreatedAt, Order::Asc)
            .order_by(message::Column::Id, Order::Asc)
            .secure()
            .scope_with(&scope(tenant_id))
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(rows
            .into_iter()
            .map(|row| RangeMessage {
                id: row.id,
                role: row.role,
                content: row.content,
                created_at: row.created_at,
            })
            .collect())
    }

    async fn commit(&self, commit: &SummaryCommit) -> Result<CommitOutcome, DomainError> {
        let commit = Arc::new(commit.clone());
        let outbox = Arc::clone(&self.outbox);
        let backend = self.db.db().backend();
        let (outcome, wake) = self
            .run_tx(move |tx| {
                let (commit, outbox) = (Arc::clone(&commit), Arc::clone(&outbox));
                Box::pin(async move { commit_in_tx(tx, &outbox, &commit, backend).await })
            })
            .await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-commit-summary
        if let Some(wake) = wake {
            wake.fire();
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-commit-summary
        Ok(outcome)
    }
}

#[cfg(test)]
#[path = "summaries_trigger_tests.rs"]
mod trigger_tests;

#[cfg(test)]
#[path = "summaries_commit_tests.rs"]
mod commit_tests;
