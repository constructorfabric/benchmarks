//! The `SQLite` and `PostgreSQL` implementation of the thread-summary persistence port: the
//! frontier pre-check, the range selection and the compare-and-set commit
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck`,
//! `-summary-range-selection` and `-summary-commit`).
//!
//! On `SQLite` every timestamp order, range and equality uses the fixed-width sort key of
//! `chat_page` with `id` as tie-break; the commit takes the database write lock before its first
//! read; on `PostgreSQL` the target message row is locked with `FOR UPDATE`.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, Set, Value};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::maintenance::summary_prompt::RangeMessage;
use crate::domain::maintenance::summary_store::{
    CommitOutcome, PreCheck, SummaryCommit, SummaryStore,
};
use crate::domain::quota::UsageQueue;
use crate::domain::streaming::summary::{Frontier, ThreadSummaryWork};
use crate::infra::storage::entity::{chat, message, thread_summary};
use crate::infra::storage::keyset_page::{TimeStorage, time_bound, time_key};
use crate::infra::storage::stream_store::time_storage;

fn scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// `instant` truncated to microseconds, the precision every supported database keeps.
fn micros(instant: OffsetDateTime) -> OffsetDateTime {
    instant
        .replace_microsecond(instant.microsecond())
        .unwrap_or(instant)
}

fn created_at_key(storage: TimeStorage) -> Expr {
    time_key(storage, Expr::col(message::Column::CreatedAt))
}

fn bound(storage: TimeStorage, instant: OffsetDateTime) -> Result<Value, DomainError> {
    time_bound(storage, instant)
        .map_err(|e| DomainError::internal(format!("cannot bind the timestamp: {e}")))
}

/// The messages at or before (`at_or_before`) or strictly after the frontier in the order
/// `(created_at, id)`.
fn position(
    storage: TimeStorage,
    frontier: &Frontier,
    at_or_before: bool,
) -> Result<Condition, DomainError> {
    let bound = bound(storage, frontier.created_at)?;
    let key = created_at_key(storage);
    let (by_time, by_id) = if at_or_before {
        (
            key.clone().lt(bound.clone()),
            message::Column::Id.lte(frontier.message_id),
        )
    } else {
        (
            key.clone().gt(bound.clone()),
            message::Column::Id.gt(frontier.message_id),
        )
    };
    Ok(Condition::any()
        .add(by_time)
        .add(Condition::all().add(key.eq(bound)).add(by_id)))
}

/// The messages of the frozen range: live, uncompressed, after the base (when not empty) and at
/// or before the target.
fn range_filter(storage: TimeStorage, work: &ThreadSummaryWork) -> Result<Condition, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-select
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-order
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-exclude-later
    // the position is compared through the fixed-width sort key on SQLite with `id` as tie-break
    // and through the native type on PostgreSQL; a message created after the frozen target is
    // excluded: it neither cancels, widens nor invalidates this run
    let mut filter = Condition::all()
        .add(message::Column::ChatId.eq(work.chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(position(storage, &work.frozen_target, true)?);
    if let Some(base) = &work.base_frontier {
        filter = filter.add(position(storage, base, false)?);
    }
    Ok(filter)
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-exclude-later
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-order
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-select
}

async fn live_target(
    runner: &impl toolkit_db::secure::DBRunner,
    work: &ThreadSummaryWork,
) -> Result<bool, DomainError> {
    let target = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::Id.eq(work.frozen_target.message_id))
                .add(message::Column::ChatId.eq(work.chat_id)),
        )
        .secure()
        .scope_with(&scope(work.tenant_id))
        .one(runner)
        .await?;
    Ok(target.is_some_and(|row| row.deleted_at.is_none()))
}

#[async_trait]
impl SummaryStore for DBProvider<DomainError> {
    async fn precheck(&self, work: &ThreadSummaryWork) -> Result<PreCheck, DomainError> {
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-load-chat
        // the chat is read without the deleted filter, by the payload's tenant and chat
        let chat_row = chat::Entity::find()
            .filter(chat::Column::Id.eq(work.chat_id))
            .secure()
            .scope_with(&scope(work.tenant_id))
            .one(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-load-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-chat-gone
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-chat-gone
        let live_chat = chat_row.is_some_and(|row| row.deleted_at.is_none());
        if !live_chat {
            return Ok(PreCheck::ChatGone);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-chat-gone
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-chat-gone
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-load-summary
        let stored = thread_summary::Entity::find()
            .filter(thread_summary::Column::ChatId.eq(work.chat_id))
            .secure()
            .scope_with(&scope(work.tenant_id))
            .one(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-load-summary
        match (&work.base_frontier, &stored) {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-base-missing
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-base-missing
            (Some(_), None) => return Ok(PreCheck::BaseMissing),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-base-missing
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-base-missing
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-moved
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-moved
            (None, Some(_)) => return Ok(PreCheck::FrontierMoved),
            (Some(base), Some(row))
                if micros(row.summarized_up_to_created_at) != micros(base.created_at)
                    || row.summarized_up_to_message_id != base.message_id =>
            {
                return Ok(PreCheck::FrontierMoved);
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-moved
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-moved
            _ => {}
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-load-target
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-target-deleted
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-target-deleted
        if !live_target(&conn, work).await? {
            return Ok(PreCheck::TargetDeleted);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-target-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-target-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-load-target
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-db-error
        // every query above returns its database failure through `?`: the handler decides the
        // retry budget
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-db-error
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-ready
        Ok(PreCheck::Ready {
            base_summary: work
                .base_frontier
                .as_ref()
                .and(stored)
                .map(|row| row.summary_text),
        })
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck:p1:inst-pc-return-ready
    }

    async fn load_range(&self, work: &ThreadSummaryWork) -> Result<Vec<RangeMessage>, DomainError> {
        let conn = self.conn()?;
        let storage = time_storage(self);
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-select
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-order
        let rows = message::Entity::find()
            .filter(range_filter(storage, work)?)
            .secure()
            .scope_with(&scope(work.tenant_id))
            .order_by(created_at_key(storage), Order::Asc)
            .order_by(message::Column::Id, Order::Asc)
            .all(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-order
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-select
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-return
        Ok(rows
            .into_iter()
            .map(|row| RangeMessage {
                id: row.id,
                role: row.role,
                content: row.content,
                created_at: row.created_at,
            })
            .collect())
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection:p1:inst-rs-return
    }

    async fn commit(
        &self,
        commit: &SummaryCommit,
        usage: Arc<dyn UsageQueue>,
    ) -> Result<CommitOutcome, DomainError> {
        let storage = time_storage(self);
        let commit = commit.clone();
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-try
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-commit
        // the summary, the frontier, the compressed range and the usage event commit together or
        // not at all
        let outcome = self
            .transaction(move |tx| {
                let commit = commit.clone();
                let usage = Arc::clone(&usage);
                Box::pin(async move {
                    let work = &commit.work;
                    let tenant = scope(work.tenant_id);
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-begin
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-lock-target
                    // the first statement takes the write lock on SQLite and the row lock on
                    // PostgreSQL: a concurrent mutation that soft-deletes the target and this
                    // commit are ordered, whichever commits second sees the other's result
                    let locked = message::Entity::update_many()
                        .col_expr(
                            message::Column::IsCompressed,
                            Expr::col(message::Column::IsCompressed),
                        )
                        .filter(
                            Condition::all()
                                .add(message::Column::Id.eq(work.frozen_target.message_id))
                                .add(message::Column::ChatId.eq(work.chat_id)),
                        )
                        .secure()
                        .scope_with(&tenant)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-lock-target
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-begin
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-target-deleted
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-return-target-deleted
                    if locked == 0 || !live_target(tx, work).await? {
                        // nothing was written: no summary, no `is_compressed` change and no usage
                        // event; the deletion of a committed summary by a retry, edit or delete is
                        // owned by the turn mutations
                        // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-invalidated
                        // the transition from a committed summary to none is never made here
                        // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-invalidated
                        return Ok(CommitOutcome::TargetDeleted);
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-return-target-deleted
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-target-deleted
                    let stored = thread_summary::Entity::find()
                        .filter(thread_summary::Column::ChatId.eq(work.chat_id))
                        .secure()
                        .scope_with(&tenant)
                        .one(tx)
                        .await?;
                    match &work.base_frontier {
                        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-first
                        None => {
                            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-insert-conflict
                            if stored.is_some() {
                                // another first summary committed
                                return Ok(CommitOutcome::FrontierMoved);
                            }
                            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-insert-conflict
                            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-insert
                            // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-first-commit
                            let row = thread_summary::ActiveModel {
                                id: Set(Uuid::new_v4()),
                                tenant_id: Set(work.tenant_id),
                                chat_id: Set(work.chat_id),
                                summary_text: Set(commit.summary_text.clone()),
                                summarized_up_to_created_at: Set(work.frozen_target.created_at),
                                summarized_up_to_message_id: Set(work.frozen_target.message_id),
                                token_estimate: Set(commit.token_estimate),
                                created_at: Set(commit.now),
                                updated_at: Set(commit.now),
                            };
                            secure_insert::<thread_summary::Entity>(row, &tenant, tx).await?;
                            // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-first-commit
                            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-insert
                        }
                        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-first
                        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-advance
                        Some(base) => {
                            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-update-conflict
                            let Some(stored) = stored else {
                                return Ok(CommitOutcome::BaseMissing);
                            };
                            let equal = micros(stored.summarized_up_to_created_at)
                                == micros(base.created_at)
                                && stored.summarized_up_to_message_id == base.message_id;
                            if !equal {
                                return Ok(CommitOutcome::FrontierMoved);
                            }
                            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-update-conflict
                            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-update
                            // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-advance
                            let key = time_key(
                                storage,
                                Expr::col(thread_summary::Column::SummarizedUpToCreatedAt),
                            );
                            let affected = thread_summary::Entity::update_many()
                                .col_expr(
                                    thread_summary::Column::SummaryText,
                                    Expr::value(commit.summary_text.clone()),
                                )
                                .col_expr(
                                    thread_summary::Column::SummarizedUpToCreatedAt,
                                    Expr::value(work.frozen_target.created_at),
                                )
                                .col_expr(
                                    thread_summary::Column::SummarizedUpToMessageId,
                                    Expr::value(work.frozen_target.message_id),
                                )
                                .col_expr(
                                    thread_summary::Column::TokenEstimate,
                                    Expr::value(commit.token_estimate),
                                )
                                .col_expr(
                                    thread_summary::Column::UpdatedAt,
                                    Expr::value(commit.now),
                                )
                                .filter(
                                    Condition::all()
                                        .add(thread_summary::Column::ChatId.eq(work.chat_id))
                                        .add(
                                            thread_summary::Column::SummarizedUpToMessageId
                                                .eq(base.message_id),
                                        )
                                        .add(key.eq(bound(storage, base.created_at)?)),
                                )
                                .secure()
                                .scope_with(&tenant)
                                .exec(tx)
                                .await?
                                .rows_affected;
                            // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-advance
                            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-update
                            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-update-conflict
                            if affected == 0 {
                                return Ok(CommitOutcome::FrontierMoved);
                            }
                            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-update-conflict
                        } // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-advance
                    }
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-compress
                    // exactly the frozen range, with the predicate and the sort key of the range
                    // selection
                    message::Entity::update_many()
                        .col_expr(message::Column::IsCompressed, Expr::value(true))
                        .filter(range_filter(storage, work)?)
                        .secure()
                        .scope_with(&tenant)
                        .exec(tx)
                        .await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-compress
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-usage
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-enqueue
                    // the usage event is enqueued on the usage queue, partitioned by `tenant_id`,
                    // with the dedupe prefix validation of the foundation's enqueuer, in this
                    // transaction
                    let wake = usage.enqueue_usage(tx, &commit.usage_event).await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-enqueue
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-usage
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-no-turn
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-no-audit
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-return
                    // no `chat_turns` row, turn state, finalization CAS, quota preflight, reserve
                    // or settlement and no `quota_usage` row is touched; no audit event is
                    // emitted, no kill switch is checked and no tenant operational bucket is
                    // debited; the enqueue wake is returned
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-return
                    Ok(CommitOutcome::Committed(wake))
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-return
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-return
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-no-audit
                    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-no-turn
                })
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-commit
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-try
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-catch
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-return-error
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-single-commit
        // @cpt-begin:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-unchanged
        // a database failure, an enqueue error or a non-committed result leaves nothing of this
        // attempt: the row, the frontier and `is_compressed` stay unchanged, and no second commit
        // is attempted for the frozen range
        match outcome {
            // a concurrent first insert that the unique `chat_id` rejected lost the compare-and-set
            Err(DomainError::UniqueViolation { .. }) => Ok(CommitOutcome::FrontierMoved),
            other => other,
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-maintenance-summary-frontier:p1:inst-fr-unchanged
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-single-commit
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-return-error
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-commit:p1:inst-cm-catch
    }
}

#[cfg(test)]
#[path = "summary_repo_tests.rs"]
mod summary_repo_tests;

#[cfg(test)]
#[path = "summary_concurrency_tests.rs"]
mod summary_concurrency_tests;
