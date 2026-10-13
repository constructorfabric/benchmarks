//! Statements on `messages` used by the send pipeline (the snapshot boundary, the prior context
//! size, the recent-history window, inserts), by the turn mutations (soft delete, summary
//! compression reset) and by the thread summary (target frontier, summarized range,
//! compression). Tenant scoped; callers pass a `chat_id` taken from an owner-scoped chat query or
//! a durable task of that chat.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg};
use toolkit_db::secure::SecureUpdateExt as _;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, secure_insert};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use super::keyset::{KeysetField, KeysetRow, KeysetSpec, paginate};
use crate::api::dto::messages::MessageField;
use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::messages::{self, Column};
use crate::infra::db::{MessageRole, ts};

/// Page size of the message list.
const LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Maps the list's `OData` fields to `messages` columns.
pub struct MessageODataMapper;

impl FieldToColumn<MessageField> for MessageODataMapper {
    type Column = Column;

    fn map_field(field: MessageField) -> Column {
        match field {
            MessageField::CreatedAt => Column::CreatedAt,
            MessageField::Id => Column::Id,
            MessageField::Role => Column::Role,
        }
    }
}

impl KeysetField for MessageField {
    fn sort_expr(self) -> Expr {
        Expr::col(MessageODataMapper::map_field(self))
    }
}

impl KeysetRow for messages::Model {
    type Field = MessageField;

    fn sort_value(&self, field: MessageField) -> sea_orm::Value {
        match field {
            MessageField::CreatedAt => self.created_at.into(),
            MessageField::Id => self.id.into(),
            MessageField::Role => self.role.clone().into(),
        }
    }
}

/// A position in the `(created_at, id)` message order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
}

impl Position {
    #[must_use]
    pub fn of(m: &messages::Model) -> Self {
        Self {
            created_at: m.created_at,
            id: m.id,
        }
    }
}

/// `(created_at, id) <= p`.
fn at_or_before(p: Position) -> Condition {
    let at = ts::normalize(p.created_at);
    Condition::any().add(Column::CreatedAt.lt(at)).add(
        Condition::all()
            .add(Column::CreatedAt.eq(at))
            .add(Column::Id.lte(p.id)),
    )
}

/// `(created_at, id) > p`.
fn after(p: Position) -> Condition {
    at_or_before(p).not()
}

/// `(created_at, id) < p`.
fn before(p: Position) -> Condition {
    let at = ts::normalize(p.created_at);
    Condition::any().add(Column::CreatedAt.lt(at)).add(
        Condition::all()
            .add(Column::CreatedAt.eq(at))
            .add(Column::Id.lt(p.id)),
    )
}

/// `base < (created_at, id) <= target`; no lower bound without `base`.
fn in_range(base: Option<Position>, target: Position) -> Condition {
    let mut range = Condition::all().add(at_or_before(target));
    if let Some(base) = base {
        range = range.add(after(base));
    }
    range
}

fn live(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(Column::ChatId.eq(chat_id))
        .add(Column::DeletedAt.is_null())
}

