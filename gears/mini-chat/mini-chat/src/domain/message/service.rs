//! The messages service: list, set and remove a reaction.

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::model::{MessageRole, MessageView, Reaction, ReactionView};
use crate::domain::authz::{Pep, actions};
use crate::domain::chat::ChatRepository;
use crate::domain::clock;
use crate::domain::error::DomainError;

/// Persistence of messages and reactions. Every method applies the caller's scope.
#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// The page of messages of the chat with attachments and the caller's reactions.
    async fn list(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError>;

    /// The role of a non-deleted message of the chat, `None` when there is no such message.
    async fn role_of(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRole>, DomainError>;

    /// Sets the caller's reaction, replacing an existing one.
    async fn upsert_reaction(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: Reaction,
        now: OffsetDateTime,
    ) -> Result<ReactionView, DomainError>;

    /// Removes the caller's reaction; idempotent.
    async fn delete_reaction(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError>;
}

/// Message list and reactions of the caller's chats.
#[domain_model]
pub struct MessageService {
    pep: Pep,
    chats: Arc<dyn ChatRepository>,
    messages: Arc<dyn MessageRepository>,
}

impl MessageService {
    /// Creates the service.
    #[must_use]
    pub fn new(
        pep: Pep,
        chats: Arc<dyn ChatRepository>,
        messages: Arc<dyn MessageRepository>,
    ) -> Self {
        Self {
            pep,
            chats,
            messages,
        }
    }

    /// Lists the messages of one of the caller's chats.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, an `OData` query error or an internal
    /// error.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::LIST_MESSAGES, Some(chat_id))
            .await?;
        self.require_chat(&scope, chat_id).await?;
        self.messages
            .list(&scope, chat_id, ctx.subject_id(), query)
            .await
    }

    /// Sets the caller's reaction on an assistant message. The value is validated before the
    /// authorization check.
    ///
    /// # Errors
    /// `InvalidReaction`, `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `MessageNotFound`,
    /// `InvalidReactionTarget` or an internal error.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let reaction = Reaction::parse(reaction).ok_or(DomainError::InvalidReaction)?;
        let scope = self
            .pep
            .chat_scope(ctx, actions::SET_REACTION, Some(chat_id))
            .await?;
        self.require_assistant_message(&scope, chat_id, message_id)
            .await?;
        self.messages
            .upsert_reaction(
                &scope,
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                message_id,
                reaction,
                clock::now(),
            )
            .await
    }

    /// Removes the caller's reaction from an assistant message (idempotent).
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `MessageNotFound`,
    /// `InvalidReactionTarget` or an internal error.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::DELETE_REACTION, Some(chat_id))
            .await?;
        self.require_assistant_message(&scope, chat_id, message_id)
            .await?;
        self.messages
            .delete_reaction(&scope, ctx.subject_id(), message_id)
            .await
    }

    async fn require_chat(&self, scope: &AccessScope, chat_id: Uuid) -> Result<(), DomainError> {
        self.chats
            .find(scope, chat_id)
            .await?
            .map(|_| ())
            .ok_or(DomainError::ChatNotFound { chat_id })
    }

    async fn require_assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        self.require_chat(scope, chat_id).await?;
        match self.messages.role_of(scope, chat_id, message_id).await? {
            None => Err(DomainError::MessageNotFound { message_id }),
            Some(MessageRole::Assistant) => Ok(()),
            Some(_) => Err(DomainError::InvalidReactionTarget),
        }
    }
}
