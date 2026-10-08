//! `chat_vector_stores` queries (DESIGN §3.7 "Creation protocol").
//!
//! Every query is keyed by `(tenant_id, chat_id)` of a chat the caller already
//! loaded through a scoped query; there is no access by `id` or by
//! `vector_store_id` alone.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, IntoActiveModel, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::infra::db::entities::chat_vector_store::{Column, Entity, Model};

/// Queries over `chat_vector_stores`.
pub struct VectorStoreRepo;

fn chat_filter(chat_id: Uuid) -> Condition {
    Condition::all().add(Column::ChatId.eq(chat_id))
}

impl VectorStoreRepo {
    /// The row of `chat_id`, if any (`vector_store_id` NULL = creation in progress).
    ///
    /// # Errors
    /// Database failures.
    pub async fn find(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<Option<Model>> {
        Ok(Entity::find()
            .filter(chat_filter(chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// Insert the creation placeholder (`vector_store_id = NULL`, `file_count = 0`).
    ///
    /// # Errors
    /// `Conflict { unique_violation }` when the chat already has a row (loser
    /// path); other database failures.
    pub async fn insert_placeholder(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
        now: DateTime<Utc>,
    ) -> DomainResult<Model> {
        let row = Model {
            id: Uuid::new_v4(),
            tenant_id,
            chat_id,
            vector_store_id: None,
            provider: provider.to_owned(),
            file_count: 0,
            created_at: now,
        };
        secure_insert::<Entity>(
            row.clone().into_active_model(),
            &AccessScope::for_tenant(tenant_id),
            runner,
        )
        .await?;
        Ok(row)
    }

    /// Compare-and-set of the provider id on the placeholder `row_id`
    /// (`WHERE vector_store_id IS NULL`); `false` when the placeholder is gone
    /// or already set.
    ///
    /// # Errors
    /// Database failures.
    pub async fn set_vector_store_id(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> DomainResult<bool> {
        let res = Entity::update_many()
            .col_expr(Column::VectorStoreId, Expr::value(vector_store_id))
            .filter(
                chat_filter(chat_id)
                    .add(Column::Id.eq(row_id))
                    .add(Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Delete the placeholder `row_id` while its `vector_store_id` is still NULL.
    ///
    /// # Errors
    /// Database failures.
    pub async fn delete_placeholder(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> DomainResult<bool> {
        let res = Entity::delete_many()
            .filter(
                chat_filter(chat_id)
                    .add(Column::Id.eq(row_id))
                    .add(Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Delete the row `row_id` (the durable marker that vector-store cleanup is
    /// outstanding) once the provider store is gone.
    ///
    /// # Errors
    /// Database failures.
    pub async fn delete_row(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> DomainResult<bool> {
        let res = Entity::delete_many()
            .filter(chat_filter(chat_id).add(Column::Id.eq(row_id)))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }
}
