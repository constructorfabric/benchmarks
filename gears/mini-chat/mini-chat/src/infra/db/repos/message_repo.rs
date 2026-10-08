//! `messages` queries. Messages are children of a chat: callers authorize the
//! chat first and pass its tenant and id. Every read excludes soft-deleted
//! messages.

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Condition, Expr, OnConflict};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, FromQueryResult, Order, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{
    DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
    secure_insert,
};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use uuid::Uuid;

use super::tenant_scope;
use crate::domain::enums::MessageRole;
use crate::domain::error::DomainError;
use crate::infra::db::entities::{message, thread_summary};

/// `GET /v1/chats/{id}/messages` page size: default 20, clamped to 100.
pub const MESSAGE_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// `$filter` / `$orderby` fields of `GET /v1/chats/{id}/messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageFilterField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageFilterField {
    const FIELDS: &'static [Self] = &[Self::CreatedAt, Self::Id, Self::Role];

    fn name(&self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Id => "id",
            Self::Role => "role",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::CreatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Role => FieldKind::String,
        }
    }
}

pub struct MessageMapper;

impl FieldToColumn<MessageFilterField> for MessageMapper {
    type Column = message::Column;

    fn map_field(field: MessageFilterField) -> message::Column {
        match field {
            MessageFilterField::CreatedAt => message::Column::CreatedAt,
            MessageFilterField::Id => message::Column::Id,
            MessageFilterField::Role => message::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageFilterField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(model: &message::Model, field: MessageFilterField) -> sea_orm::Value {
        match field {
            MessageFilterField::CreatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.created_at))
            }
            MessageFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MessageFilterField::Role => sea_orm::Value::String(Some(model.role.clone())),
        }
    }
}

/// A message row to insert (`content_type = "text"`, not compressed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRole,
    pub content: String,
    pub request_kind: String,
    pub features_used: serde_json::Value,
    pub provider_response_id: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub model: Option<String>,
    pub created_at: OffsetDateTime,
}

fn live_in_chat(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
}

/// Inserts a message.
///
/// # Errors
/// Database failure (including the `(chat_id, request_id, role)` unique index).
pub async fn insert(
    runner: &impl DBRunner,
    new: NewMessage,
) -> Result<message::Model, DomainError> {
    let am = message::ActiveModel {
        id: Set(new.id),
        tenant_id: Set(new.tenant_id),
        chat_id: Set(new.chat_id),
        request_id: Set(Some(new.request_id)),
        role: Set(new.role.as_str().to_owned()),
        content: Set(new.content),
        content_type: Set("text".to_owned()),
        // Reserved column: always written as 0 (DESIGN section 3.7).
        token_estimate: Set(0),
        provider_response_id: Set(new.provider_response_id),
        request_kind: Set(new.request_kind),
        features_used: Set(new.features_used),
        input_tokens: Set(new.input_tokens),
        output_tokens: Set(new.output_tokens),
        cache_read_input_tokens: Set(new.cache_read_input_tokens),
        cache_write_input_tokens: Set(new.cache_write_input_tokens),
        reasoning_tokens: Set(new.reasoning_tokens),
        model: Set(new.model),
        is_compressed: Set(false),
        created_at: Set(new.created_at),
        deleted_at: Set(None),
    };
    Ok(secure_insert::<message::Entity>(am, &tenant_scope(new.tenant_id), runner).await?)
}

/// The live message `msg_id` of the chat.
///
/// # Errors
/// Database failure.
pub async fn find_live(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    msg_id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(live_in_chat(chat_id))
        .filter(message::Column::Id.eq(msg_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

#[derive(Debug, FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    n: i64,
}

/// Live message count per chat (chats without messages are absent).
///
/// # Errors
/// Database failure.
pub async fn count_live_by_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message::Entity::find()
        .filter(message::Column::ChatId.is_in(chat_ids.iter().copied()))
        .filter(message::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .project_all(runner, |q| {
            q.select_only()
                .column(message::Column::ChatId)
                .column_as(message::Column::Id.count(), "n")
                .group_by(message::Column::ChatId)
                .into_model::<ChatCount>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.n)).collect())
}

/// Page of live messages of the chat. Default order `created_at asc`,
/// tiebreaker `id asc`.
///
/// # Errors
/// `Query` for a bad `OData` query or a pagination failure.
pub async fn list_page(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    mut query: ODataQuery,
) -> Result<Page<message::Model>, DomainError> {
    if query.cursor.is_none() && query.order.is_empty() {
        query.order = ODataOrderBy(vec![OrderKey {
            field: MessageFilterField::CreatedAt.name().to_owned(),
            dir: SortDir::Asc,
        }]);
    }
    let select = message::Entity::find()
        .filter(live_in_chat(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id));
    Ok(
        paginate_odata::<MessageFilterField, MessageMapper, message::Entity, message::Model, _, _>(
            select,
            runner,
            &query,
            (MessageFilterField::Id.name(), SortDir::Asc),
            MESSAGE_LIMITS,
            |m| m,
        )
        .await?,
    )
}

/// `(input_tokens, output_tokens)` of the latest live assistant message that
/// carries provider usage (either count above zero), ignoring the turn
/// `exclude_request_id` (the turn a retry/edit replaces).
///
/// # Errors
/// Database failure.
pub async fn latest_assistant_with_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    exclude_request_id: Option<Uuid>,
) -> Result<Option<(i64, i64)>, DomainError> {
    let mut select = message::Entity::find().filter(live_in_chat(chat_id));
    if let Some(rid) = exclude_request_id {
        select = select.filter(
            Condition::any()
                .add(message::Column::RequestId.is_null())
                .add(message::Column::RequestId.ne(rid)),
        );
    }
    let row = select
        .filter(message::Column::Role.eq(MessageRole::Assistant.as_str()))
        .filter(
            Condition::any()
                .add(message::Column::InputTokens.gt(0))
                .add(message::Column::OutputTokens.gt(0)),
        )
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(row.map(|m| (m.input_tokens, m.output_tokens)))
}

