//! Message reactions (DESIGN "Message Reaction API"): one like / dislike per user and assistant
//! message.

use std::sync::Arc;

use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Authz, ChatAction};
use crate::domain::error::DomainError;
use crate::infra::db::MessageRole;
use crate::infra::db::entity::message_reactions;
use crate::infra::db::repo;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx;

/// A reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    Like,
    Dislike,
}

impl Reaction {
    /// Value stored in `message_reactions.reaction`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }

    /// Parses the stored / requested value (`like` or `dislike`, case-sensitive).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "like" => Some(Self::Like),
            "dislike" => Some(Self::Dislike),
            _ => None,
        }
    }
}

/// A stored reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: Reaction,
    pub created_at: OffsetDateTime,
}

/// Sets and removes the caller's reaction on assistant messages.
pub struct ReactionService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<Authz>,
}

impl ReactionService {
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, authz: Arc<Authz>) -> Self {
        Self { db, authz }
    }

    /// Sets (or replaces) the caller's reaction on assistant message `message_id` of `chat_id`.
    /// The value is validated before the PDP is asked.
    ///
    /// # Errors
    /// `InvalidReaction`, `AccessDenied` / `AuthzUnavailable`, `ChatNotFound`,
    /// `MessageNotFound` (unknown, deleted or in another chat), `ReactionTargetNotAssistant`,
    /// database failures.
    pub async fn set(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let reaction = Reaction::parse(reaction).ok_or(DomainError::InvalidReaction)?;
        let (scope, tenant_id) = self
            .assistant_message_scope(ctx, ChatAction::SetReaction, chat_id, message_id)
            .await?;
        let row = message_reactions::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            message_id: Set(message_id),
            user_id: Set(ctx.subject_id()),
            reaction: Set(reaction.as_str().to_owned()),
            created_at: Set(db_now()),
        };
        // Upsert, then read the stored row back in the same transaction: a replaced reaction
        // keeps its original `created_at`, so the response is built from what is stored.
        let stored = write_tx(&self.db, move |tx| {
            let (scope, row) = (scope.clone(), row.clone());
            Box::pin(async move {
                repo::reactions::upsert(tx, &scope, row).await?;
                repo::reactions::find(tx, &scope, message_id)
                    .await?
                    .ok_or_else(|| {
                        DomainError::Internal("reaction missing after upsert".to_owned())
                    })
            })
        })
        .await?;
        let reaction = Reaction::parse(&stored.reaction).ok_or_else(|| {
            DomainError::Internal(format!(
                "stored reaction `{}` is not valid",
                stored.reaction
            ))
        })?;
        Ok(ReactionView {
            message_id,
            reaction,
            created_at: stored.created_at,
        })
    }

    /// Removes the caller's reaction on assistant message `message_id` (idempotent).
    ///
    /// # Errors
    /// `AccessDenied` / `AuthzUnavailable`, `ChatNotFound`, `MessageNotFound`,
    /// `ReactionTargetNotAssistant`, database failures.
    pub async fn remove(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let (scope, _) = self
            .assistant_message_scope(ctx, ChatAction::DeleteReaction, chat_id, message_id)
            .await?;
        write_tx(&self.db, move |tx| {
            let scope = scope.clone();
            Box::pin(async move {
                repo::reactions::delete(tx, &scope, message_id)
                    .await
                    .map(drop)
            })
        })
        .await
    }

    /// Authorizes `action` on the chat and checks that `message_id` is a live assistant message
    /// of it; returns the PDP scope (tenant + owner) and the chat's tenant.
    async fn assistant_message_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(toolkit_db::secure::AccessScope, Uuid), DomainError> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = repo::chats::load_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::ChatNotFound {
                id: chat_id.to_string(),
            })?;
        let message = repo::messages::find(&conn, &scope.tenant_only(), chat_id, message_id)
            .await?
            .ok_or_else(|| DomainError::MessageNotFound {
                id: message_id.to_string(),
            })?;
        if message.role != MessageRole::Assistant.as_str() {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok((scope, chat.tenant_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_like_and_dislike_parse() {
        assert_eq!(Reaction::parse("like"), Some(Reaction::Like));
        assert_eq!(Reaction::parse("dislike"), Some(Reaction::Dislike));
        for bad in ["", "Like", "love", "like ", "LIKE"] {
            assert_eq!(Reaction::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Reaction::Like.as_str(), "like");
        assert_eq!(Reaction::Dislike.as_str(), "dislike");
    }
}
