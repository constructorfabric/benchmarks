//! `chats` queries. Every query excludes soft-deleted chats.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use uuid::Uuid;

use super::tenant_scope;
use crate::domain::error::DomainError;
use crate::infra::db::entities::chat;

/// `GET /v1/chats` page size: default 20, larger values clamped to 100.
pub const CHAT_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// `$filter` / `$orderby` fields of `GET /v1/chats` (contract order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatFilterField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatFilterField {
    const FIELDS: &'static [Self] = &[Self::UpdatedAt, Self::Id, Self::Title];

    fn name(&self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated_at",
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::UpdatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }
}

pub struct ChatMapper;

impl FieldToColumn<ChatFilterField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(field: ChatFilterField) -> chat::Column {
        match field {
            ChatFilterField::UpdatedAt => chat::Column::UpdatedAt,
            ChatFilterField::Id => chat::Column::Id,
            ChatFilterField::Title => chat::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatFilterField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: ChatFilterField) -> sea_orm::Value {
        match field {
            ChatFilterField::UpdatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.updated_at))
            }
            ChatFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            // Untitled chats store NULL; a NULL cursor value would fail
            // encoding, so they page as the empty string. Known, accepted
            // limitation: the keyset predicate compares the raw column, so with
            // `$orderby=title` NULL-title rows at a page boundary can be
            // skipped (`title > ''` is never true for NULL). Default order
            // (`updated_at`, `id`) is unaffected.
            ChatFilterField::Title => {
                sea_orm::Value::String(Some(model.title.clone().unwrap_or_default()))
            }
        }
    }
}

/// The live (non-deleted) chat `chat_id` visible through `scope`.
///
/// # Errors
/// Database failure.
pub async fn find_scoped(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat::Model>, DomainError> {
    Ok(chat::Entity::find()
        .filter(chat::Column::Id.eq(chat_id))
        .filter(chat::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// The chat `chat_id` visible through `scope`, soft-deleted or not.
///
/// # Errors
/// Database failure.
pub async fn find_any_scoped(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat::Model>, DomainError> {
    Ok(chat::Entity::find()
        .filter(chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Inserts a chat row; `scope` must admit its tenant and owner.
///
/// # Errors
/// Scope violation or database failure.
pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    am: chat::ActiveModel,
) -> Result<chat::Model, DomainError> {
    Ok(secure_insert::<chat::Entity>(am, scope, runner).await?)
}

/// Sets the title and `updated_at` of a live chat. Returns the affected row
/// count (0 when the chat is missing, deleted or out of scope).
///
/// # Errors
/// Database failure.
pub async fn update_title(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    title: &str,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(chat::Entity::update_many()
        .col_expr(chat::Column::Title, Expr::value(Some(title.to_owned())))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(chat::Column::Id.eq(chat_id))
        .filter(chat::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?
        .rows_affected)
}

/// Soft-deletes a live chat (`deleted_at = updated_at = now`). Returns the
/// affected row count.
///
/// # Errors
/// Database failure.
pub async fn soft_delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(chat::Entity::update_many()
        .col_expr(chat::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(chat::Column::Id.eq(chat_id))
        .filter(chat::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?
        .rows_affected)
}

/// Sets `updated_at = now` on the chat (tenant-scoped; used in the same
/// transaction as sent messages, retries and edits).
///
/// # Errors
/// Database failure.
pub async fn touch_updated_at(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Page of live chats visible through `scope`. Default order `updated_at
/// desc`, tiebreaker `id desc`.
///
/// # Errors
/// `Query` for a bad `OData` query or a pagination failure.
pub async fn list_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    mut query: ODataQuery,
) -> Result<Page<chat::Model>, DomainError> {
    if query.cursor.is_none() && query.order.is_empty() {
        query.order = ODataOrderBy(vec![OrderKey {
            field: ChatFilterField::UpdatedAt.name().to_owned(),
            dir: SortDir::Desc,
        }]);
    }
    let select = chat::Entity::find()
        .filter(chat::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope);
    Ok(
        paginate_odata::<ChatFilterField, ChatMapper, chat::Entity, chat::Model, _, _>(
            select,
            runner,
            &query,
            (ChatFilterField::Id.name(), SortDir::Desc),
            CHAT_LIMITS,
            |m| m,
        )
        .await?,
    )
}
