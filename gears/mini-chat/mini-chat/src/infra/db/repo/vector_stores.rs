//! `chat_vector_stores` repository. Always accessed by `(tenant_id, chat_id)`.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_vector_store;

pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<chat_vector_store::Model>, DomainError> {
    Ok(chat_vector_store::Entity::find()
        .filter(
            Condition::all()
                .add(chat_vector_store::Column::TenantId.eq(tenant_id))
                .add(chat_vector_store::Column::ChatId.eq(chat_id)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Insert the creation placeholder (`vector_store_id = NULL`).
pub async fn insert_placeholder(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    provider: &str,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = chat_vector_store::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        vector_store_id: Set(None),
        provider: Set(provider.to_owned()),
        file_count: Set(0),
        created_at: Set(now),
    };
    chat_vector_store::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(())
}

/// CAS: set the provider id only while it is NULL.
pub async fn cas_set_id(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row_id: Uuid,
    vector_store_id: &str,
) -> Result<bool, DomainError> {
    let res = chat_vector_store::Entity::update_many()
        .secure()
        .col_expr(
            chat_vector_store::Column::VectorStoreId,
            Expr::value(vector_store_id.to_owned()),
        )
        .filter(
            Condition::all()
                .add(chat_vector_store::Column::Id.eq(row_id))
                .add(chat_vector_store::Column::VectorStoreId.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

pub async fn delete_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row_id: Uuid,
) -> Result<(), DomainError> {
    chat_vector_store::Entity::delete_many()
        .filter(Condition::all().add(chat_vector_store::Column::Id.eq(row_id)))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}
