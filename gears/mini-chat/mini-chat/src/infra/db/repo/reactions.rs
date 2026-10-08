//! `message_reactions` repository.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::message_reactions::{ActiveModel, Column, Entity, Model};

fn scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// Reactions of a user on the given messages.
///
/// # Errors
/// Database errors.
pub async fn for_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<Vec<Model>, DomainError> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(Entity::find()
        .filter(Column::MessageId.is_in(message_ids.to_vec()))
        .filter(Column::UserId.eq(user_id))
        .secure()
        .scope_with(&scope(tenant_id, user_id))
        .all(runner)
        .await?)
}

/// Upserts the reaction of a user; returns the stored row.
///
/// # Errors
/// Database errors.
pub async fn upsert(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
    reaction: &str,
    now: DateTime<Utc>,
) -> Result<Model, DomainError> {
    let sc = scope(tenant_id, user_id);
    let existing = Entity::find()
        .filter(Column::MessageId.eq(message_id))
        .filter(Column::UserId.eq(user_id))
        .secure()
        .scope_with(&sc)
        .one(runner)
        .await?;
    if let Some(row) = existing {
        Entity::update_many()
            .col_expr(Column::Reaction, Expr::value(reaction.to_owned()))
            .col_expr(Column::CreatedAt, Expr::value(now))
            .filter(Column::Id.eq(row.id))
            .secure()
            .scope_with(&sc)
            .exec(runner)
            .await?;
        return Ok(Model {
            reaction: reaction.to_owned(),
            created_at: now,
            ..row
        });
    }
    let model = Model {
        id: Uuid::new_v4(),
        message_id,
        user_id,
        tenant_id,
        reaction: reaction.to_owned(),
        created_at: now,
    };
    let am: ActiveModel = model.clone().into();
    Entity::insert(am)
        .secure()
        .scope_unchecked(&sc)?
        .exec(runner)
        .await?;
    Ok(model)
}

/// Removes the reaction of a user (idempotent).
///
/// # Errors
/// Database errors.
pub async fn delete(runner: &impl DBRunner, tenant_id: Uuid, user_id: Uuid, message_id: Uuid) -> Result<(), DomainError> {
    Entity::delete_many()
        .filter(Column::MessageId.eq(message_id))
        .filter(Column::UserId.eq(user_id))
        .secure()
        .scope_with(&scope(tenant_id, user_id))
        .exec(runner)
        .await?;
    Ok(())
}