/// Position of the chat's latest non-deleted message (the snapshot boundary).
///
/// # Errors
/// `Internal` on a database error.
pub async fn latest_position(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<Position>, DomainError> {
    latest_position_where(conn, scope, live(chat_id)).await
}

/// Position of the chat's latest non-deleted message that does not belong to turn
/// `request_id` (the snapshot boundary of a retry/edit turn, whose user message is already
/// stored).
///
/// # Errors
/// `Internal` on a database error.
pub async fn latest_position_excluding(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<Position>, DomainError> {
    let window = live(chat_id).add(
        Condition::any()
            .add(Column::RequestId.is_null())
            .add(Column::RequestId.ne(request_id)),
    );
    latest_position_where(conn, scope, window).await
}

/// Position of the chat's latest non-deleted message strictly before `p` (the frozen target of a
/// thread summary, `p` being the causing turn's user message).
///
/// # Errors
/// `Internal` on a database error.
pub async fn latest_before(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    p: Position,
) -> Result<Option<Position>, DomainError> {
    latest_position_where(conn, scope, live(chat_id).add(before(p))).await
}

async fn latest_position_where(
    conn: &impl DBRunner,
    scope: &AccessScope,
    window: Condition,
) -> Result<Option<Position>, DomainError> {
    let latest = messages::Entity::find()
        .filter(window)
        .order_by_desc(Column::CreatedAt)
        .order_by_desc(Column::Id)
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(latest.as_ref().map(Position::of))
}

/// `input_tokens + output_tokens` of the latest non-deleted assistant message with a non-zero
/// count; 0 when there is none.
///
/// # Errors
/// `Internal` on a database error.
pub async fn prior_context_tokens(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let latest = messages::Entity::find()
        .filter(live(chat_id))
        .filter(Column::Role.eq(MessageRole::Assistant.as_str()))
        .filter(
            Condition::any()
                .add(Column::InputTokens.gt(0))
                .add(Column::OutputTokens.gt(0)),
        )
        .order_by_desc(Column::CreatedAt)
        .order_by_desc(Column::Id)
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(latest.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
}

/// The newest `limit` live, uncompressed messages with a `request_id` at or before `boundary`
/// (and after `frontier` when a thread summary exists), in chronological order (DESIGN "Recent
/// messages query").
///
/// # Errors
/// `Internal` on a database error.
pub async fn recent(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    boundary: Position,
    frontier: Option<Position>,
    limit: u64,
) -> Result<Vec<messages::Model>, DomainError> {
    let mut window = live(chat_id)
        .add(Column::RequestId.is_not_null())
        .add(Column::IsCompressed.eq(false))
        .add(at_or_before(boundary));
    if let Some(frontier) = frontier {
        window = window.add(after(frontier));
    }
    let mut rows = messages::Entity::find()
        .filter(window)
        .order_by_desc(Column::CreatedAt)
        .order_by_desc(Column::Id)
        .limit(limit)
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)?;
    rows.reverse();
    Ok(rows)
}

/// The live, uncompressed messages of `chat_id` in `(base, target]`, in `(created_at, id)`
/// order (the range of a thread summary).
///
/// # Errors
/// `Internal` on a database error.
pub async fn summary_range(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    base: Option<Position>,
    target: Position,
) -> Result<Vec<messages::Model>, DomainError> {
    messages::Entity::find()
        .filter(live(chat_id))
        .filter(Column::IsCompressed.eq(false))
        .filter(in_range(base, target))
        .order_by_asc(Column::CreatedAt)
        .order_by_asc(Column::Id)
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}

/// Marks the live messages of `chat_id` in `(base, target]` as compressed (covered by the
/// committed thread summary).
///
/// # Errors
/// `Internal` on a database error.
pub async fn mark_compressed(
    tx: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    base: Option<Position>,
    target: Position,
) -> Result<(), DomainError> {
    messages::Entity::update_many()
        .col_expr(Column::IsCompressed, Expr::value(true))
        .filter(live(chat_id))
        .filter(in_range(base, target))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// Locks the live message `id` of `chat_id` (`FOR UPDATE` on `PostgreSQL`; a no-op on `SQLite`,
/// where the write transaction itself serializes) and returns it; `None` when it is missing or
/// soft-deleted.
///
/// # Errors
/// `Internal` on a database error.
pub async fn lock_live(
    tx: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<messages::Model>, DomainError> {
    messages::Entity::find()
        .filter(live(chat_id))
        .filter(Column::Id.eq(id))
        .lock_exclusive()
        .secure()
        .scope_with(scope)
        .one(tx)
        .await
        .map_err(map_scope_err)
}

/// The live message `id` of `chat_id`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn find(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<messages::Model>, DomainError> {
    messages::Entity::find()
        .filter(live(chat_id))
        .filter(Column::Id.eq(id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// The live `role` message of turn `request_id` of `chat_id`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_of_turn(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    role: MessageRole,
) -> Result<Option<messages::Model>, DomainError> {
    messages::Entity::find()
        .filter(live(chat_id))
        .filter(Column::RequestId.eq(request_id))
        .filter(Column::Role.eq(role.as_str()))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Soft-deletes the live messages of turn `request_id` of `chat_id`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn soft_delete_of_turn(
    tx: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    messages::Entity::update_many()
        .col_expr(Column::DeletedAt, Expr::value(Some(ts::normalize(now))))
        .filter(live(chat_id))
        .filter(Column::RequestId.eq(request_id))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// Clears `is_compressed` on every message of `chat_id` (its thread summary was dropped).
///
/// # Errors
/// `Internal` on a database error.
pub async fn clear_compressed(
    tx: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<(), DomainError> {
    messages::Entity::update_many()
        .col_expr(Column::IsCompressed, Expr::value(false))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::IsCompressed.eq(true))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// Inserts a message. A second live message with the same `(chat_id, request_id, role)` is a
/// `Conflict{unique_violation}`.
///
/// # Errors
/// `Conflict`, `AccessDenied` outside `scope`, `Internal` on a database error.
pub async fn insert(
    tx: &impl DBRunner,
    scope: &AccessScope,
    message: messages::ActiveModel,
) -> Result<(), DomainError> {
    secure_insert::<messages::Entity>(message, scope, tx)
        .await
        .map(drop)
        .map_err(map_scope_err)
}

/// One page of the live messages of `chat_id` (tenant scoped; `chat_id` comes from an
/// owner-scoped chat query). Without `$orderby` and cursor the order is `created_at asc, id asc`
/// (chronological); no sort key is nullable.
///
/// # Errors
/// `OData` for an invalid filter, order or cursor, `Internal` on a database error.
pub async fn list(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<messages::Model>, DomainError> {
    let select = messages::Entity::find()
        .filter(live(chat_id))
        .secure()
        .scope_with(scope);
    paginate::<MessageField, MessageODataMapper, messages::Entity>(
        select,
        conn,
        query,
        &KeysetSpec {
            default_order: (MessageField::CreatedAt, SortDir::Asc),
            tiebreaker: (MessageField::Id, SortDir::Asc),
            time_fields: &[MessageField::CreatedAt],
            limits: LIMITS,
        },
    )
    .await
}
