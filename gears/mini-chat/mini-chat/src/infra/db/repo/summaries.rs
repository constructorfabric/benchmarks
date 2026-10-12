//! Thread-summary execution persistence (DESIGN section 3.6 "Thread Summary
//! Update", "Stable Range and Commit Invariant", section 3.7
//! `thread_summaries`, "Summary Interaction on Turn Mutation").
//!
//! - the frozen range of a work item: the non-deleted, non-compressed
//!   messages of the chat with `base < (created_at, id) <= target`, ordered
//!   by `(created_at, id)` ascending;
//! - the commit transaction ([`commit_in_tx`]):
//!   1. a no-op `UPDATE` of the live target frontier message, the first
//!      statement (on `SQLite` it takes the write lock before any read; on
//!      PostgreSQL it locks the row like `SELECT ... FOR UPDATE`, ordering
//!      the commit with a retry / edit / delete that soft-deletes it). No
//!      row means the target was deleted: nothing is written;
//!   2. the compare-and-set upsert of `thread_summaries`: without a base,
//!      an insert when the chat has no row; with a base, an `UPDATE` guarded
//!      by the stored frontier equal to the base. A lost CAS writes nothing;
//!   3. `is_compressed = true` on exactly the range;
//!   4. the system usage event on the usage queue (partition: tenant).
//!
//! The outbox wake fires after the commit. Timestamps are bound through the
//! database's [`TimestampForm`].

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::messages::{active_in_chat, after, at_or_before, thread_summary};
use super::{DbRepository, child_scope};
use crate::domain::error::DomainError;
use crate::domain::ports::{
    ContextMessageRecord, MessageKey, MessageRole, ThreadSummaryCommit, ThreadSummaryCommitOutcome,
    ThreadSummaryRecord, ThreadSummaryStore,
};
use crate::infra::db::entities::{TimestampForm, messages, thread_summaries};
use crate::infra::outbox::OutboxEnqueuer;

/// `base < (created_at, id) <= target` (no lower bound without a base).
fn range_condition(ts: TimestampForm, base: Option<MessageKey>, target: MessageKey) -> Condition {
    let mut condition = Condition::all().add(at_or_before(ts, target));
    if let Some(base) = base {
        condition = condition.add(after(ts, base));
    }
    condition
}

