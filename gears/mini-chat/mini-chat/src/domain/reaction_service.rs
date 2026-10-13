//! Message reactions (DESIGN §3.3 "Message Reaction API").

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::service::{Svc, child_scope};
use crate::infra::db::entities::{message_reactions, messages};
use crate::infra::db::now;

/// Validates a reaction value.
///
/// # Errors
/// `InvalidReaction`.
pub fn validate_reaction(v: &str) -> Result<&'static str, DomainError> {
    match v {
        "like" => Ok("like"),
        "dislike" => Ok("dislike"),
        _ => Err(DomainError::InvalidReaction),
    }
}

impl Svc {
    async fn reaction_target(&self, ctx: &SecurityContext, action: &str, chat_id: Uuid, msg_id: Uuid) -> Result<messages::Model, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, action, chat_id).await?;
        let scope = child_scope(&chat);
        let conn = self.db.conn()?;
        let msg = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::Id.eq(msg_id))
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::MessageNotFound)?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTarget);
        }
        Ok(msg)
    }

    /// `PUT /chats/{id}/messages/{msg_id}/reaction`.
    ///
    /// # Errors
    /// 400/404/PDP.
    pub async fn set_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid, reaction: &str) -> Result<message_reactions::Model, DomainError> {
        let reaction = validate_reaction(reaction)?;
        let msg = self.reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id).await?;
        let scope = AccessScope::for_tenant(msg.tenant_id).ensure_owner(ctx.subject_id());
        let am = message_reactions::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(msg.id),
            user_id: Set(ctx.subject_id()),
            tenant_id: Set(msg.tenant_id),
            reaction: Set(reaction.to_owned()),
            created_at: Set(now()),
        };
        let conn = self.db.conn()?;
        message_reactions::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict_raw(
                OnConflict::columns([message_reactions::Column::MessageId, message_reactions::Column::UserId])
                    .update_columns([message_reactions::Column::Reaction, message_reactions::Column::CreatedAt])
                    .to_owned(),
            )
            .exec(&conn)
            .await?;
        message_reactions::Entity::find()
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg.id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("reaction not found after upsert"))
    }

    /// `DELETE /chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    /// 400/404/PDP.
    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid) -> Result<(), DomainError> {
        let msg = self.reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id).await?;
        let scope = AccessScope::for_tenant(msg.tenant_id).ensure_owner(ctx.subject_id());
        let conn = self.db.conn()?;
        message_reactions::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg.id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        Ok(())
    }
}
