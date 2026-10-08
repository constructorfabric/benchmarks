//! `chat_vector_stores` queries (DESIGN section 3.7, creation protocol).
//!
//! Every query is chat-scoped: it filters by the tenant (secure scope) and the
//! `chat_id` of an already authorized chat.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use uuid::Uuid;

use super::tenant_scope;
use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_vector_store;

/// The chat's row (placeholder or complete).
///
/// # Errors
/// Database failure.
pub async fn find(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<chat_vector_store::Model>, DomainError> {
    Ok(chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::TenantId.eq(tenant_id))
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// Inserts the placeholder (`vector_store_id = NULL`); returns its row id.
///
/// # Errors
/// `UniqueViolation` when the chat already has a row, database failure.
pub async fn insert_placeholder(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    provider: &str,
    now: OffsetDateTime,
) -> Result<Uuid, DomainError> {
    let id = Uuid::new_v4();
    secure_insert::<chat_vector_store::Entity>(
        chat_vector_store::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            vector_store_id: Set(None),
            provider: Set(provider.to_owned()),
            file_count: Set(0),
            created_at: Set(now),
        },
        &tenant_scope(tenant_id),
        runner,
    )
    .await?;
    Ok(id)
}

/// Compare-and-set of the store id on the placeholder `row_id`; 0 when the
/// placeholder is gone or already has a store.
///
/// # Errors
/// Database failure.
pub async fn set_store_id(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
    vector_store_id: &str,
) -> Result<u64, DomainError> {
    Ok(chat_vector_store::Entity::update_many()
        .col_expr(
            chat_vector_store::Column::VectorStoreId,
            Expr::value(Some(vector_store_id.to_owned())),
        )
        .filter(chat_vector_store::Column::Id.eq(row_id))
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .filter(chat_vector_store::Column::VectorStoreId.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Deletes the placeholder `row_id` if it still has no store.
///
/// # Errors
/// Database failure.
pub async fn delete_placeholder(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<u64, DomainError> {
    Ok(chat_vector_store::Entity::delete_many()
        .filter(chat_vector_store::Column::Id.eq(row_id))
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .filter(chat_vector_store::Column::VectorStoreId.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Deletes the chat's placeholder when it is older than `cutoff` (its creator
/// died between the insert and the CAS).
///
/// # Errors
/// Database failure.
pub async fn delete_stale_placeholder(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    cutoff: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(chat_vector_store::Entity::delete_many()
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .filter(chat_vector_store::Column::VectorStoreId.is_null())
        .filter(chat_vector_store::Column::CreatedAt.lt(cutoff))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Deletes the chat's row `row_id` (vector-store cleanup completion marker).
///
/// # Errors
/// Database failure.
pub async fn delete_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<u64, DomainError> {
    Ok(chat_vector_store::Entity::delete_many()
        .filter(chat_vector_store::Column::Id.eq(row_id))
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}
