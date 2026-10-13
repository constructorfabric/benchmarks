//! Message reactions (DESIGN §3.3 "Message Reaction API").

use std::sync::Arc;

use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::chat::ChatService;
use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{MessageRole, Reaction};
use crate::infra::db::entities::{message, message_reaction};
use crate::infra::db::repos::{MessageRepo, ReactionRepo};

pub struct ReactionService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    chats: Arc<ChatService>,
}

impl ReactionService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<ChatAuthz>,
        chats: Arc<ChatService>,
    ) -> Self {
        Self { db, authz, chats }
    }

    /// Set (or replace) the caller's reaction on an assistant message. The value
    /// is validated before authorization.
    ///
    /// # Errors
    /// `InvalidReaction`, authorization errors, `ChatNotFound`, `MessageNotFound`,
    /// `ReactionTargetNotAssistant`, database failures.
    pub async fn set(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> DomainResult<message_reaction::Model> {
        let reaction = Reaction::parse(reaction).ok_or(DomainError::InvalidReaction)?;
        let target = self
            .reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id)
            .await?;
        ReactionRepo::upsert(
            &self.db.conn()?,
            target.tenant_id,
            ctx.subject_id(),
            target.id,
            reaction.as_str(),
            now_utc(),
        )
        .await
    }

    /// Remove the caller's reaction (idempotent for assistant messages).
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `MessageNotFound`,
    /// `ReactionTargetNotAssistant`, database failures.
    pub async fn remove(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> DomainResult<()> {
        let target = self
            .reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id)
            .await?;
        ReactionRepo::delete(
            &self.db.conn()?,
            target.tenant_id,
            ctx.subject_id(),
            target.id,
        )
        .await?;
        Ok(())
    }

    /// The assistant message `msg_id` of the caller's chat `chat_id`.
    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> DomainResult<message::Model> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let chat = self.chats.load_chat(&scope, chat_id).await?;
        let msg = MessageRepo::find_live(&self.db.conn()?, chat.tenant_id, chat.id, msg_id)
            .await?
            .ok_or(DomainError::MessageNotFound)?;
        if msg.role == MessageRole::Assistant.as_str() {
            Ok(msg)
        } else {
            Err(DomainError::ReactionTargetNotAssistant)
        }
    }
}
