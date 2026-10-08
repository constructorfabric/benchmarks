//! Like / dislike reactions on assistant messages.

use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entity::{message, message_reaction};
use crate::infra::repo::{self, now_utc};

#[derive(Debug, Clone)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}

/// # Errors
/// 400 `INVALID_REACTION`.
pub fn validate_reaction(r: &str) -> DomainResult<String> {
    match r {
        "like" | "dislike" => Ok(r.to_owned()),
        _ => Err(DomainError::invalid(
            Res::Message,
            "reaction",
            "INVALID_REACTION",
            "reaction must be 'like' or 'dislike'",
        )),
    }
}

impl AppState {
    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> DomainResult<(authz::ChatScopes, message::Model)> {
        let scopes = authz::chat_scopes(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let conn = self.conn()?;
        repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let msg = message::Entity::find()
            .secure()
            .scope_with(&scopes.tenant)
            .filter(
                Condition::all()
                    .add(message::Column::Id.eq(msg_id))
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Message, msg_id))?;
        if msg.role != "assistant" {
            return Err(DomainError::precondition(
                Res::Message,
                "reaction_target",
                "STATE",
                "reactions are allowed on assistant messages only",
            ));
        }
        Ok((scopes, msg))
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> DomainResult<ReactionView> {
        let reaction = validate_reaction(reaction)?;
        let (scopes, msg) = self
            .reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id)
            .await?;
        let now = now_utc();
        let am = message_reaction::ActiveModel {
            id: Set(Uuid::now_v7()),
            message_id: Set(msg.id),
            user_id: Set(ctx.subject_id()),
            tenant_id: Set(msg.tenant_id),
            reaction: Set(reaction.clone()),
            created_at: Set(now),
        };
        let conn = self.conn()?;
        let owner = authz::with_owner(&scopes.tenant, ctx);
        message_reaction::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&owner, &am)?
            .on_conflict_raw(
                OnConflict::columns([
                    message_reaction::Column::MessageId,
                    message_reaction::Column::UserId,
                ])
                .update_columns([
                    message_reaction::Column::Reaction,
                    message_reaction::Column::CreatedAt,
                ])
                .to_owned(),
            )
            .exec(&conn)
            .await?;
        let _ = Expr::value(1);
        Ok(ReactionView {
            message_id: msg.id,
            reaction,
            created_at: now,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> DomainResult<()> {
        let (scopes, msg) = self
            .reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id)
            .await?;
        let conn = self.conn()?;
        let owner = authz::with_owner(&scopes.tenant, ctx);
        message_reaction::Entity::delete_many()
            .secure()
            .scope_with(&owner)
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(msg.id))
                    .add(message_reaction::Column::UserId.eq(ctx.subject_id())),
            )
            .exec(&conn)
            .await?;
        Ok(())
    }
}
