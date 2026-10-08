//! Queries on `chats` and the child-table statements of chat deletion and the message count.
//!
//! Chat queries take the PDP scope (already narrowed to the calling user). Child tables
//! (`messages`, `attachments`) are tenant scoped only: callers pass `scope.tenant_only()` and a
//! `chat_id` taken from an owner-scoped chat query.

use std::collections::HashMap;

use sea_orm::sea_query::{Expr, ExprTrait as _, Func};
use sea_orm::{ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use super::keyset::{KeysetField, KeysetRow, KeysetSpec, paginate};
use crate::api::dto::chats::ChatField;
use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::{attachments, chats, messages};
use crate::infra::db::{CleanupStatus, ts};

/// Page size of the chat list.
const LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Maps the list's `OData` fields to `chats` columns and cursor values.
pub struct ChatODataMapper;

impl FieldToColumn<ChatField> for ChatODataMapper {
    type Column = chats::Column;

    fn map_field(field: ChatField) -> chats::Column {
        match field {
            ChatField::UpdatedAt => chats::Column::UpdatedAt,
            ChatField::Id => chats::Column::Id,
            ChatField::Title => chats::Column::Title,
        }
    }
}

/// Inserts a chat.
///
/// # Errors
/// `AccessDenied` when the row is outside `scope`, `Internal` on a database error.
pub async fn insert(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: chats::ActiveModel,
) -> Result<chats::Model, DomainError> {
    secure_insert::<chats::Entity>(chat, scope, conn)
        .await
        .map_err(map_scope_err)
}

/// The non-deleted chat `id` inside `scope`; `None` when it does not exist, is soft-deleted or
/// belongs to someone else.
///
/// # Errors
/// `Internal` on a database error.
pub async fn load_scoped(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> Result<Option<chats::Model>, DomainError> {
    chats::Entity::find()
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .and_id(id)
        .map_err(map_scope_err)?
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Locks the non-deleted chat `id` inside `scope` for the rest of the transaction (`FOR UPDATE`
/// on `PostgreSQL`; a no-op on `SQLite`, where the write transaction itself serializes) and
/// returns it; `None` when it does not exist or is soft-deleted. Serializes per-chat
/// check-then-insert sequences such as the attachment limits.
///
/// # Errors
/// `Internal` on a database error.
pub async fn lock_live(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> Result<Option<chats::Model>, DomainError> {
    chats::Entity::find()
        .filter(chats::Column::DeletedAt.is_null())
        .lock_exclusive()
        .secure()
        .scope_with(scope)
        .and_id(id)
        .map_err(map_scope_err)?
        .one(tx)
        .await
        .map_err(map_scope_err)
}

/// Sort position of a chat under one `$orderby` key: the column expression and how a row's
/// value is carried in a cursor.
///
/// An untitled chat (`title IS NULL`) sorts as the empty string, i.e. before every titled chat
/// in ascending order and after them in descending order, on every backend (`SQLite` and
/// `PostgreSQL` disagree on where `NULL` sorts, and a `NULL` cannot be carried in a platform
/// cursor or compared with `>`/`<`). Titles are never empty, so the mapping is unambiguous.
impl KeysetField for ChatField {
    fn sort_expr(self) -> Expr {
        match self {
            Self::UpdatedAt => Expr::col(chats::Column::UpdatedAt),
            Self::Id => Expr::col(chats::Column::Id),
            Self::Title => Func::coalesce([Expr::col(chats::Column::Title), Expr::val("")]).into(),
        }
    }
}

impl KeysetRow for chats::Model {
    type Field = ChatField;

    fn sort_value(&self, field: ChatField) -> sea_orm::Value {
        match field {
            ChatField::UpdatedAt => self.updated_at.into(),
            ChatField::Id => self.id.into(),
            ChatField::Title => self.title.clone().unwrap_or_default().into(),
        }
    }
}

/// One page of the caller's non-deleted chats. Without `$orderby` and cursor the order is
/// `updated_at desc, id desc` (most recent activity first). Keyset pagination over
/// [`ChatField::sort_expr`], where `NULL` titles sort as `''` (see [`super::keyset`]).
///
/// # Errors
/// `OData` for an invalid filter, order or cursor, `Internal` on a database error.
pub async fn list(
    conn: &impl DBRunner,
    scope: &AccessScope,
    query: &ODataQuery,
) -> Result<Page<chats::Model>, DomainError> {
    let select = chats::Entity::find()
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope);
    paginate::<ChatField, ChatODataMapper, chats::Entity>(
        select,
        conn,
        query,
        &KeysetSpec {
            default_order: (ChatField::UpdatedAt, SortDir::Desc),
            tiebreaker: (ChatField::Id, SortDir::Desc),
            time_fields: &[ChatField::UpdatedAt],
            limits: LIMITS,
        },
    )
    .await
}

/// Number of non-deleted messages of `chat_id`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn message_count(
    conn: &impl DBRunner,
    scope_tenant: &AccessScope,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let count = messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .filter(messages::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope_tenant)
        .count(conn)
        .await
        .map_err(map_scope_err)?;
    i64::try_from(count).map_err(|_| DomainError::Internal("message count overflow".to_owned()))
}

#[derive(FromQueryResult)]
struct MessageCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Number of non-deleted messages per chat; chats without messages are absent from the map.
///
/// # Errors
/// `Internal` on a database error.
pub async fn message_counts(
    conn: &impl DBRunner,
    scope_tenant: &AccessScope,
    chat_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = messages::Entity::find()
        .filter(messages::Column::ChatId.is_in(chat_ids.iter().copied()))
        .filter(messages::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope_tenant)
        .project_all(conn, |select| {
            select
                .select_only()
                .column(messages::Column::ChatId)
                .column_as(Expr::col(messages::Column::Id).count(), "cnt")
                .group_by(messages::Column::ChatId)
                .into_model::<MessageCount>()
        })
        .await
        .map_err(map_scope_err)?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
}

/// Sets `updated_at = now` on `chat_id`. Used inside the transaction of every operation that
/// counts as chat activity; the caller has already loaded the chat with [`load_scoped`], so the
/// update itself is not narrowed to a scope.
///
/// # Errors
/// `Internal` on a database error.
pub async fn touch_updated_at(
    tx: &impl DBRunner,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(chats::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// Sets the title and `updated_at` of a non-deleted chat; `false` when no row matched.
///
/// # Errors
/// `Internal` on a database error.
pub async fn rename(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    title: &str,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chats::Entity::update_many()
        .col_expr(chats::Column::Title, Expr::value(title))
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(chats::Column::Id.eq(id))
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Soft-deletes a non-deleted chat (`deleted_at = updated_at = now`); `false` when no row matched.
///
/// # Errors
/// `Internal` on a database error.
pub async fn soft_delete(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chats::Entity::update_many()
        .col_expr(chats::Column::DeletedAt, Expr::value(ts::normalize(now)))
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(chats::Column::Id.eq(id))
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Flags the chat's non-deleted attachments that have no cleanup state yet as
/// `cleanup_status = 'pending'`; returns how many rows changed.
///
/// # Errors
/// `Internal` on a database error.
pub async fn mark_attachments_cleanup_pending(
    tx: &impl DBRunner,
    scope_tenant: &AccessScope,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let res = attachments::Entity::update_many()
        .col_expr(
            attachments::Column::CleanupStatus,
            Expr::value(CleanupStatus::Pending.as_str()),
        )
        .col_expr(
            attachments::Column::CleanupUpdatedAt,
            Expr::value(ts::normalize(now)),
        )
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::CleanupStatus.is_null()),
        )
        .secure()
        .scope_with(scope_tenant)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected)
}

/// The chat `id` of `tenant_id` in any state (soft-deleted included), for the background
/// handlers (`scope` is typically `allow_all`).
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_any(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<Option<chats::Model>, DomainError> {
    chats::Entity::find()
        .filter(chats::Column::TenantId.eq(tenant_id))
        .filter(chats::Column::Id.eq(id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}
