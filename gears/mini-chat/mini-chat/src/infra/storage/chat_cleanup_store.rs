//! The `SQLite` and `PostgreSQL` implementation of the chat-cleanup persistence port: the chat and
//! the `pending` attachment rows, the guarded cleanup writes and the `chat_vector_stores` row
//! (`cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler`,
//! `-chat-attachment-file` and `-vector-store-cleanup`).

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::attachments::cleanup::AttemptRecord;
use crate::domain::error::DomainError;
use crate::domain::maintenance::chat_store::{
    ChatCleanupStore, ChatFacts, PendingAttachment, VectorStoreRow,
};
use crate::infra::storage::entity::{attachment, chat, chat_vector_store};
use crate::infra::storage::keyset_page::time_key;
use crate::infra::storage::stream_store::time_storage;

fn scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

fn pending_of_chat(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(attachment::Column::ChatId.eq(chat_id))
        .add(attachment::Column::CleanupStatus.eq("pending"))
}

#[async_trait]
impl ChatCleanupStore for DBProvider<DomainError> {
    async fn load_chat(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<ChatFacts>, DomainError> {
        let conn = self.conn()?;
        // the deleted filter is not applied: the handler needs the soft-deleted chat
        let row = chat::Entity::find()
            .filter(chat::Column::Id.eq(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        Ok(row.map(|row| ChatFacts {
            model: row.model,
            deleted: row.deleted_at.is_some(),
        }))
    }

    async fn pending_attachments(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<PendingAttachment>, DomainError> {
        let conn = self.conn()?;
        let storage = time_storage(self);
        let rows = attachment::Entity::find()
            .filter(pending_of_chat(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .order_by(
                time_key(storage, Expr::col(attachment::Column::CreatedAt)),
                Order::Asc,
            )
            .order_by(attachment::Column::Id, Order::Asc)
            .all(&conn)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| PendingAttachment {
                id: row.id,
                provider_file_id: row.provider_file_id,
                storage_backend: row.storage_backend,
                cleanup_attempts: row.cleanup_attempts,
                secondary_file_id: row.secondary_file_id,
                secondary_status: row.secondary_status,
                secondary_provider_kind: row.secondary_provider_kind,
            })
            .collect())
    }

    async fn count_pending(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<u64, DomainError> {
        let conn = self.conn()?;
        let count = attachment::Entity::find()
            .filter(pending_of_chat(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .count(&conn)
            .await?;
        Ok(count)
    }

    async fn has_failed(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let row = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::CleanupStatus.eq("failed")),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        Ok(row.is_some())
    }

    async fn mark_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let affected = attachment::Entity::update_many()
            .col_expr(attachment::Column::CleanupStatus, Expr::value("done"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }

    async fn record_failed_attempt(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError> {
        let cause = cause.to_owned();
        self.transaction(move |tx| {
            let cause = cause.clone();
            Box::pin(async move {
                let guard = || {
                    Condition::all()
                        .add(attachment::Column::Id.eq(attachment_id))
                        .add(attachment::Column::CleanupStatus.eq("pending"))
                };
                let affected = attachment::Entity::update_many()
                    .col_expr(
                        attachment::Column::CleanupAttempts,
                        Expr::col(attachment::Column::CleanupAttempts).add(1),
                    )
                    // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-file-debt-query
                    // an admin queries the `failed` rows of deleted chats and their
                    // `last_cleanup_error`, a fixed text without file content or provider text
                    .col_expr(attachment::Column::LastCleanupError, Expr::value(cause))
                    // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-file-debt-query
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
                    .filter(guard())
                    .secure()
                    .scope_with(&scope(tenant_id))
                    .exec(tx)
                    .await?
                    .rows_affected;
                if affected == 0 {
                    return Ok(None);
                }
                let row = attachment::Entity::find()
                    .filter(attachment::Column::Id.eq(attachment_id))
                    .secure()
                    .scope_with(&scope(tenant_id))
                    .one(tx)
                    .await?
                    .ok_or_else(|| DomainError::internal("the cleanup row vanished"))?;
                let exhausted = u32::try_from(row.cleanup_attempts).unwrap_or(0) >= max_attempts;
                if exhausted {
                    attachment::Entity::update_many()
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("failed"))
                        .filter(guard())
                        .secure()
                        .scope_with(&scope(tenant_id))
                        .exec(tx)
                        .await?;
                }
                Ok(Some(AttemptRecord {
                    attempts: row.cleanup_attempts,
                    exhausted,
                }))
            })
        })
        .await
    }

    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        let conn = self.conn()?;
        let row = chat_vector_store::Entity::find()
            .filter(chat_vector_store::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        Ok(row.map(|row| VectorStoreRow {
            id: row.id,
            vector_store_id: row.vector_store_id,
            provider: row.provider,
        }))
    }

    async fn delete_vector_store_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        placeholder_only: bool,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let mut filter = Condition::all()
            .add(chat_vector_store::Column::Id.eq(id))
            .add(chat_vector_store::Column::ChatId.eq(chat_id));
        if placeholder_only {
            filter = filter.add(chat_vector_store::Column::VectorStoreId.is_null());
        }
        let affected = chat_vector_store::Entity::delete_many()
            .filter(filter)
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }
}

#[cfg(test)]
#[path = "chat_cleanup_repo_tests.rs"]
mod chat_cleanup_repo_tests;

#[cfg(test)]
#[path = "chat_cleanup_concurrency_tests.rs"]
mod chat_cleanup_concurrency_tests;