/// The non-deleted, non-compressed messages of `chat_id` in `(base,
/// target]`, chronological.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored role outside the
/// value set.
pub async fn summary_range(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    base: Option<MessageKey>,
    target: MessageKey,
) -> Result<Vec<ContextMessageRecord>, DomainError> {
    let rows = messages::Entity::find()
        .filter(
            active_in_chat(chat_id)
                .add(messages::Column::IsCompressed.eq(false))
                .add(range_condition(ts, base, target)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(messages::Column::CreatedAt, Order::Asc)
        .order_by(messages::Column::Id, Order::Asc)
        .all(runner)
        .await?;
    rows.into_iter()
        .map(|model| {
            let role = MessageRole::parse(&model.role).ok_or_else(|| {
                DomainError::internal(format!(
                    "message {} has an unknown stored role `{}`",
                    model.id, model.role
                ))
            })?;
            Ok(ContextMessageRecord {
                id: model.id,
                role,
                content: model.content,
                created_at: model.created_at.at(),
            })
        })
        .collect()
}

/// Locks the live message `target` of `chat_id` without changing it (a
/// no-op `UPDATE`); `false` when it is soft-deleted or gone.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn lock_live_message(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    target: Uuid,
) -> Result<bool, DomainError> {
    let result = messages::Entity::update_many()
        .col_expr(
            messages::Column::IsCompressed,
            Expr::col(messages::Column::IsCompressed),
        )
        .filter(active_in_chat(chat_id).add(messages::Column::Id.eq(target)))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// The CAS upsert of the summary row; the outcome when the CAS is lost.
async fn upsert_summary(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    commit: &ThreadSummaryCommit,
) -> Result<Option<ThreadSummaryCommitOutcome>, DomainError> {
    let scope = child_scope(chat_scope);
    let Some(base) = commit.base_frontier else {
        if thread_summary(runner, chat_scope, commit.chat_id)
            .await?
            .is_some()
        {
            return Ok(Some(ThreadSummaryCommitOutcome::CasConflict));
        }
        let row = thread_summaries::ActiveModel {
            id: Set(commit.summary_id),
            tenant_id: Set(commit.tenant_id),
            chat_id: Set(commit.chat_id),
            summary_text: Set(commit.summary_text.clone()),
            summarized_up_to_created_at: Set(ts.stamp(commit.target.created_at)),
            summarized_up_to_message_id: Set(commit.target.id),
            token_estimate: Set(commit.token_estimate),
            created_at: Set(ts.stamp(commit.at)),
            updated_at: Set(ts.stamp(commit.at)),
        };
        secure_insert::<thread_summaries::Entity>(row, &scope, runner).await?;
        return Ok(None);
    };
    let updated = thread_summaries::Entity::update_many()
        .col_expr(
            thread_summaries::Column::SummaryText,
            Expr::value(commit.summary_text.clone()),
        )
        .col_expr(
            thread_summaries::Column::SummarizedUpToCreatedAt,
            Expr::value(ts.stamp(commit.target.created_at)),
        )
        .col_expr(
            thread_summaries::Column::SummarizedUpToMessageId,
            Expr::value(commit.target.id),
        )
        .col_expr(
            thread_summaries::Column::TokenEstimate,
            Expr::value(commit.token_estimate),
        )
        .col_expr(
            thread_summaries::Column::UpdatedAt,
            Expr::value(ts.stamp(commit.at)),
        )
        .filter(
            Condition::all()
                .add(thread_summaries::Column::ChatId.eq(commit.chat_id))
                .add(
                    thread_summaries::Column::SummarizedUpToCreatedAt.eq(ts.stamp(base.created_at)),
                )
                .add(thread_summaries::Column::SummarizedUpToMessageId.eq(base.id)),
        )
        .secure()
        .scope_with(&scope)
        .exec(runner)
        .await?;
    if updated.rows_affected > 0 {
        return Ok(None);
    }
    let stored = thread_summary(runner, chat_scope, commit.chat_id).await?;
    Ok(Some(if stored.is_some() {
        ThreadSummaryCommitOutcome::CasConflict
    } else {
        ThreadSummaryCommitOutcome::BaseMissing
    }))
}

/// Marks exactly the live, uncompressed messages of `(base, target]` as
/// compressed; returns the number of rows.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_range_compressed(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    base: Option<MessageKey>,
    target: MessageKey,
) -> Result<u64, DomainError> {
    let result = messages::Entity::update_many()
        .col_expr(messages::Column::IsCompressed, Expr::value(true))
        .filter(
            active_in_chat(chat_id)
                .add(messages::Column::IsCompressed.eq(false))
                .add(range_condition(ts, base, target)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected)
}

/// The commit unit of work inside `tx` (module docs). Returns the outcome and
/// the outbox wake (empty unless committed).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query or enqueue failure (the caller's
/// transaction rolls back).
pub async fn commit_in_tx(
    tx: &(impl DBRunner + Sync),
    ts: TimestampForm,
    outbox: &OutboxEnqueuer,
    chat_scope: &AccessScope,
    commit: &ThreadSummaryCommit,
) -> Result<(ThreadSummaryCommitOutcome, Wake), DomainError> {
    if !lock_live_message(tx, chat_scope, commit.chat_id, commit.target.id).await? {
        return Ok((ThreadSummaryCommitOutcome::FrontierDeleted, Wake::empty()));
    }
    if let Some(lost) = upsert_summary(tx, ts, chat_scope, commit).await? {
        return Ok((lost, Wake::empty()));
    }
    mark_range_compressed(
        tx,
        ts,
        chat_scope,
        commit.chat_id,
        commit.base_frontier,
        commit.target,
    )
    .await?;
    let wake = outbox.enqueue_usage(tx, &commit.usage_event).await?;
    Ok((ThreadSummaryCommitOutcome::Committed, wake))
}

#[async_trait]
impl ThreadSummaryStore for DbRepository {
    async fn current_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRecord>, DomainError> {
        let conn = self.db().conn()?;
        thread_summary(&conn, scope, chat_id).await
    }

    async fn summary_range(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        base: Option<MessageKey>,
        target: MessageKey,
    ) -> Result<Vec<ContextMessageRecord>, DomainError> {
        let conn = self.db().conn()?;
        summary_range(&conn, self.timestamps(), scope, chat_id, base, target).await
    }

    async fn commit_summary(
        &self,
        scope: &AccessScope,
        commit: ThreadSummaryCommit,
    ) -> Result<ThreadSummaryCommitOutcome, DomainError> {
        let db = self.db().db();
        let ts = self.timestamps();
        let outbox = std::sync::Arc::clone(self.outbox());
        let scope = scope.clone();
        in_transaction(&db, move |tx| {
            Box::pin(async move { commit_in_tx(tx, ts, &outbox, &scope, &commit).await })
        })
        .await
    }
}

#[cfg(test)]
#[path = "summaries_tests.rs"]
mod tests;
