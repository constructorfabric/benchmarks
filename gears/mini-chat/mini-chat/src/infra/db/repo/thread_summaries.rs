//! `ThreadSummaryRepository` over the Secure ORM (tenant scope derived from
//! the owner-scoped chat lookup, or from the tenant of a system task).

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::messages::{after_frontier, to_row, up_to_frontier};
use crate::domain::error::DomainError;
use crate::domain::messages::{MessageRow, SummaryFrontier};
use crate::domain::ports::ThreadSummaryRepository;
use crate::domain::thread_summary::{NewSummary, SummaryRow};
use crate::infra::db::entity::{messages, thread_summaries};
use crate::infra::db::timestamp::{from_db, to_db};

/// Ids per `UPDATE ... WHERE id IN (...)`, below the bind-variable limit of
/// every supported engine.
const FLAG_BATCH: usize = 400;

fn to_summary_row(model: thread_summaries::Model) -> SummaryRow {
    SummaryRow {
        id: model.id,
        text: model.summary_text,
        frontier: SummaryFrontier {
            created_at: from_db(model.summarized_up_to_created_at),
            message_id: model.summarized_up_to_message_id,
        },
        token_estimate: model.token_estimate,
    }
}

#[async_trait]
impl<R: DBRunner> ThreadSummaryRepository for R {
    async fn find_summary_frontier(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<SummaryFrontier>, DomainError> {
        Ok(self
            .find_summary(scope, chat_id)
            .await?
            .map(|row| row.frontier))
    }

    async fn find_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<SummaryRow>, DomainError> {
        let model = thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_summary_row))
    }

    async fn delete_summary(&self, scope: &AccessScope, chat_id: Uuid) -> Result<u64, DomainError> {
        let deleted = thread_summaries::Entity::delete_many()
            .filter(thread_summaries::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(deleted.rows_affected)
    }

    async fn latest_message_frontier(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        excluded_request: Uuid,
    ) -> Result<Option<SummaryFrontier>, DomainError> {
        let model = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .filter(
                Condition::any()
                    .add(messages::Column::RequestId.is_null())
                    .add(messages::Column::RequestId.ne(excluded_request)),
            )
            .secure()
            .scope_with(scope)
            .order_by(messages::Column::CreatedAt, Order::Desc)
            .order_by(messages::Column::Id, Order::Desc)
            .one(self)
            .await?;
        Ok(model.map(|row| SummaryFrontier {
            created_at: from_db(row.created_at),
            message_id: row.id,
        }))
    }

    async fn summary_range(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        base: Option<SummaryFrontier>,
        target: SummaryFrontier,
    ) -> Result<Vec<MessageRow>, DomainError> {
        let mut select = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::DeletedAt.is_null())
            .filter(messages::Column::IsCompressed.eq(false))
            .filter(messages::Column::Role.is_in(["user", "assistant"]))
            .filter(up_to_frontier(&target));
        if let Some(base) = &base {
            select = select.filter(after_frontier(base));
        }
        let rows = select
            .secure()
            .scope_with(scope)
            .order_by(messages::Column::CreatedAt, Order::Asc)
            .order_by(messages::Column::Id, Order::Asc)
            .all(self)
            .await?;
        Ok(rows.into_iter().map(to_row).collect())
    }

    async fn replace_summary(
        &self,
        scope: &AccessScope,
        summary: NewSummary,
    ) -> Result<(), DomainError> {
        self.delete_summary(scope, summary.chat_id).await?;
        secure_insert::<thread_summaries::Entity>(
            thread_summaries::ActiveModel {
                id: Set(summary.id),
                tenant_id: Set(summary.tenant_id),
                chat_id: Set(summary.chat_id),
                summary_text: Set(Some(summary.text)),
                summarized_up_to_created_at: Set(to_db(summary.frontier.created_at)),
                summarized_up_to_message_id: Set(summary.frontier.message_id),
                token_estimate: Set(Some(summary.token_estimate)),
                created_at: Set(to_db(summary.now)),
                updated_at: Set(to_db(summary.now)),
            },
            scope,
            self,
        )
        .await?;
        Ok(())
    }

    async fn lock_live_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let locked = messages::Entity::update_many()
            .col_expr(
                messages::Column::IsCompressed,
                Expr::col(messages::Column::IsCompressed),
            )
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::Id.eq(id))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(locked.rows_affected > 0)
    }

    async fn mark_compressed(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<u64, DomainError> {
        let mut changed = 0;
        for batch in ids.chunks(FLAG_BATCH) {
            let updated = messages::Entity::update_many()
                .col_expr(messages::Column::IsCompressed, Expr::value(true))
                .filter(messages::Column::ChatId.eq(chat_id))
                .filter(messages::Column::Id.is_in(batch.iter().copied()))
                .secure()
                .scope_with(scope)
                .exec(self)
                .await?;
            changed += updated.rows_affected;
        }
        Ok(changed)
    }
}
