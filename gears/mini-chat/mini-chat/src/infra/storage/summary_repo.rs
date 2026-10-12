//! `SeaORM` implementation of the thread-summary repository over Secure ORM (DESIGN section 3.6
//! "Thread Summary Update"). Background work runs outside any user scope: every query carries the
//! tenant of the work item.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::DBProvider;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::entity::{messages, thread_summaries};
use super::map_scope_err;
use super::ordering::{after, at_or_before};
use crate::domain::error::DomainError;
use crate::domain::llm::InputRole;
use crate::domain::summary::{
    CommitOutcome, CommitSummary, Frontier, RangeMessage, StoredSummary, SummaryRepository,
};
use crate::infra::outbox::OutboxEnqueuer;

/// Summary repository over the gear's database.
pub struct SummaryRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SummaryRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }
}

/// The messages a summary covers: `(created_at, id)` in `(base, target]`.
fn range_condition(base: Option<Frontier>, target: Frontier) -> Condition {
    let condition = Condition::all().add(at_or_before(target.created_at, target.message_id));
    match base {
        Some(base) => condition.add(after(base.created_at, base.message_id)),
        None => condition,
    }
}

fn to_frontier(row: &thread_summaries::Model) -> Frontier {
    Frontier {
        created_at: row.summarized_up_to_created_at,
        message_id: row.summarized_up_to_message_id,
    }
}

#[async_trait]
impl SummaryRepository for SummaryRepositoryImpl {
    async fn current(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoredSummary>, DomainError> {
        let conn = self.db.conn()?;
        Ok(thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(|row| StoredSummary {
                text: row.summary_text.clone(),
                frontier: to_frontier(&row),
            }))
    }

    async fn message_alive(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let alive = messages::Entity::find()
            .filter(messages::Column::Id.eq(message_id))
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .count(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(alive > 0)
    }

    async fn load_range(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<Frontier>,
        target: Frontier,
    ) -> Result<Vec<RangeMessage>, DomainError> {
        let conn = self.db.conn()?;
        Ok(messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .filter(messages::Column::IsCompressed.eq(false))
            .filter(messages::Column::Role.is_in(["user", "assistant"]))
            .filter(range_condition(base, target))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Asc)
            .order_by(messages::Column::Id, sea_orm::Order::Asc)
            .all(&conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .map(|row| RangeMessage {
                role: if row.role == "assistant" {
                    InputRole::Assistant
                } else {
                    InputRole::User
                },
                content: row.content,
            })
            .collect())
    }

    async fn commit(
        &self,
        tenant_id: Uuid,
        command: CommitSummary,
    ) -> Result<CommitOutcome, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let result = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let scope = AccessScope::for_tenant(tenant_id);
                // Lock and check the target: a no-op update takes the row lock where the database
                // has one, and affects nothing once a retry, edit or delete soft-deleted the row.
                let locked = messages::Entity::update_many()
                    .col_expr(
                        messages::Column::IsCompressed,
                        Expr::col(messages::Column::IsCompressed),
                    )
                    .filter(messages::Column::Id.eq(command.target.message_id))
                    .filter(messages::Column::ChatId.eq(command.chat_id))
                    .filter(messages::Column::DeletedAt.is_null())
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if locked == 0 {
                    return Ok::<_, DomainError>((
                        (CommitOutcome::FrontierDeleted, None),
                        Wake::empty(),
                    ));
                }
                let stored = thread_summaries::Entity::find()
                    .filter(thread_summaries::Column::ChatId.eq(command.chat_id))
                    .secure()
                    .scope_with(&scope)
                    .one(tx)
                    .await
                    .map_err(map_scope_err)?;
                match (command.base, stored) {
                    (None, None) => {
                        let row = thread_summaries::ActiveModel {
                            id: ActiveValue::Set(Uuid::new_v4()),
                            tenant_id: ActiveValue::Set(tenant_id),
                            chat_id: ActiveValue::Set(command.chat_id),
                            summary_text: ActiveValue::Set(command.summary_text.clone()),
                            summarized_up_to_created_at: ActiveValue::Set(
                                command.target.created_at,
                            ),
                            summarized_up_to_message_id: ActiveValue::Set(
                                command.target.message_id,
                            ),
                            token_estimate: ActiveValue::Set(command.token_estimate),
                            created_at: ActiveValue::Set(command.now),
                            updated_at: ActiveValue::Set(command.now),
                        };
                        secure_insert::<thread_summaries::Entity>(row, &scope, tx)
                            .await
                            .map_err(map_scope_err)?;
                    }
                    (None, Some(_)) => {
                        return Ok(((CommitOutcome::CasConflict, None), Wake::empty()));
                    }
                    (Some(_), None) => {
                        return Ok(((CommitOutcome::BaseMissing, None), Wake::empty()));
                    }
                    (Some(base), Some(row)) => {
                        if to_frontier(&row) != base {
                            return Ok(((CommitOutcome::CasConflict, None), Wake::empty()));
                        }
                        let advanced = thread_summaries::Entity::update_many()
                            .col_expr(
                                thread_summaries::Column::SummaryText,
                                Expr::value(command.summary_text.clone()),
                            )
                            .col_expr(
                                thread_summaries::Column::SummarizedUpToCreatedAt,
                                Expr::value(command.target.created_at),
                            )
                            .col_expr(
                                thread_summaries::Column::SummarizedUpToMessageId,
                                Expr::value(command.target.message_id),
                            )
                            .col_expr(
                                thread_summaries::Column::TokenEstimate,
                                Expr::value(command.token_estimate),
                            )
                            .col_expr(
                                thread_summaries::Column::UpdatedAt,
                                Expr::value(command.now),
                            )
                            .filter(thread_summaries::Column::Id.eq(row.id))
                            .filter(
                                thread_summaries::Column::SummarizedUpToCreatedAt
                                    .eq(base.created_at),
                            )
                            .filter(
                                thread_summaries::Column::SummarizedUpToMessageId
                                    .eq(base.message_id),
                            )
                            .secure()
                            .scope_with(&scope)
                            .exec(tx)
                            .await
                            .map_err(map_scope_err)?
                            .rows_affected;
                        if advanced == 0 {
                            return Ok(((CommitOutcome::CasConflict, None), Wake::empty()));
                        }
                    }
                }
                messages::Entity::update_many()
                    .col_expr(messages::Column::IsCompressed, Expr::value(true))
                    .filter(messages::Column::ChatId.eq(command.chat_id))
                    .filter(messages::Column::DeletedAt.is_null())
                    .filter(messages::Column::IsCompressed.eq(false))
                    .filter(messages::Column::Role.is_in(["user", "assistant"]))
                    .filter(range_condition(command.base, command.target))
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?;
                // Enqueue last: an error after the enqueue would drop the wake on rollback.
                let (wake, message) = outbox.enqueue_usage(tx, &command.usage_event).await?;
                Ok(((CommitOutcome::Committed, Some(message)), wake))
            })
        })
        .await;
        match result {
            Ok((outcome, announced)) => {
                if let Some(message) = announced {
                    self.outbox.announce(&[message]);
                }
                Ok(outcome)
            }
            // A concurrent first summary won the unique index on `chat_id`.
            Err(DomainError::UniqueViolation) => Ok(CommitOutcome::CasConflict),
            Err(err) => Err(err),
        }
    }
}
