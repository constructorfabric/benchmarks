//! `message_reactions` repository (owner-scoped by `user_id`).

use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::message_reaction;

/// Insert or replace the user's reaction on a message; returns the stored row.
pub async fn upsert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
    reaction: &str,
    now: OffsetDateTime,
) -> Result<message_reaction::Model, DomainError> {
    let am = message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(message_id),
        user_id: Set(user_id),
        tenant_id: Set(tenant_id),
        reaction: Set(reaction.to_owned()),
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
        .scope_with_model(scope, &am)?
        .on_conflict(on_conflict)
        .exec(runner)
        .await?;
    find(runner, scope, message_id, user_id)
        .await?
        .ok_or_else(|| DomainError::internal("reaction vanished after upsert"))
}

pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<Option<message_reaction::Model>, DomainError> {
    Ok(message_reaction::Entity::find()
        .filter(
            Condition::all()
                .add(message_reaction::Column::MessageId.eq(message_id))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

pub async fn delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<(), DomainError> {
    message_reaction::Entity::delete_many()
        .filter(
            Condition::all()
                .add(message_reaction::Column::MessageId.eq(message_id))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// The user's reactions on the given messages.
pub async fn for_messages(
    runner: &impl DBRunner,
    scope: &AccessScope,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, String>, DomainError> {
    if message_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = message_reaction::Entity::find()
        .filter(
            Condition::all()
                .add(message_reaction::Column::UserId.eq(user_id))
                .add(message_reaction::Column::MessageId.is_in(message_ids.iter().copied())),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.message_id, r.reaction))
        .collect())
}
