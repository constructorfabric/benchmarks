//! Message reactions (OWNER: REST CRUD work package).
//!
//! `PUT/DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (DESIGN §3.3 Message Reaction API).

use std::sync::Arc;

use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::api::rest::dto::{MiniChatReactionDto, ReactionKindDto};
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::service::Deps;
use crate::domain::service::chat_access::load_chat;
use crate::domain::service::messages::reaction_dto;
use crate::infra::db::entity::{message, message_reaction};

/// Parses the requested reaction value (`like` | `dislike`).
///
/// # Errors
/// 400 `invalid_argument` (`reaction` / `INVALID_REACTION`).
pub fn parse_reaction(raw: &str) -> Result<ReactionKindDto, DomainError> {
    match raw {
        "like" => Ok(ReactionKindDto::Like),
        "dislike" => Ok(ReactionKindDto::Dislike),
        _ => Err(DomainError::invalid(
            resource_types::MESSAGE,
            "reaction",
            reasons::INVALID_REACTION,
            "Reaction must be 'like' or 'dislike'",
        )),
    }
}

const fn reaction_str(r: ReactionKindDto) -> &'static str {
    match r {
        ReactionKindDto::Like => "like",
        ReactionKindDto::Dislike => "dislike",
    }
}

/// Loads a non-deleted assistant message of `chat_id`.
///
/// # Errors
/// 404 (message) when missing/deleted/in another chat; 400 `failed_precondition`
/// (`reaction_target` / `STATE`) for user and system messages.
async fn load_target(
    runner: &impl DBRunner,
    child_scope: &AccessScope,
    chat_id: Uuid,
    msg_id: Uuid,
) -> Result<message::Model, DomainError> {
    let msg = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::DeletedAt.is_null())
        .secure()
        .scope_with(child_scope)
        .and_id(msg_id)?
        .one(runner)
        .await?
        .ok_or(DomainError::NotFound {
            resource: resource_types::MESSAGE,
        })?;
    if msg.role != "assistant" {
        return Err(DomainError::precondition(
            "reaction_target",
            reasons::STATE,
            "Only assistant messages can receive reactions",
        ));
    }
    Ok(msg)
}

pub struct ReactionService {
    deps: Arc<Deps>,
}

impl ReactionService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// `PUT .../reaction`: validates the value first, then upserts on `(message_id, user_id)`.
    ///
    /// # Errors
    /// 400 `INVALID_REACTION` (before the PEP), 404 chat/message, 400 `reaction_target`, 403/503.
    pub async fn set(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        raw_reaction: &str,
    ) -> Result<MiniChatReactionDto, DomainError> {
        let reaction = parse_reaction(raw_reaction)?;
        let ac = load_chat(&self.deps, ctx, chat_id, actions::SET_REACTION).await?;
        let conn = self.deps.db.conn()?;
        let msg = load_target(&conn, &ac.child_scope, ac.chat.id, msg_id).await?;

        let user_id = ctx.subject_id();
        let scope = ac.child_scope.ensure_owner(user_id);
        let now = OffsetDateTime::now_utc();
        let am = message_reaction::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            message_id: ActiveValue::Set(msg.id),
            user_id: ActiveValue::Set(user_id),
            tenant_id: ActiveValue::Set(ctx.subject_tenant_id()),
            reaction: ActiveValue::Set(reaction_str(reaction).to_owned()),
            created_at: ActiveValue::Set(now),
        };
        let on_conflict = SecureOnConflict::<message_reaction::Entity>::columns([
            message_reaction::Column::MessageId,
            message_reaction::Column::UserId,
        ])
        .update_columns([
            message_reaction::Column::Reaction,
            message_reaction::Column::CreatedAt,
        ])?;
        message_reaction::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict(on_conflict)
            .exec(&conn)
            .await?;

        let row = message_reaction::Entity::find()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(msg.id))
                    .add(message_reaction::Column::UserId.eq(user_id)),
            )
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("reaction row missing after upsert"))?;
        Ok(MiniChatReactionDto {
            message_id: row.message_id,
            reaction: reaction_dto(&row.reaction)?,
            created_at: row.created_at,
        })
    }

    /// `DELETE .../reaction`: removes the caller's reaction (idempotent).
    ///
    /// # Errors
    /// 404 chat/message, 400 `reaction_target`, 403/503.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<(), DomainError> {
        let ac = load_chat(&self.deps, ctx, chat_id, actions::DELETE_REACTION).await?;
        let conn = self.deps.db.conn()?;
        let msg = load_target(&conn, &ac.child_scope, ac.chat.id, msg_id).await?;
        let user_id = ctx.subject_id();
        let scope = ac.child_scope.ensure_owner(user_id);
        message_reaction::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(msg.id))
                    .add(message_reaction::Column::UserId.eq(user_id)),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "reactions_tests.rs"]
mod reactions_tests;
