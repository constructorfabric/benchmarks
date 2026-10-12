//! Reactions service (DESIGN 3.3 "Message Reaction API").
//!
//! Order of checks: reaction value (before the PDP and any lookup), PDP
//! (`set_reaction` / `delete_reaction` on the chat), owner-scoped chat lookup,
//! tenant-scoped message lookup inside that chat, assistant-only rule. The
//! reaction row always belongs to the caller (`tenant_id`, `user_id` come from
//! the security context).

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::chat_not_found;
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::messages::{MessageRole, message_not_found};
use crate::domain::ports::{ChatRepository, MessageRepository, ReactionRepository};

/// Reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    /// Positive.
    Like,
    /// Negative.
    Dislike,
}

impl ReactionKind {
    /// Parses a caller supplied value (case-sensitive).
    ///
    /// # Errors
    /// `InvalidReaction` for anything but `like` / `dislike`.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "like" => Ok(Self::Like),
            "dislike" => Ok(Self::Dislike),
            _ => Err(DomainError::InvalidReaction),
        }
    }

    /// Parses a stored value.
    ///
    /// # Errors
    /// `Internal` for a value outside the schema contract.
    pub fn parse_stored(raw: &str) -> Result<Self, DomainError> {
        Self::parse(raw).map_err(|_| DomainError::internal(format!("unknown reaction `{raw}`")))
    }

    /// The stored / wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }
}

/// A reaction to insert or replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReaction {
    /// Tenant of the caller.
    pub tenant_id: Uuid,
    /// The caller.
    pub user_id: Uuid,
    /// Reacted message.
    pub message_id: Uuid,
    /// Reaction value (`like` / `dislike`).
    pub reaction: String,
    /// Time of the (re)action.
    pub now: OffsetDateTime,
}

/// A stored reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionRow {
    /// Reacted message.
    pub message_id: Uuid,
    /// Stored value.
    pub reaction: String,
    /// Time of the (last) reaction.
    pub created_at: OffsetDateTime,
}

/// A reaction as reported by the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    /// Reacted message.
    pub message_id: Uuid,
    /// Value.
    pub reaction: ReactionKind,
    /// Time of the (last) reaction.
    pub created_at: OffsetDateTime,
}

/// Sets and removes the caller's reaction on assistant messages.
pub struct ReactionsService {
    db: Arc<DBProvider<DomainError>>,
    enforcer: PolicyEnforcer,
}

impl ReactionsService {
    /// Creates the service.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, enforcer: PolicyEnforcer) -> Self {
        Self { db, enforcer }
    }

    /// Sets (or replaces) the caller's reaction on an assistant message.
    ///
    /// # Errors
    /// `InvalidReaction`, PDP errors, `NotFound` (chat or message),
    /// `InvalidReactionTarget` for a user / system message, `Internal`.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let kind = ReactionKind::parse(reaction)?;
        let scope = self
            .authorize_target(ctx, actions::SET_REACTION, chat_id, message_id)
            .await?;
        let conn = self.db.conn()?;
        let row = conn
            .upsert_reaction(
                &scope,
                NewReaction {
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    message_id,
                    reaction: kind.as_str().to_owned(),
                    now: now_utc(),
                },
            )
            .await?;
        Ok(ReactionView {
            message_id: row.message_id,
            reaction: ReactionKind::parse_stored(&row.reaction)?,
            created_at: row.created_at,
        })
    }

    /// Removes the caller's reaction; succeeds whether or not one existed.
    ///
    /// # Errors
    /// PDP errors, `NotFound` (chat or message), `InvalidReactionTarget`,
    /// `Internal`.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .authorize_target(ctx, actions::DELETE_REACTION, chat_id, message_id)
            .await?;
        let conn = self.db.conn()?;
        conn.delete_reaction(&scope, ctx.subject_id(), message_id)
            .await
    }

    /// PDP + chat lookup + message lookup + assistant-only rule; returns the
    /// scope of the reaction table (tenant + the caller as owner).
    async fn authorize_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<AccessScope, DomainError> {
        let chat_scope = chat_scope(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        conn.find_live_chat(&chat_scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;

        let tenant_scope = chat_scope.tenant_only();
        let message = conn
            .find_live_message(&tenant_scope, chat_id, message_id)
            .await?
            .ok_or_else(|| message_not_found(message_id))?;
        if MessageRole::parse(&message.role)? != MessageRole::Assistant {
            return Err(DomainError::InvalidReactionTarget);
        }
        Ok(tenant_scope.ensure_owner(ctx.subject_id()))
    }
}
