//! `message_reactions` queries. Reactions hang off messages of an authorized
//! chat; every query is narrowed to the caller (`user_id`) and tenant.

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use uuid::Uuid;

use super::tenant_scope;
use crate::domain::enums::ReactionKind;
use crate::domain::error::DomainError;
use crate::infra::db::entities::message_reaction;

/// Sets the user's reaction on a message, replacing an existing one (upsert
/// on `(message_id, user_id)`), and returns the stored row.
///
/// # Errors
/// Database failure.
pub async fn upsert(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    message_id: Uuid,
    user_id: Uuid,
    reaction: ReactionKind,
    now: OffsetDateTime,
) -> Result<message_reaction::Model, DomainError> {
    let scope = tenant_scope(tenant_id);
    let am = message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(message_id),
        user_id: Set(user_id),
        tenant_id: Set(tenant_id),
        reaction: Set(reaction.as_str().to_owned()),
        created_at: Set(now),
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
        .exec(runner)
        .await?;
    message_reaction::Entity::find()
        .filter(message_reaction::Column::MessageId.eq(message_id))
        .filter(message_reaction::Column::UserId.eq(user_id))
        .secure()
        .scope_with(&scope)
        .one(runner)
        .await?
        .ok_or_else(|| DomainError::Internal("upserted reaction not found".to_owned()))
}

/// Removes the user's reaction on a message (no-op when there is none).
///
/// # Errors
/// Database failure.
pub async fn delete(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<(), DomainError> {
    message_reaction::Entity::delete_many()
        .filter(message_reaction::Column::MessageId.eq(message_id))
        .filter(message_reaction::Column::UserId.eq(user_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// The stored reaction value of `row` (`Internal` for an unknown value).
///
/// # Errors
/// `Internal` when the stored value is not `like` / `dislike`.
pub fn reaction_kind(row: &message_reaction::Model) -> Result<ReactionKind, DomainError> {
    ReactionKind::parse(&row.reaction).ok_or_else(|| {
        DomainError::Internal(format!("reaction {} has value {}", row.id, row.reaction))
    })
}

/// The user's reactions on `message_ids` (messages without one are absent).
///
/// # Errors
/// `Internal` for an unknown stored value, database failure.
pub async fn mine_for_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, ReactionKind>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_reaction::Entity::find()
        .filter(message_reaction::Column::MessageId.is_in(message_ids.iter().copied()))
        .filter(message_reaction::Column::UserId.eq(user_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(runner)
        .await?;
    rows.iter()
        .map(|r| reaction_kind(r).map(|k| (r.message_id, k)))
        .collect()
}
