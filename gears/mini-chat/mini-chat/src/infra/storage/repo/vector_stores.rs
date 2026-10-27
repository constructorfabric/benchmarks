//! Chat vector store repository.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{Db, now};
use super::super::entity::chat_vector_stores;
use crate::error::DomainError;

/// Chat vector store repository.
#[derive(Clone)]
pub struct VectorStoreRepo {
    #[allow(dead_code)]
    db: Db,
}

impl VectorStoreRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Load the row for a chat (tenant-scoped).
    pub async fn get(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<chat_vector_stores::Model>, DomainError> {
        let row = chat_vector_stores::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_vector_stores::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat_id)))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Insert a placeholder row (`vector_store_id = NULL`). Returns `None`
    /// when the row already exists (unique violation — loser path).
    pub async fn insert_placeholder(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
    ) -> Result<Option<chat_vector_stores::Model>, DomainError> {
        let am = chat_vector_stores::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            tenant_id: ActiveValue::Set(tenant_id),
            chat_id: ActiveValue::Set(chat_id),
            vector_store_id: ActiveValue::Set(None),
            provider: ActiveValue::Set(provider.to_owned()),
            file_count: ActiveValue::Set(0),
            created_at: ActiveValue::Set(now()),
        };
        let res = toolkit_db::secure::secure_insert::<chat_vector_stores::Entity>(am, &AccessScope::allow_all(), runner).await;
        match res {
            Ok(model) => Ok(Some(model)),
            Err(e) if e.is_unique_violation() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// CAS-set the vector store id (only when still NULL). Returns whether
    /// the update happened.
    pub async fn cas_set_id(
        &self,
        runner: &impl DBRunner,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        let res = chat_vector_stores::Entity::update_many()
            .col_expr(
                chat_vector_stores::Column::VectorStoreId,
                Expr::value(Some(vector_store_id.to_owned())),
            )
            .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)))
            .filter(Condition::all().add(chat_vector_stores::Column::VectorStoreId.is_null()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Delete the row by id (placeholder reclaim / post-cleanup).
    pub async fn delete_by_id(
        &self,
        runner: &impl DBRunner,
        row_id: Uuid,
    ) -> Result<(), DomainError> {
        chat_vector_stores::Entity::delete_many()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Delete the row of a chat (chat cleanup handler).
    pub async fn delete_for_chat(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        chat_vector_stores::Entity::delete_many()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_vector_stores::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat_id)))
            .exec(runner)
            .await?;
        Ok(())
    }
}