/// `(created_at, id)` of the latest live message: the snapshot boundary of a
/// turn's context.
///
/// # Errors
/// Database failure.
pub async fn snapshot_boundary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<(OffsetDateTime, Uuid)>, DomainError> {
    let row = message::Entity::find()
        .filter(live_in_chat(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(row.map(|m| (m.created_at, m.id)))
}

/// `(created_at, id) <= key` (`inclusive`) or `< key`.
fn before(key: (OffsetDateTime, Uuid), inclusive: bool) -> Condition {
    let (at, id) = key;
    let id_cond = if inclusive {
        message::Column::Id.lte(id)
    } else {
        message::Column::Id.lt(id)
    };
    Condition::any().add(message::Column::CreatedAt.lt(at)).add(
        Condition::all()
            .add(message::Column::CreatedAt.eq(at))
            .add(id_cond),
    )
}

/// `(created_at, id) > key`.
fn after(key: (OffsetDateTime, Uuid)) -> Condition {
    let (at, id) = key;
    Condition::any().add(message::Column::CreatedAt.gt(at)).add(
        Condition::all()
            .add(message::Column::CreatedAt.eq(at))
            .add(message::Column::Id.gt(id)),
    )
}

/// The latest `limit` live messages in `(frontier, boundary]` (keyset on
/// `(created_at, id)`), returned in chronological order. Like the DESIGN
/// "Recent messages query", messages without a `request_id` and compressed
/// messages are skipped.
///
/// # Errors
/// Database failure.
pub async fn recent_for_context(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    boundary: (OffsetDateTime, Uuid),
    frontier: Option<(OffsetDateTime, Uuid)>,
    limit: u64,
) -> Result<Vec<message::Model>, DomainError> {
    let mut window = Condition::all().add(before(boundary, true));
    if let Some(f) = frontier {
        window = window.add(after(f));
    }
    let mut rows = message::Entity::find()
        .filter(live_in_chat(chat_id))
        .filter(message::Column::RequestId.is_not_null())
        .filter(message::Column::IsCompressed.eq(false))
        .filter(window)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(limit)
        .all(runner)
        .await?;
    rows.reverse();
    Ok(rows)
}

/// The live user message of the turn `request_id`.
///
/// # Errors
/// Database failure.
pub async fn find_turn_user_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(live_in_chat(chat_id))
        .filter(message::Column::RequestId.eq(request_id))
        .filter(message::Column::Role.eq(MessageRole::User.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// `(created_at, id)` of the latest live message strictly before `key`
/// (keyset on `(created_at, id)`) that does not belong to the turn
/// `exclude_request_id`: the frozen target of a thread summary.
///
/// # Errors
/// Database failure.
pub async fn latest_live_before(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    key: (OffsetDateTime, Uuid),
    exclude_request_id: Uuid,
) -> Result<Option<(OffsetDateTime, Uuid)>, DomainError> {
    let row = message::Entity::find()
        .filter(live_in_chat(chat_id))
        .filter(before(key, false))
        .filter(
            Condition::any()
                .add(message::Column::RequestId.is_null())
                .add(message::Column::RequestId.ne(exclude_request_id)),
        )
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(row.map(|m| (m.created_at, m.id)))
}

/// The chat's thread summary frontier `(summarized_up_to_created_at,
/// summarized_up_to_message_id)`, if a summary exists.
///
/// # Errors
/// Database failure.
pub async fn summary_frontier(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<(OffsetDateTime, Uuid)>, DomainError> {
    let row = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?;
    Ok(row.map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id)))
}

/// The chat's thread summary row, if any.
///
/// # Errors
/// Database failure.
pub async fn thread_summary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<thread_summary::Model>, DomainError> {
    Ok(thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// Soft-deletes the live messages of the turn `request_id` (turn mutation).
/// Returns the affected count.
///
/// # Errors
/// Database failure.
pub async fn soft_delete_turn(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(Some(now)))
        .filter(live_in_chat(chat_id))
        .filter(message::Column::RequestId.eq(request_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Clears `is_compressed` on every message of the chat (the thread summary
/// was deleted). Returns the affected count.
///
/// # Errors
/// Database failure.
pub async fn clear_compressed(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<u64, DomainError> {
    Ok(message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(false))
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::IsCompressed.eq(true))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Deletes the chat's thread summary row. Returns the affected count.
///
/// # Errors
/// Database failure.
pub async fn delete_thread_summary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<u64, DomainError> {
    Ok(thread_summary::Entity::delete_many()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// The thread summary range: live, non-compressed messages of the chat in
/// `(base, target]` (keyset on `(created_at, id)`; no lower bound without a
/// base), ordered by `(created_at, id)` ascending.
///
/// # Errors
/// Database failure.
pub async fn summary_range(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> Result<Vec<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(live_in_chat(chat_id))
        .filter(message::Column::IsCompressed.eq(false))
        .filter(range_cond(base, target))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .all(runner)
        .await?)
}

fn range_cond(base: Option<(OffsetDateTime, Uuid)>, target: (OffsetDateTime, Uuid)) -> Condition {
    let mut cond = Condition::all().add(before(target, true));
    if let Some(b) = base {
        cond = cond.add(after(b));
    }
    cond
}

/// Guarded no-op write on the live message `msg_id` (thread summary commit):
/// takes the row lock (the `SQLite` write lock) and checks it is not
/// soft-deleted. Returns the affected count (0 = deleted or missing).
///
/// # Errors
/// Database failure.
pub async fn touch_live(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    msg_id: Uuid,
) -> Result<u64, DomainError> {
    Ok(message::Entity::update_many()
        .col_expr(
            message::Column::IsCompressed,
            Expr::col(message::Column::IsCompressed),
        )
        .filter(live_in_chat(chat_id))
        .filter(message::Column::Id.eq(msg_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Sets `is_compressed = true` on the live, non-compressed messages in
/// `(base, target]`. Returns the affected count.
///
/// # Errors
/// Database failure.
pub async fn mark_range_compressed(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> Result<u64, DomainError> {
    Ok(message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(live_in_chat(chat_id))
        .filter(message::Column::IsCompressed.eq(false))
        .filter(range_cond(base, target))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// A summary to store (thread summary commit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryWrite {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub summary_text: String,
    pub frontier: (OffsetDateTime, Uuid),
    pub token_estimate: i32,
    pub now: OffsetDateTime,
}

/// Inserts the chat's first summary unless a row already exists (unique
/// `chat_id`). Returns whether the row was inserted.
///
/// # Errors
/// Database failure.
pub async fn insert_summary_if_absent(
    runner: &impl DBRunner,
    w: &SummaryWrite,
) -> Result<bool, DomainError> {
    let am = thread_summary::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(w.tenant_id),
        chat_id: Set(w.chat_id),
        summary_text: Set(w.summary_text.clone()),
        summarized_up_to_created_at: Set(w.frontier.0),
        summarized_up_to_message_id: Set(w.frontier.1),
        token_estimate: Set(w.token_estimate),
        created_at: Set(w.now),
        updated_at: Set(w.now),
    };
    let scope = tenant_scope(w.tenant_id);
    // Untargeted DO NOTHING (any unique key, here `chat_id`).
    let on_conflict = OnConflict::new()
        .do_nothing_on([thread_summary::Column::Id])
        .to_owned();
    match thread_summary::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await
    {
        Ok(_) => Ok(true),
        Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Replaces the chat's summary only if its frontier still equals `base`
/// (compare-and-set). Returns the affected count (0 = CAS lost).
///
/// # Errors
/// Database failure.
pub async fn cas_update_summary(
    runner: &impl DBRunner,
    w: &SummaryWrite,
    base: (OffsetDateTime, Uuid),
) -> Result<u64, DomainError> {
    Ok(thread_summary::Entity::update_many()
        .col_expr(
            thread_summary::Column::SummaryText,
            Expr::value(w.summary_text.clone()),
        )
        .col_expr(
            thread_summary::Column::SummarizedUpToCreatedAt,
            Expr::value(w.frontier.0),
        )
        .col_expr(
            thread_summary::Column::SummarizedUpToMessageId,
            Expr::value(w.frontier.1),
        )
        .col_expr(
            thread_summary::Column::TokenEstimate,
            Expr::value(w.token_estimate),
        )
        .col_expr(thread_summary::Column::UpdatedAt, Expr::value(w.now))
        .filter(thread_summary::Column::ChatId.eq(w.chat_id))
        .filter(thread_summary::Column::SummarizedUpToCreatedAt.eq(base.0))
        .filter(thread_summary::Column::SummarizedUpToMessageId.eq(base.1))
        .secure()
        .scope_with(&tenant_scope(w.tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}
