//! Message history and reaction persistence: the Secure ORM implementation of
//! the [`MessageRepository`] port.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::QueryFilter;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{
    ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Chat, Message};
use crate::domain::ports::{AttachmentRow, MessageRepository, MessageRow, StoredReaction};
use crate::infra::db::entities::{attachment, message, message_attachment, message_reaction};
use crate::infra::db::mapping::scope_error;
use crate::infra::db::odata::{MessageFilterField, MessageMapper, paginate_scoped};
use crate::infra::db::repos::chat_access;

/// Secure ORM message store.
pub struct SeaMessageRepository {
    db: Arc<DBProvider<DomainError>>,
}

impl SeaMessageRepository {
    /// Creates the store over the database provider.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>) -> Self {
        Self { db }
    }
}

impl From<message::Model> for MessageRow {
    fn from(row: message::Model) -> Self {
        Self {
            id: row.id,
            request_id: row.request_id,
            role: row.role,
            content: row.content,
            created_at: row.created_at,
            model: row.model,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
        }
    }
}

impl From<attachment::Model> for AttachmentRow {
    fn from(row: attachment::Model) -> Self {
        Self {
            id: row.id,
            attachment_kind: row.attachment_kind,
            filename: row.filename,
            status: row.status,
            img_thumbnail: row.img_thumbnail,
            img_thumbnail_width: row.img_thumbnail_width,
            img_thumbnail_height: row.img_thumbnail_height,
        }
    }
}

#[async_trait]
impl MessageRepository for SeaMessageRepository {
    async fn load_chat(&self, scope: &AccessScope, chat_id: Uuid) -> Result<Chat, DomainError> {
        let conn = self.db.conn()?;
        chat_access::load_chat(&conn, scope, chat_id).await
    }

    async fn load_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(Chat, Message), DomainError> {
        let conn = self.db.conn()?;
        chat_access::load_message(&conn, scope, chat_id, message_id).await
    }

    async fn page(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageRow>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-page-messages
        // Child rows carry no owner column: the tenant part of the scope plus
        // the id of the owner-scoped chat loaded by the caller isolate them.
        let select = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only());
        paginate_scoped::<MessageFilterField, MessageMapper, _, _>(
            select,
            &conn,
            query,
            ("created_at", SortDir::Asc),
            MessageRow::from,
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-page-messages
    }

    async fn attachments_of(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentRow>>, DomainError> {
        let conn = self.db.conn()?;
        let tenant = scope.tenant_only();
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-attachments
        // Two batch queries per page (links, then their non-deleted
        // attachments), never one per message.
        let links = message_attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .secure()
            .scope_with(&tenant)
            .order_by(message_attachment::Column::CreatedAt, Order::Asc)
            .order_by(message_attachment::Column::AttachmentId, Order::Asc)
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        if links.is_empty() {
            return Ok(HashMap::new());
        }
        let attachments = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.is_in(links.iter().map(|link| link.attachment_id)))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&tenant)
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        let by_id: HashMap<Uuid, attachment::Model> =
            attachments.into_iter().map(|row| (row.id, row)).collect();
        let mut grouped: HashMap<Uuid, Vec<AttachmentRow>> = HashMap::new();
        for link in links {
            if let Some(row) = by_id.get(&link.attachment_id) {
                grouped
                    .entry(link.message_id)
                    .or_default()
                    .push(AttachmentRow::from(row.clone()));
            }
        }
        Ok(grouped)
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-attachments
    }

    async fn reactions_of(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, DomainError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.db.conn()?;
        let reaction_scope = scope.tenant_and_owner();
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-reactions
        // Reactions carry `tenant_id` and `user_id` but their `id` is not the
        // chat id: the chat and the messages were resolved through the full
        // scope by the caller, so the tenant and owner parts are enough here.
        let rows = message_reaction::Entity::find()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .secure()
            .scope_with(&reaction_scope)
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(rows
            .into_iter()
            .map(|row| (row.message_id, row.reaction))
            .collect())
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-enrich-messages:p1:inst-select-reactions
    }

    async fn upsert_reaction(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: &str,
        at: OffsetDateTime,
    ) -> Result<StoredReaction, DomainError> {
        let conn = self.db.conn()?;
        // The chat and the message were resolved through the full scope by the
        // caller; a reaction row is addressed by tenant and owner only (its
        // `id` is not the chat id the PDP may constrain).
        let reaction_scope = scope.tenant_and_owner();
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-upsert-reaction
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-none-to-like
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-none-to-dislike
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-like-to-dislike
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-dislike-to-like
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-repeat-same
        // One statement on UNIQUE `(message_id, user_id)`: a missing row is
        // inserted, an existing row keeps its `id` and gets the new
        // `reaction` and `created_at`.
        let active = message_reaction::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(message_id),
            user_id: Set(user_id),
            tenant_id: Set(tenant_id),
            reaction: Set(reaction.to_owned()),
            created_at: Set(at),
        };
        let on_conflict = SecureOnConflict::<message_reaction::Entity>::columns([
            message_reaction::Column::MessageId,
            message_reaction::Column::UserId,
        ])
        .update_columns([
            message_reaction::Column::Reaction,
            message_reaction::Column::CreatedAt,
        ])
        .map_err(|error| scope_error(&error))?;
        message_reaction::Entity::insert(active.clone())
            .secure()
            .scope_with_model(&reaction_scope, &active)
            .map_err(|error| match error {
                ScopeError::Denied(_) | ScopeError::TenantNotInScope { .. } => {
                    DomainError::AuthzDenied
                }
                other => scope_error(&other),
            })?
            .on_conflict(on_conflict)
            .exec(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-repeat-same
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-dislike-to-like
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-like-to-dislike
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-none-to-dislike
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-none-to-like
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-upsert-reaction

        let stored = message_reaction::Entity::find()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.eq(message_id)),
            )
            .secure()
            .scope_with(&reaction_scope)
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?
            .ok_or_else(|| DomainError::Internal("stored reaction not found".to_owned()))?;
        Ok(StoredReaction {
            message_id: stored.message_id,
            reaction: stored.reaction,
            created_at: stored.created_at,
        })
    }

    async fn delete_reaction(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        // Tenant and owner parts of the scope only: see `upsert_reaction`.
        let reaction_scope = scope.tenant_and_owner();
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-delete-reaction
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-to-none
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-none-stays-none
        // Another user's reaction is never matched (`user_id`); 0 affected
        // rows is success.
        message_reaction::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.eq(message_id)),
            )
            .secure()
            .scope_with(&reaction_scope)
            .exec(&conn)
            .await
            .map(|_| ())
            .map_err(|error| scope_error(&error))
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-none-stays-none
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-message-reaction:p1:inst-to-none
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-delete-reaction
    }
}
