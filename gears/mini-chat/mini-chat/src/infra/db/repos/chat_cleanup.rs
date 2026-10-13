//! The Secure ORM implementation of the [`ChatCleanupStore`] port
//! (`cpt-cf-mini-chat-feature-background-workers`, chat cleanup). Background
//! workers run without a request `SecurityContext`: every statement is bound to
//! the `tenant_id` and `chat_id` of the message through a tenant scope, and
//! every write is a single autocommit statement guarded by the cleanup status.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::attachments::model::{CleanupState, StoreRow};
use crate::domain::service::workers::ports::{
    ChatCleanupStore, CleanupChat, CleanupSummary, PendingAttachment,
};
use crate::infra::db::entities::{attachment, chat, chat_vector_store};
use crate::infra::db::mapping::scope_error;

/// Secure ORM store of the chat cleanup handler.
pub struct SeaChatCleanupStore {
    db: Arc<DBProvider<DomainError>>,
}

impl SeaChatCleanupStore {
    /// Creates the store over the database provider.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>) -> Self {
        Self { db }
    }
}

fn scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

fn of_chat(chat_id: Uuid) -> Condition {
    Condition::all().add(attachment::Column::ChatId.eq(chat_id))
}

async fn affected(
    update: sea_orm::UpdateMany<attachment::Entity>,
    tenant_id: Uuid,
    runner: &impl DBRunner,
) -> Result<u64, DomainError> {
    update
        .secure()
        .scope_with(&scope(tenant_id))
        .exec(runner)
        .await
        .map(|result| result.rows_affected)
        .map_err(|error| scope_error(&error))
}

#[async_trait]
impl ChatCleanupStore for SeaChatCleanupStore {
    async fn chat(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<CleanupChat>, DomainError> {
        let conn = self.db.conn()?;
        chat::Entity::find()
            .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await
            .map(|row| {
                row.map(|chat| CleanupChat {
                    deleted: chat.deleted_at.is_some(),
                    model: chat.model,
                })
            })
            .map_err(|error| scope_error(&error))
    }

    async fn claim_stragglers(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        let update = attachment::Entity::update_many()
            .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(of_chat(chat_id).add(attachment::Column::CleanupStatus.is_null()));
        affected(update, tenant_id, &conn).await
    }

    async fn pending_attachments(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<PendingAttachment>, DomainError> {
        let conn = self.db.conn()?;
        let rows = attachment::Entity::find()
            .filter(of_chat(chat_id).add(attachment::Column::CleanupStatus.eq("pending")))
            .order_by(attachment::Column::CreatedAt, Order::Asc)
            .order_by(attachment::Column::Id, Order::Asc)
            .secure()
            .scope_with(&scope(tenant_id))
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(rows
            .into_iter()
            .map(|row| PendingAttachment {
                id: row.id,
                provider_file_id: row.provider_file_id,
                storage_backend: row.storage_backend,
                secondary_uploaded: row.secondary_status == "uploaded",
                secondary_file_id: row.secondary_file_id,
            })
            .collect())
    }

    async fn record_attempt(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError> {
        let conn = self.db.conn()?;
        let update = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::CleanupAttempts,
                Expr::col(attachment::Column::CleanupAttempts).add(1),
            )
            .col_expr(
                attachment::Column::LastCleanupError,
                Expr::value(error.to_owned()),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                of_chat(chat_id)
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            );
        if affected(update, tenant_id, &conn).await? == 0 {
            return Ok(None);
        }
        attachment::Entity::find()
            .filter(of_chat(chat_id).add(attachment::Column::Id.eq(id)))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await
            .map(|row| row.map(|row| row.cleanup_attempts))
            .map_err(|error| scope_error(&error))
    }

    async fn finish(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        state: CleanupState,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let update = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(state.as_str()),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                of_chat(chat_id)
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            );
        Ok(affected(update, tenant_id, &conn).await? > 0)
    }

    async fn summary(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<CleanupSummary, DomainError> {
        let conn = self.db.conn()?;
        let rows = attachment::Entity::find()
            .filter(
                of_chat(chat_id)
                    .add(attachment::Column::CleanupStatus.is_in(["pending", "failed"])),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        let has = |status: &str| {
            rows.iter()
                .any(|row| row.cleanup_status.as_deref() == Some(status))
        };
        Ok(CleanupSummary {
            any_pending: has("pending"),
            any_failed: has("failed"),
        })
    }

    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoreRow>, DomainError> {
        let conn = self.db.conn()?;
        chat_vector_store::Entity::find()
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await
            .map(|row| row.map(StoreRow::from))
            .map_err(|error| scope_error(&error))
    }

    async fn delete_store_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        only_placeholder: bool,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let mut condition = Condition::all()
            .add(chat_vector_store::Column::Id.eq(row_id))
            .add(chat_vector_store::Column::ChatId.eq(chat_id));
        if only_placeholder {
            condition = condition.add(chat_vector_store::Column::VectorStoreId.is_null());
        }
        chat_vector_store::Entity::delete_many()
            .filter(condition)
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await
            .map(|result| result.rows_affected > 0)
            .map_err(|error| scope_error(&error))
    }
}
