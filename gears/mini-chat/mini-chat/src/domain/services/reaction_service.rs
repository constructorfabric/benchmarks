//! Message reactions (DESIGN section 3.3, Message Reaction API).

use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::enums::{MessageRole, ReactionKind};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, ChatAction};
use crate::domain::time::db_now;
use crate::infra::db::entities::message;
use crate::infra::db::repos::{chat_repo, message_repo, reaction_repo};

/// The caller's reaction on a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: ReactionKind,
    pub created_at: OffsetDateTime,
}

pub struct ReactionService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<dyn AuthzPort>,
}

impl ReactionService {
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, authz: Arc<dyn AuthzPort>) -> Self {
        Self { db, authz }
    }

    /// Sets (or replaces) the caller's reaction on an assistant message. The
    /// value is validated before the authorization check.
    ///
    /// # Errors
    /// `InvalidReaction`, authorization failure, `NotFound` (chat or message),
    /// `ReactionTargetNotAssistant`, database failure.
    pub async fn set(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let kind = ReactionKind::parse(reaction).ok_or(DomainError::InvalidReaction)?;
        let msg = self
            .target(ctx, ChatAction::SetReaction, chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        let row = reaction_repo::upsert(
            &conn,
            msg.tenant_id,
            msg.id,
            ctx.subject_id(),
            kind,
            db_now(),
        )
        .await?;
        Ok(ReactionView {
            message_id: row.message_id,
            reaction: kind,
            created_at: row.created_at,
        })
    }

    /// Removes the caller's reaction (idempotent for assistant messages).
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat or message),
    /// `ReactionTargetNotAssistant`, database failure.
    pub async fn remove(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<(), DomainError> {
        let msg = self
            .target(ctx, ChatAction::DeleteReaction, chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        reaction_repo::delete(&conn, msg.tenant_id, msg.id, ctx.subject_id()).await
    }

    /// Authorizes `action` on the chat and loads the live assistant message.
    async fn target(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<message::Model, DomainError> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = chat_repo::find_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        let msg = message_repo::find_live(&conn, chat.tenant_id, chat.id, msg_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Message,
            })?;
        if msg.role != MessageRole::Assistant.as_str() {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok(msg)
    }
}

#[cfg(test)]
#[path = "reaction_service_tests.rs"]
mod reaction_service_tests;
