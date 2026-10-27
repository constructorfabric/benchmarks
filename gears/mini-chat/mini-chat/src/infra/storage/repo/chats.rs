//! Chat repository.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt};
use toolkit_odata::{ODataQuery, Page};
use uuid::Uuid;

use super::{Db, now};
use super::super::entity::chats;
use crate::api::rest::dto::ChatListQueryFilterField;
use crate::error::DomainError;

/// New chat row.
pub struct NewChat {
    /// Chat id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning user.
    pub user_id: Uuid,
    /// Selected model.
    pub model: String,
    /// Title.
    pub title: Option<String>,
}

/// `OData` field mapping for the chat list.
pub struct ChatODataMapping;

impl FieldToColumn<ChatListQueryFilterField> for ChatODataMapping {
    type Column = chats::Column;

    fn map_field(field: ChatListQueryFilterField) -> Self::Column {
        match field {
            ChatListQueryFilterField::UpdatedAt => chats::Column::UpdatedAt,
            ChatListQueryFilterField::Id => chats::Column::Id,
            ChatListQueryFilterField::Title => chats::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatListQueryFilterField> for ChatODataMapping {
    type Entity = chats::Entity;

    fn extract_cursor_value(model: &chats::Model, field: ChatListQueryFilterField) -> sea_orm::Value {
        match field {
            ChatListQueryFilterField::UpdatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.updated_at))
            }
            ChatListQueryFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            ChatListQueryFilterField::Title => model.title.clone().into(),
        }
    }
}

/// Chat repository.
#[derive(Clone)]
pub struct ChatRepo {
    #[allow(dead_code)]
    db: Db,
}

impl ChatRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    fn scoped(
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
    ) -> toolkit_db::secure::SecureSelect<chats::Entity, toolkit_db::secure::Scoped> {
        chats::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(chats::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chats::Column::UserId.eq(user_id)))
    }

    /// Insert a chat.
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        new: NewChat,
    ) -> Result<chats::Model, DomainError> {
        let now = now();
        let am = chats::ActiveModel {
            id: ActiveValue::Set(new.id),
            tenant_id: ActiveValue::Set(new.tenant_id),
            user_id: ActiveValue::Set(new.user_id),
            model: ActiveValue::Set(new.model),
            title: ActiveValue::Set(new.title),
            is_temporary: ActiveValue::Set(false),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
            deleted_at: ActiveValue::Set(None),
        };
        let model = toolkit_db::secure::secure_insert::<chats::Entity>(am, scope, runner).await?;
        Ok(model)
    }

    /// Load a chat (owner-scoped, not deleted).
    pub async fn get(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<chats::Model>, DomainError> {
        let row = Self::scoped(scope, tenant_id, user_id)
            .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Internal chat load by (tenant, id), including soft-deleted rows, no
    /// owner filter (background cleanup handlers).
    pub async fn get_internal(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<chats::Model>, DomainError> {
        let row = chats::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chats::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// List chats (owner-scoped, not deleted), cursor-paginated.
    pub async fn list_page(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<chats::Model>, DomainError> {
        let select = Self::scoped(scope, tenant_id, user_id)
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()));
        let page = paginate_odata::<ChatListQueryFilterField, ChatODataMapping, _, _, _, _>(
            select,
            runner,
            query,
            ("updated_at", toolkit_odata::SortDir::Desc),
            LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await
        .map_err(|e| {
            tracing::debug!("mini-chat: chat list pagination failed: {e}");
            odata_err(e)
        })?;
        Ok(page)
    }

    /// Rename a chat (CAS on not-deleted). Returns the updated row.
    pub async fn rename(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
        title: Option<String>,
    ) -> Result<Option<chats::Model>, DomainError> {
        let now = now();
        let updated = chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(title))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
            .filter(Condition::all().add(chats::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chats::Column::UserId.eq(user_id)))
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?;
        if updated.rows_affected == 0 {
            return Ok(None);
        }
        self.get(runner, scope, tenant_id, user_id, chat_id).await
    }

    /// Soft-delete a chat (CAS on not-deleted). Returns whether a row was
    /// transitioned.
    pub async fn soft_delete(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        let now = now();
        let res = chats::Entity::update_many()
            .col_expr(chats::Column::DeletedAt, Expr::value(Some(now)))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
            .filter(Condition::all().add(chats::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chats::Column::UserId.eq(user_id)))
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?;
        Ok(res.rows_affected > 0)
    }

    /// Bump `updated_at` (same transaction as a sent message / retry / edit).
    pub async fn touch(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let now = now();
        chats::Entity::update_many()
            .col_expr(chats::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
            .filter(Condition::all().add(chats::Column::TenantId.eq(tenant_id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }
}

/// Map an `OData` pagination failure to the canonical 400 error.
#[must_use]
pub fn odata_err(e: toolkit_odata::Error) -> DomainError {
    DomainError::Canonical(toolkit_canonical_errors::CanonicalError::from(e))
}
