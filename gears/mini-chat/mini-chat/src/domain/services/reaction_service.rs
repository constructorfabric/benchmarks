//! Message reactions (D "Message Reaction API").

use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz;
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::models::ReactionView;
use crate::domain::services::ChatService;
use crate::infra::db::entity::{chat, message_reaction};
use crate::infra::db::repos::{MessageRepo, ReactionRepo};

const LIKE: &str = "like";
const DISLIKE: &str = "dislike";

/// Set / remove the caller's reaction on an assistant message.
pub struct ReactionService {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    chats: Arc<ChatService>,
}

impl ReactionService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        clock: Arc<dyn Clock>,
        chats: Arc<ChatService>,
    ) -> Self {
        Self { db, clock, chats }
    }

    /// Upsert the caller's reaction on assistant message `msg_id`; an
    /// existing reaction is replaced (value and `created_at`).
    ///
    /// # Errors
    /// `InvalidReaction` (checked before authorization), `ChatNotFound`,
    /// `MessageNotFound`, `ReactionTargetNotAssistant`, authorization and
    /// database failures.
    pub async fn set(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        if reaction != LIKE && reaction != DISLIKE {
            return Err(DomainError::InvalidReaction);
        }
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::SET_REACTION, chat_id)
            .await?;
        self.assistant_message(&scope, &chat, msg_id).await?;

        let conn = self.db.conn()?;
        let user_id = ctx.subject_id();
        let now = self.clock.now();
        let row = match ReactionRepo
            .replace(&conn, &scope, user_id, msg_id, reaction, now)
            .await?
        {
            Some(row) => row,
            None => {
                self.insert(&scope, &chat, user_id, msg_id, reaction, now)
                    .await?
            }
        };
        Ok(ReactionView {
            message_id: row.message_id,
            reaction: row.reaction,
            created_at: row.created_at,
        })
    }

    /// Remove the caller's reaction on assistant message `msg_id`
    /// (idempotent: succeeds when there is none).
    ///
    /// # Errors
    /// `ChatNotFound`, `MessageNotFound`, `ReactionTargetNotAssistant`,
    /// authorization and database failures.
    pub async fn remove(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<(), DomainError> {
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::DELETE_REACTION, chat_id)
            .await?;
        self.assistant_message(&scope, &chat, msg_id).await?;
        let conn = self.db.conn()?;
        ReactionRepo
            .delete_for_message(&conn, &scope, ctx.subject_id(), msg_id)
            .await?;
        Ok(())
    }

    /// The message must be a non-deleted message of the chat, authored by
    /// the assistant.
    async fn assistant_message(
        &self,
        scope: &AccessScope,
        chat: &chat::Model,
        msg_id: Uuid,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let message = MessageRepo
            .find_by_id(&conn, scope, msg_id)
            .await?
            .filter(|m| m.chat_id == chat.id && m.deleted_at.is_none())
            .ok_or(DomainError::MessageNotFound)?;
        if message.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok(())
    }

    /// Insert the first reaction; a concurrent insert of the same
    /// `(message, user)` falls back to replacing it.
    async fn insert(
        &self,
        scope: &AccessScope,
        chat: &chat::Model,
        user_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
        now: OffsetDateTime,
    ) -> Result<message_reaction::Model, DomainError> {
        let conn = self.db.conn()?;
        let row = message_reaction::Model {
            id: Uuid::new_v4(),
            message_id: msg_id,
            user_id,
            tenant_id: chat.tenant_id,
            reaction: reaction.to_owned(),
            created_at: now,
        };
        match ReactionRepo.insert(&conn, scope, row).await {
            Ok(row) => Ok(row),
            Err(e) if e.is_unique_violation() => ReactionRepo
                .replace(&conn, scope, user_id, msg_id, reaction, now)
                .await?
                .ok_or_else(|| DomainError::Internal("reaction vanished after conflict".into())),
            Err(e) => Err(e.into()),
        }
    }
}
