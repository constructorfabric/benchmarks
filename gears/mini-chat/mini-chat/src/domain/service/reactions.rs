//! Message reactions (`PUT` / `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`).

use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::chats::tenant_scope;
use super::{Core, now};
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, Resource};
use crate::infra::db::entities::{message, reaction};

/// Stored reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}

/// Validates the reaction value (400 `INVALID_REACTION`).
///
/// # Errors
/// Values other than `like` / `dislike`.
pub fn validate_reaction(v: &str) -> Result<String, DomainError> {
    match v {
        "like" | "dislike" => Ok(v.to_owned()),
        _ => Err(DomainError::invalid(
            Resource::Message,
            "reaction",
            "INVALID_REACTION",
            "reaction must be 'like' or 'dislike'",
        )),
    }
}

impl Core {
    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<message::Model, DomainError> {
        let chat = self.authorize_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let m = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::Id.eq(msg_id))
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&tenant_scope(chat.tenant_id))
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::not_found(Resource::Message, &msg_id))?;
        if m.role != "assistant" {
            return Err(DomainError::precondition(
                Resource::Message,
                "reaction_target",
                "STATE",
                "reactions are allowed on assistant messages only",
            ));
        }
        Ok(m)
    }

    /// Sets (upserts) the caller's reaction.
    ///
    /// # Errors
    /// 400 / 404 / PEP errors.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        value: &str,
    ) -> Result<ReactionView, DomainError> {
        let value = validate_reaction(value)?;
        let m = self
            .reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id)
            .await?;
        let ts = now();
        let am = reaction::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            message_id: ActiveValue::Set(m.id),
            user_id: ActiveValue::Set(ctx.subject_id()),
            tenant_id: ActiveValue::Set(m.tenant_id),
            reaction: ActiveValue::Set(value.clone()),
            created_at: ActiveValue::Set(ts),
        };
        let conn = self.db.conn()?;
        let res = reaction::Entity::insert(am)
            .secure()
            .scope_unchecked(&tenant_scope(m.tenant_id))?
            .on_conflict_raw(
                OnConflict::columns([reaction::Column::MessageId, reaction::Column::UserId])
                    .update_columns([reaction::Column::Reaction, reaction::Column::CreatedAt])
                    .to_owned(),
            )
            .exec(&conn)
            .await;
        match res {
            Ok(_) | Err(toolkit_db::secure::ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(ReactionView {
            message_id: m.id,
            reaction: value,
            created_at: ts,
        })
    }

    /// Removes the caller's reaction (idempotent).
    ///
    /// # Errors
    /// 400 / 404 / PEP errors.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<(), DomainError> {
        let m = self
            .reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        reaction::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(reaction::Column::MessageId.eq(m.id))
                    .add(reaction::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&tenant_scope(m.tenant_id))
            .exec(&conn)
            .await?;
        Ok(())
    }
}
