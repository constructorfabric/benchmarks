//! `chats` repository.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, Scoped, SecureEntityExt, SecureInsertExt, SecureSelect,
    SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::chats::{ActiveModel, Column, Entity, Model};

/// Inserts a chat.
///
/// # Errors
/// Database errors.
pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    am: ActiveModel,
) -> Result<(), DomainError> {
    Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(())
}

/// Non-deleted chat visible in `scope`.
///
/// # Errors
/// Database errors.
pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Id.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Chat by id regardless of deletion (system access).
///
/// # Errors
/// Database errors.
pub async fn find_including_deleted(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Id.eq(chat_id))
        .filter(Column::TenantId.eq(tenant_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(runner)
        .await?)
}

/// Base select of non-deleted chats in `scope` (list endpoint).
pub fn list_select(scope: &AccessScope) -> SecureSelect<Entity, Scoped> {
    Entity::find()
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
}

/// Sets the title and bumps `updated_at`.
///
/// # Errors
/// Database errors.
pub async fn update_title(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    title: &str,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(Column::Title, Expr::value(title.to_owned()))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(Column::Id.eq(chat_id))
                .add(Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?
        .rows_affected)
}

/// Bumps `updated_at` (activity) of a chat.
///
/// # Errors
/// Database errors.
pub async fn touch(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    Entity::update_many()
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::Id.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Soft-deletes a chat; returns rows affected.
///
/// # Errors
/// Database errors.
pub async fn soft_delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(Column::DeletedAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(Column::Id.eq(chat_id))
                .add(Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?
        .rows_affected)
}
