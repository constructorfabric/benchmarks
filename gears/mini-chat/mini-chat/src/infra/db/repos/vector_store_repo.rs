//! `chat_vector_store` repository (chat child: tenant-only scope).
//!
//! D§3.7: access by `id` or by `vector_store_id` alone is forbidden; every
//! lookup is by `(tenant_id, chat_id)`.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureUpdateExt,
};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::chat_vector_store;

/// Repository for `chat_vector_stores` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct VectorStoreRepo;

impl VectorStoreRepo {
    /// Insert a row (normally the `vector_store_id = NULL` placeholder).
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error; a second row for the
    /// same chat is a unique violation.
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: chat_vector_store::Model,
    ) -> Result<chat_vector_store::Model, ScopeError> {
        insert_model::<chat_vector_store::Entity>(runner, &scope.tenant_only(), row).await
    }

    /// The chat's row, if any.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_chat(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<chat_vector_store::Model>, ScopeError> {
        chat_vector_store::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
            .one(runner)
            .await
    }

    /// Compare-and-set of the provider store id on the chat's placeholder
    /// `row_id` (only while `vector_store_id IS NULL`). Returns the number of
    /// rows updated (0: the placeholder was reclaimed or already set).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn set_vector_store_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<u64, ScopeError> {
        let res = chat_vector_store::Entity::update_many()
            .filter(chat_vector_store::Column::ChatId.eq(chat_id))
            .filter(chat_vector_store::Column::Id.eq(row_id))
            .filter(chat_vector_store::Column::VectorStoreId.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(
                chat_vector_store::Column::VectorStoreId,
                Expr::value(vector_store_id.to_owned()),
            )
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Delete the chat's row `row_id` (chat cleanup, after the provider
    /// store was deleted). Returns the number of rows deleted.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn delete_row(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<u64, ScopeError> {
        let res = chat_vector_store::Entity::delete_many()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::ChatId.eq(chat_id))
                    .add(chat_vector_store::Column::Id.eq(row_id)),
            )
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Delete the chat's placeholder `row_id` while it is still a placeholder
    /// (`vector_store_id IS NULL`). Returns the number of rows deleted.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn delete_placeholder(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<u64, ScopeError> {
        let res = chat_vector_store::Entity::delete_many()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::ChatId.eq(chat_id))
                    .add(chat_vector_store::Column::Id.eq(row_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
            )
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }
}
