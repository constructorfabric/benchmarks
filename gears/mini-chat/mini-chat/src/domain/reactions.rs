//! Message reactions (DESIGN §3.3 "Message Reaction API", §3.7 `message_reactions`).

use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::chats::load_chat;
use crate::domain::error::{DomainError, Resource};
use crate::domain::services::AppServices;
use crate::infra::db::entities::{chat, message, message_reaction};

/// Allowed reaction values.
pub const REACTIONS: [&str; 2] = ["like", "dislike"];

/// A stored reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}

/// Validates the reaction value (400 `INVALID_REACTION`, checked before authorization).
///
/// # Errors
/// `InvalidArgument` (`reaction` / `INVALID_REACTION`).
pub fn validate_reaction(value: &str) -> Result<&'static str, DomainError> {
    REACTIONS.iter().copied().find(|r| *r == value).ok_or_else(|| {
        DomainError::invalid(Resource::Message, "reaction", "INVALID_REACTION", "reaction must be 'like' or 'dislike'")
    })
}

/// Loads the chat (owner scope) and the target message, which must be a non-deleted
/// assistant message of the chat.
async fn load_target(
    app: &AppServices,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<(chat::Model, message::Model), DomainError> {
    let scope = app.authz.chat_scope(ctx, action, Some(chat_id)).await?;
    let chat = load_chat(app, &scope, chat_id).await?;
    let conn = app.db.conn()?;
    let msg = find_message(&conn, chat.tenant_id, chat.id, message_id).await?;
    if msg.role != "assistant" {
        return Err(DomainError::precondition(
            Resource::Message,
            "reaction_target",
            "STATE",
            "only assistant messages can receive reactions",
        ));
    }
    Ok((chat, msg))
}

async fn find_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<message::Model, DomainError> {
    message::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(message::Column::Id.eq(message_id))
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?
        .ok_or_else(|| DomainError::not_found(Resource::Message, message_id))
}

fn reaction_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// `PUT .../reaction`: upserts the caller's reaction by `(message_id, user_id)`.
///
/// # Errors
/// `INVALID_REACTION`, authz errors, 404 Chat / Message, `reaction_target` precondition, DB errors.
pub async fn set_reaction(
    app: &AppServices,
    ctx: &SecurityContext,
    chat_id: Uuid,
    message_id: Uuid,
    reaction: &str,
) -> Result<ReactionView, DomainError> {
    let reaction = validate_reaction(reaction)?;
    let (chat, msg) = load_target(app, ctx, "set_reaction", chat_id, message_id).await?;
    let user_id = ctx.subject_id();
    let scope = reaction_scope(chat.tenant_id, user_id);
    let now = clock::now();
    let am = message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(msg.id),
        user_id: Set(user_id),
        tenant_id: Set(chat.tenant_id),
        reaction: Set(reaction.to_owned()),
        created_at: Set(now),
    };
    let on_conflict = SecureOnConflict::<message_reaction::Entity>::columns([
        message_reaction::Column::MessageId,
        message_reaction::Column::UserId,
    ])
    .update_columns([message_reaction::Column::Reaction, message_reaction::Column::CreatedAt])?;
    let conn = app.db.conn()?;
    message_reaction::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)?
        .on_conflict(on_conflict)
        .exec(&conn)
        .await?;
    let row = message_reaction::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message_reaction::Column::MessageId.eq(msg.id))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .one(&conn)
        .await?
        .ok_or_else(|| DomainError::internal("reaction not found after upsert"))?;
    Ok(ReactionView { message_id: row.message_id, reaction: row.reaction, created_at: row.created_at })
}

/// `DELETE .../reaction`: removes the caller's reaction if any (idempotent).
///
/// # Errors
/// Authz errors, 404 Chat / Message, `reaction_target` precondition, DB errors.
pub async fn delete_reaction(
    app: &AppServices,
    ctx: &SecurityContext,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<(), DomainError> {
    let (chat, msg) = load_target(app, ctx, "delete_reaction", chat_id, message_id).await?;
    let user_id = ctx.subject_id();
    let conn = app.db.conn()?;
    message_reaction::Entity::delete_many()
        .filter(
            Condition::all()
                .add(message_reaction::Column::MessageId.eq(msg.id))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(&reaction_scope(chat.tenant_id, user_id))
        .exec(&conn)
        .await?;
    Ok(())
}
