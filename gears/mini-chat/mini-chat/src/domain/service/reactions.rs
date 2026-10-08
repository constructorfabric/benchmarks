//! Like / dislike reactions on assistant messages (DESIGN §3.3 Message Reaction API).

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{MiniChatService, now};
use crate::domain::authz::{ChatScopes, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entities::{message_reactions, messages};

/// Stored reaction.
#[derive(Debug, Clone)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}

/// `like` / `dislike` only.
///
/// # Errors
/// `INVALID_REACTION`.
pub fn validate_reaction(value: &str) -> DomainResult<&'static str> {
    match value {
        "like" => Ok("like"),
        "dislike" => Ok("dislike"),
        _ => Err(DomainError::invalid(Res::Message, "reaction", "INVALID_REACTION", "reaction must be 'like' or 'dislike'")),
    }
}

impl MiniChatService {
    async fn reaction_target(&self, ctx: &SecurityContext, action: &str, chat_id: Uuid, msg_id: Uuid) -> DomainResult<ChatScopes> {
        let scopes = self.scopes(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        self.load_chat(&conn, &scopes, chat_id).await?;
        let msg = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::Id.eq(msg_id))
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scopes.tenant)
            .one(&conn)
            .await?
            .ok_or(DomainError::NotFound(Res::Message))?;
        if msg.role != "assistant" {
            return Err(DomainError::precondition(
                Res::Message,
                "reaction_target",
                "STATE",
                "reactions are allowed on assistant messages only",
            ));
        }
        Ok(scopes)
    }

    /// `PUT .../reaction` (upsert).
    ///
    /// # Errors
    /// Validation, authorization, not found or precondition.
    pub async fn set_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid, reaction: &str) -> DomainResult<ReactionView> {
        let reaction = validate_reaction(reaction)?;
        let scopes = self.reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id).await?;
        let conn = self.db.conn()?;
        let existing = message_reactions::Entity::find()
            .filter(message_reactions::Column::MessageId.eq(msg_id))
            .secure()
            .scope_with(&scopes.owner)
            .one(&conn)
            .await?;
        let ts = now();
        if let Some(r) = existing {
            message_reactions::Entity::update_many()
                .col_expr(message_reactions::Column::Reaction, Expr::value(reaction))
                .col_expr(message_reactions::Column::CreatedAt, Expr::value(ts))
                .filter(message_reactions::Column::Id.eq(r.id))
                .secure()
                .scope_with(&scopes.owner)
                .exec(&conn)
                .await?;
        } else {
            let am = message_reactions::ActiveModel {
                id: Set(Uuid::new_v4()),
                message_id: Set(msg_id),
                user_id: Set(ctx.subject_id()),
                tenant_id: Set(ctx.subject_tenant_id()),
                reaction: Set(reaction.to_owned()),
                created_at: Set(ts),
            };
            secure_insert::<message_reactions::Entity>(am, &scopes.owner, &conn).await?;
        }
        Ok(ReactionView { message_id: msg_id, reaction: reaction.to_owned(), created_at: ts })
    }

    /// `DELETE .../reaction` (idempotent).
    ///
    /// # Errors
    /// Authorization, not found or precondition.
    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid) -> DomainResult<()> {
        let scopes = self.reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id).await?;
        let conn = self.db.conn()?;
        message_reactions::Entity::delete_many()
            .filter(message_reactions::Column::MessageId.eq(msg_id))
            .secure()
            .scope_with(&scopes.owner)
            .exec(&conn)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_values() {
        assert_eq!(validate_reaction("like").ok(), Some("like"));
        assert_eq!(validate_reaction("dislike").ok(), Some("dislike"));
        assert!(validate_reaction("love").is_err());
        assert!(validate_reaction("LIKE").is_err());
    }
}
