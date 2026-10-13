//! Statements on `chat_vector_stores` (one provider vector store per chat). Tenant scoped;
//! callers pass a `chat_id` taken from an owner-scoped chat query.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt as _, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use uuid::Uuid;

use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::chat_vector_stores::{self, Column};
use crate::infra::db::ts;

/// The provider vector store id of the chat; `None` without a row or while it is being created.
///
/// # Errors
/// `Internal` on a database error.
pub async fn vector_store_id(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<String>, DomainError> {
    Ok(find(conn, scope, chat_id)
        .await?
        .and_then(|r| r.vector_store_id))
}

/// The chat's row (also while the store is being created).
///
/// # Errors
/// `Internal` on a database error.
pub async fn find(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_vector_stores::Model>, DomainError> {
    chat_vector_stores::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Inserts the creation placeholder (`vector_store_id = NULL`) of the chat's store.
///
/// # Errors
/// `Conflict { code: "unique_violation" }` when the chat already has a row, `Internal` on a
/// database error.
pub async fn insert_placeholder(
    conn: &impl DBRunner,
    scope: &AccessScope,
    row: chat_vector_stores::ActiveModel,
) -> Result<(), DomainError> {
    secure_insert::<chat_vector_stores::Entity>(row, scope, conn)
        .await
        .map(drop)
        .map_err(map_scope_err)
}

/// Compare-and-set of the provider id on placeholder `row_id`; `false` when the placeholder is
/// gone or already has an id.
///
/// # Errors
/// `Internal` on a database error.
pub async fn set_vector_store_id(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    row_id: Uuid,
    vector_store_id: &str,
) -> Result<bool, DomainError> {
    let res = chat_vector_stores::Entity::update_many()
        .col_expr(
            Column::VectorStoreId,
            Expr::value(Some(vector_store_id.to_owned())),
        )
        .filter(Column::Id.eq(row_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::VectorStoreId.is_null())
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Deletes placeholder `row_id` while it has no provider id and was created before
/// `created_before` (pass a future instant to delete it regardless of age); `false` when no row
/// matched.
///
/// # Errors
/// `Internal` on a database error.
pub async fn delete_placeholder(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    row_id: Uuid,
    created_before: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chat_vector_stores::Entity::delete_many()
        .filter(Column::Id.eq(row_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::VectorStoreId.is_null())
        .filter(Column::CreatedAt.lt(ts::normalize(created_before)))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// The vector store row of the chat of `tenant_id` (also while the store is being created).
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_for_cleanup(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<chat_vector_stores::Model>, DomainError> {
    chat_vector_stores::Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Deletes row `row_id` of the chat (the durable "vector store cleanup done" marker); `false`
/// when it is already gone.
///
/// # Errors
/// `Internal` on a database error.
pub async fn delete_row(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<bool, DomainError> {
    let res = chat_vector_stores::Entity::delete_many()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::Id.eq(row_id))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}
