//! `chat_vector_stores` repository (DESIGN section 3.7 `chat_vector_stores`,
//! "Creation protocol (P1, attachment service)").
//!
//! The table is tenant-scoped only: every query applies the tenant
//! predicates of the parent chat scope ([`child_scope`]) plus the parent
//! `chat_id`, after the chat was loaded with the full scope. No query
//! addresses a row by its id or by `vector_store_id` alone: the row id is
//! only an additional predicate of the creator's compare-and-set and of the
//! placeholder removal.
//!
//! - [`find`] — the chat's row (placeholder or created store);
//! - [`insert_placeholder`] — the `vector_store_id = NULL` row; a unique
//!   violation on `uq_chat_vector_stores_tenant_chat` means another upload
//!   owns the creation (`false`);
//! - [`set_vector_store_id`] — the winner's compare-and-set (`WHERE
//!   vector_store_id IS NULL`);
//! - [`delete_placeholder`] — removes a placeholder that is still NULL (failed
//!   creation, stale placeholder reclaim).

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{DbRepository, child_scope};
use crate::domain::error::DomainError;
use crate::domain::ports::{NewVectorStorePlaceholder, VectorStoreRecord, VectorStoreRepository};
use crate::infra::db::entities::{TimestampForm, chat_vector_stores};
use crate::infra::db::error::{UniqueTarget, unique_violation_target};

impl From<chat_vector_stores::Model> for VectorStoreRecord {
    fn from(model: chat_vector_stores::Model) -> Self {
        Self {
            id: model.id,
            vector_store_id: model.vector_store_id,
            provider: model.provider,
            created_at: model.created_at.at(),
        }
    }
}

fn of_chat(chat_id: Uuid) -> Condition {
    Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat_id))
}

/// The `chat_vector_stores` row of `chat_id`, if any.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn find(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<VectorStoreRecord>, DomainError> {
    let found = chat_vector_stores::Entity::find()
        .filter(of_chat(chat_id))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?;
    Ok(found.map(VectorStoreRecord::from))
}

/// Inserts the creation placeholder (timestamps bound as `ts`); `false`
/// when the chat already has a row.
///
/// # Errors
///
/// [`DomainError`] on any other insert failure.
pub async fn insert_placeholder(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    placeholder: NewVectorStorePlaceholder,
) -> Result<bool, DomainError> {
    let model = chat_vector_stores::ActiveModel {
        id: Set(placeholder.id),
        tenant_id: Set(placeholder.tenant_id),
        chat_id: Set(placeholder.chat_id),
        vector_store_id: Set(None),
        provider: Set(placeholder.provider),
        file_count: Set(0),
        created_at: Set(ts.stamp(placeholder.now)),
    };
    match secure_insert::<chat_vector_stores::Entity>(model, &child_scope(chat_scope), runner).await
    {
        Ok(_) => Ok(true),
        Err(err) => match unique_violation_target(&err) {
            Some(UniqueTarget::ChatVectorStore) => Ok(false),
            _ => Err(DomainError::from(err)),
        },
    }
}

/// The creator's compare-and-set: `vector_store_id = :id WHERE id = :row_id
/// AND vector_store_id IS NULL`; `false` when the placeholder is gone or set.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn set_vector_store_id(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    row_id: Uuid,
    vector_store_id: &str,
) -> Result<bool, DomainError> {
    let result = chat_vector_stores::Entity::update_many()
        .col_expr(
            chat_vector_stores::Column::VectorStoreId,
            Expr::value(Some(vector_store_id.to_owned())),
        )
        .filter(
            of_chat(chat_id)
                .add(chat_vector_stores::Column::Id.eq(row_id))
                .add(chat_vector_stores::Column::VectorStoreId.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Deletes the placeholder `row_id` of `chat_id` while it is still NULL;
/// `false` when nothing was deleted.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn delete_placeholder(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<bool, DomainError> {
    let result = chat_vector_stores::Entity::delete_many()
        .secure()
        .scope_with(&child_scope(chat_scope))
        .filter(
            of_chat(chat_id)
                .add(chat_vector_stores::Column::Id.eq(row_id))
                .add(chat_vector_stores::Column::VectorStoreId.is_null()),
        )
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

#[async_trait]
impl VectorStoreRepository for DbRepository {
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRecord>, DomainError> {
        let conn = self.db().conn()?;
        find(&conn, scope, chat_id).await
    }

    async fn insert_placeholder(
        &self,
        scope: &AccessScope,
        placeholder: NewVectorStorePlaceholder,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        insert_placeholder(&conn, self.timestamps(), scope, placeholder).await
    }

    async fn set_vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        set_vector_store_id(&conn, scope, chat_id, row_id, vector_store_id).await
    }

    async fn delete_placeholder(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        delete_placeholder(&conn, scope, chat_id, row_id).await
    }
}

/// Deletes the `chat_vector_stores` row `row_id` of `chat_id` (the durable
/// completion marker of the chat cleanup's vector-store delete); `false`
/// when nothing was deleted.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn delete_row(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<bool, DomainError> {
    let result = chat_vector_stores::Entity::delete_many()
        .secure()
        .scope_with(&child_scope(chat_scope))
        .filter(of_chat(chat_id).add(chat_vector_stores::Column::Id.eq(row_id)))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}
