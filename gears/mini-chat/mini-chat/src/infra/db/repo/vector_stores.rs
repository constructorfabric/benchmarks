//! `VectorStoreRepository` over the Secure ORM. Every query carries the chat
//! id next to the tenant scope: the table is only reached through a chat.

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::attachments::VectorStoreRow;
use crate::domain::error::DomainError;
use crate::domain::ports::VectorStoreRepository;
use crate::infra::db::entity::chat_vector_stores;
use crate::infra::db::timestamp::{from_db, to_db};

fn to_row(model: chat_vector_stores::Model) -> VectorStoreRow {
    VectorStoreRow {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        vector_store_id: model.vector_store_id,
        provider: model.provider,
        created_at: from_db(model.created_at),
    }
}

#[async_trait]
impl<R: DBRunner> VectorStoreRepository for R {
    async fn find_vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        Ok(self
            .find_vector_store(scope, chat_id)
            .await?
            .and_then(|row| row.vector_store_id))
    }

    async fn find_vector_store(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        let model = chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn insert_vector_store_placeholder(
        &self,
        scope: &AccessScope,
        row: &VectorStoreRow,
    ) -> Result<(), DomainError> {
        secure_insert::<chat_vector_stores::Entity>(
            chat_vector_stores::ActiveModel {
                id: Set(row.id),
                tenant_id: Set(row.tenant_id),
                chat_id: Set(row.chat_id),
                vector_store_id: Set(row.vector_store_id.clone()),
                provider: Set(row.provider.clone()),
                file_count: Set(0),
                created_at: Set(to_db(row.created_at)),
            },
            scope,
            self,
        )
        .await?;
        Ok(())
    }

    async fn set_vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        let updated = chat_vector_stores::Entity::update_many()
            .col_expr(
                chat_vector_stores::Column::VectorStoreId,
                Expr::value(vector_store_id),
            )
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .filter(chat_vector_stores::Column::Id.eq(row_id))
            .filter(chat_vector_stores::Column::VectorStoreId.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn delete_vector_store_placeholder(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError> {
        let deleted = chat_vector_stores::Entity::delete_many()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .filter(chat_vector_stores::Column::Id.eq(row_id))
            .filter(chat_vector_stores::Column::VectorStoreId.is_null())
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(deleted.rows_affected > 0)
    }

    async fn delete_vector_store_row(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        let deleted = chat_vector_stores::Entity::delete_many()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(deleted.rows_affected > 0)
    }
}
