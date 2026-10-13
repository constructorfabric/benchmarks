//! `chat_vector_stores` repository.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_vector_stores::{ActiveModel, Column, Entity, Model};

fn tscope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Vector store row of a chat.
///
/// # Errors
/// Database errors.
pub async fn find(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .one(runner)
        .await?)
}

/// Inserts a placeholder row (`vector_store_id = NULL`).
///
/// # Errors
/// Unique violation when another request won the race.
pub async fn insert_placeholder(runner: &impl DBRunner, tenant_id: Uuid, am: ActiveModel) -> Result<(), DomainError> {
    Entity::insert(am)
        .secure()
        .scope_unchecked(&tscope(tenant_id))?
        .exec(runner)
        .await?;
    Ok(())
}

/// CAS-sets the provider id on the placeholder.
///
/// # Errors
/// Database errors.
pub async fn set_vector_store_id(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    row_id: Uuid,
    vector_store_id: &str,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(Column::VectorStoreId, Expr::value(vector_store_id.to_owned()))
        .filter(Column::Id.eq(row_id))
        .filter(Column::VectorStoreId.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Deletes a row by id.
///
/// # Errors
/// Database errors.
pub async fn delete_row(runner: &impl DBRunner, tenant_id: Uuid, row_id: Uuid) -> Result<u64, DomainError> {
    Ok(Entity::delete_many()
        .filter(Column::Id.eq(row_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}
