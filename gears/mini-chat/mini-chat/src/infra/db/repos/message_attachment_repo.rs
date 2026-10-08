//! `message_attachment` repository (chat child: tenant-only scope).

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::message_attachment;

/// Repository for `message_attachments` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageAttachmentRepo;

impl MessageAttachmentRepo {
    /// Insert a link row (`chat_id` must be the parent message's chat).
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error (PK / FK violations
    /// included).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: message_attachment::Model,
    ) -> Result<message_attachment::Model, ScopeError> {
        insert_model::<message_attachment::Entity>(runner, &scope.tenant_only(), row).await
    }

    /// Links of one message of a chat, oldest first.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_for_message(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Vec<message_attachment::Model>, ScopeError> {
        message_attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.eq(message_id)),
            )
            .order_by(message_attachment::Column::CreatedAt, Order::Asc)
            .all(runner)
            .await
    }

    /// Join rows of the given messages of the chat.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_for_messages(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<Vec<message_attachment::Model>, ScopeError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        message_attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .order_by(message_attachment::Column::CreatedAt, Order::Asc)
            .all(runner)
            .await
    }

    /// Whether any message of the chat references the attachment.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn is_referenced(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<bool, ScopeError> {
        let n = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::AttachmentId.eq(attachment_id)),
            )
            .count(runner)
            .await?;
        Ok(n > 0)
    }
}
