//! Chat CRUD service (DESIGN §3.3 "Create/List/Get/Update/Delete Chat", §3.8, §4 "Cleanup on Chat Deletion").
//!
//! CONTRACT: `load_chat`, `load_chat_tx` and `touch_chat` are used by other services; their
//! signatures are fixed. The rest is implemented by the CRUD work package.

use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, ODataValue};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::error::{DomainError, Resource};
use crate::domain::models::invalid_model;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{attachment, chat, message};
use crate::infra::outbox::PAYLOAD_CHAT_CLEANUP;
use crate::infra::outbox::payloads::ChatCleanupPayload;

/// Page size limits of the list endpoints (default 20, larger values clamped to 100).
pub const LIST_LIMITS: LimitCfg = LimitCfg { default: 20, max: 100 };

/// Maximum chat title length (characters, after trim).
pub const MAX_TITLE_CHARS: usize = 255;

/// Loads a non-deleted chat under `scope` (404 Chat when missing, foreign or deleted).
///
/// # Errors
/// `NotFound` or DB errors.
pub async fn load_chat(app: &AppServices, scope: &AccessScope, chat_id: Uuid) -> Result<chat::Model, DomainError> {
    let conn = app.db.conn()?;
    find_chat(&conn, scope, chat_id).await
}

/// Same as `load_chat` inside a transaction.
///
/// # Errors
/// `NotFound` or DB errors.
pub async fn load_chat_tx(tx: &DbTx<'_>, scope: &AccessScope, chat_id: Uuid) -> Result<chat::Model, DomainError> {
    find_chat(tx, scope, chat_id).await
}

/// Bumps `chats.updated_at` (activity ordering).
///
/// # Errors
/// DB errors.
pub async fn touch_chat(tx: &DbTx<'_>, tenant_id: Uuid, chat_id: Uuid, now: OffsetDateTime) -> Result<(), DomainError> {
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(tx)
        .await?;
    Ok(())
}

async fn find_chat(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid) -> Result<chat::Model, DomainError> {
    chat::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)).add(chat::Column::DeletedAt.is_null()))
        .one(runner)
        .await?
        .ok_or_else(|| DomainError::not_found(Resource::Chat, chat_id))
}

// ───────────────────────────── CRUD service ─────────────────────────────

/// A chat with its non-deleted message count (`ChatDetail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatView {
    pub chat: chat::Model,
    pub message_count: i64,
}

/// Trims and validates a chat title (1–255 characters after trim, else 400 `INVALID_TITLE`).
///
/// # Errors
/// `InvalidArgument` (`title` / `INVALID_TITLE`).
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    if len == 0 || len > MAX_TITLE_CHARS {
        return Err(DomainError::invalid(
            Resource::Chat,
            "title",
            "INVALID_TITLE",
            format!("title must be 1-{MAX_TITLE_CHARS} characters after trimming"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// `POST /chats`: title validation, authorization, model resolution, insert.
///
/// # Errors
/// `INVALID_TITLE`, authz errors, `INVALID_MODEL`, policy / DB errors.
pub async fn create_chat(
    app: &AppServices,
    ctx: &SecurityContext,
    title: Option<String>,
    model: Option<String>,
) -> Result<ChatView, DomainError> {
    let title = title.as_deref().map(validate_title).transpose()?;
    let scope = app.authz.chat_scope(ctx, "create", None).await?;
    let snapshot = app.policy.current_snapshot(ctx.subject_id()).await?;
    let model_id = match model {
        Some(m) => snapshot.enabled_model(&m).map(|e| e.id.clone()).ok_or_else(|| invalid_model(&m))?,
        None => snapshot.default_model().map(|e| e.id.clone()).ok_or_else(|| invalid_model(""))?,
    };
    let now = clock::now();
    let am = chat::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(ctx.subject_tenant_id()),
        user_id: Set(ctx.subject_id()),
        model: Set(model_id),
        title: Set(title),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    };
    let conn = app.db.conn()?;
    let chat = secure_insert::<chat::Entity>(am, &scope, &conn).await?;
    Ok(ChatView { chat, message_count: 0 })
}

/// `GET /chats/{id}`.
///
/// # Errors
/// Authz errors, 404 Chat, DB errors.
pub async fn get_chat(app: &AppServices, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
    let scope = app.authz.chat_scope(ctx, "read", Some(chat_id)).await?;
    let conn = app.db.conn()?;
    let chat = find_chat(&conn, &scope, chat_id).await?;
    let message_count = message_count(&conn, chat.tenant_id, chat.id).await?;
    Ok(ChatView { chat, message_count })
}

/// `PATCH /chats/{id}`: renames the chat (only `title`), bumps `updated_at`.
///
/// # Errors
/// `INVALID_TITLE`, authz errors, 404 Chat, DB errors.
pub async fn update_chat_title(
    app: &AppServices,
    ctx: &SecurityContext,
    chat_id: Uuid,
    title: &str,
) -> Result<ChatView, DomainError> {
    let title = validate_title(title)?;
    let scope = app.authz.chat_scope(ctx, "update", Some(chat_id)).await?;
    let now = clock::now();
    let conn = app.db.conn()?;
    let res = chat::Entity::update_many()
        .col_expr(chat::Column::Title, Expr::value(title))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)).add(chat::Column::DeletedAt.is_null()))
        .secure()
        .scope_with(&scope)
        .exec(&conn)
        .await?;
    if res.rows_affected == 0 {
        return Err(DomainError::not_found(Resource::Chat, chat_id));
    }
    let chat = find_chat(&conn, &scope, chat_id).await?;
    let message_count = message_count(&conn, chat.tenant_id, chat.id).await?;
    Ok(ChatView { chat, message_count })
}

/// `DELETE /chats/{id}`: in one transaction soft-deletes the chat, marks its attachments
/// `cleanup_status = pending` and enqueues the chat-cleanup outbox message. A running turn
/// is not cancelled.
///
/// # Errors
/// Authz errors, 404 Chat (also on a second delete), outbox / DB errors.
pub async fn delete_chat(app: &AppServices, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
    let scope = app.authz.chat_scope(ctx, "delete", Some(chat_id)).await?;
    let queue_name = app.outbox.chat_cleanup_queue().to_owned();
    let wake = crate::domain::tx::retry_contention(|| {
        let outbox = std::sync::Arc::clone(&app.outbox);
        let queue = queue_name.clone();
        let scope = scope.clone();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                let chat = find_chat(tx, &scope, chat_id).await?;
                let now = clock::now();
                let res = chat::Entity::update_many()
                    .col_expr(chat::Column::DeletedAt, Expr::value(now))
                    .col_expr(chat::Column::UpdatedAt, Expr::value(now))
                    .filter(Condition::all().add(chat::Column::Id.eq(chat_id)).add(chat::Column::DeletedAt.is_null()))
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await?;
                if res.rows_affected == 0 {
                    return Err(DomainError::not_found(Resource::Chat, chat_id));
                }
                attachment::Entity::update_many()
                    .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
                    .filter(
                        Condition::all()
                            .add(attachment::Column::ChatId.eq(chat_id))
                            .add(attachment::Column::DeletedAt.is_null())
                            .add(attachment::Column::CleanupStatus.is_null()),
                    )
                    .secure()
                    .scope_with(&AccessScope::for_tenant(chat.tenant_id))
                    .exec(tx)
                    .await?;
                let payload = ChatCleanupPayload {
                    tenant_id: chat.tenant_id,
                    chat_id,
                    system_request_id: Uuid::new_v4(),
                    reason: "chat_soft_delete".to_owned(),
                    chat_deleted_at: now,
                };
                outbox.enqueue_json(tx, &queue, chat_id, PAYLOAD_CHAT_CLEANUP, &payload).await
            })
        })
    })
    .await?;
    wake.fire();
    Ok(())
}

/// `GET /chats`: the caller's non-deleted chats, `OData` filter/order/cursor, default
/// `updated_at desc, id desc`.
///
/// # Errors
/// Authz errors, `OData` errors (400), DB errors.
pub async fn list_chats(app: &AppServices, ctx: &SecurityContext, query: &ODataQuery) -> Result<Page<ChatView>, DomainError> {
    let scope = app.authz.chat_scope(ctx, "list", None).await?;
    let query = with_default_order(query, &[("updated_at", SortDir::Desc), ("id", SortDir::Desc)]);
    let conn = app.db.conn()?;
    let select = chat::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(chat::Column::DeletedAt.is_null()));
    let tiebreaker = ("id", SortDir::Desc);
    let page = if text_timestamps(app) {
        paginate_odata::<ChatField, ChatMapper<true>, _, _, _, _>(select, &conn, &query, tiebreaker, LIST_LIMITS, |m| m)
            .await?
    } else {
        paginate_odata::<ChatField, ChatMapper<false>, _, _, _, _>(select, &conn, &query, tiebreaker, LIST_LIMITS, |m| m)
            .await?
    };
    let mut by_tenant: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for c in &page.items {
        by_tenant.entry(c.tenant_id).or_default().push(c.id);
    }
    let mut counts = HashMap::new();
    for (tenant, ids) in by_tenant {
        counts.extend(message_counts(&conn, tenant, ids).await?);
    }
    Ok(page.map_items(|chat| {
        let message_count = counts.get(&chat.id).copied().unwrap_or(0);
        ChatView { chat, message_count }
    }))
}

/// Number of non-deleted messages of a chat.
///
/// # Errors
/// DB errors.
pub async fn message_count(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
    Ok(message_counts(runner, tenant_id, vec![chat_id]).await?.get(&chat_id).copied().unwrap_or(0))
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Batch non-deleted message counts for chats of one tenant.
///
/// # Errors
/// DB errors.
pub async fn message_counts(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_ids: Vec<Uuid>,
) -> Result<HashMap<Uuid, i64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message::Entity::find()
        .filter(Condition::all().add(message::Column::ChatId.is_in(chat_ids)).add(message::Column::DeletedAt.is_null()))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .project_all(runner, |q| {
            q.select_only()
                .column(message::Column::ChatId)
                .expr_as(sea_orm::sea_query::Func::count(Expr::col(message::Column::Id)), "cnt")
                .group_by(message::Column::ChatId)
                .into_model::<ChatCount>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
}

// ───────────────────────────── OData helpers ─────────────────────────────

/// `true` when timestamps are stored as text (SQLite) and `OData` timestamp filter values must
/// be bound in the stored text format.
pub(crate) fn text_timestamps(app: &AppServices) -> bool {
    matches!(app.db.db().backend(), sea_orm::DbBackend::Sqlite)
}

/// Applies the endpoint's default order when the request has neither `$orderby` nor a cursor.
pub(crate) fn with_default_order(query: &ODataQuery, default: &[(&str, SortDir)]) -> ODataQuery {
    let mut q = query.clone();
    if q.cursor.is_none() && q.order.is_empty() {
        q.order = ODataOrderBy(default.iter().map(|(f, d)| OrderKey { field: (*f).to_owned(), dir: *d }).collect());
    }
    q
}

/// Maps an `OData` timestamp filter value to the stored representation.
///
/// On SQLite `OffsetDateTime` columns hold RFC 3339 text with a 9-digit fraction and `Z`
/// (see `clock`), while the toolkit binds `OData` timestamps as chrono values (`+00:00`, variable
/// fraction) which compare wrongly as text. Binding the same 9-digit `Z` text makes text order
/// equal time order. Other backends keep the typed value.
pub(crate) fn map_timestamp_value(text: bool, value: &ODataValue) -> ODataValue {
    match value {
        ODataValue::DateTime(dt) if text => ODataValue::String(dt.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()),
        other => other.clone(),
    }
}

/// `OData` fields of `GET /chats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatField {
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

/// Column mapping of `ChatField`; `TEXT_TS` selects the SQLite timestamp binding.
pub struct ChatMapper<const TEXT_TS: bool>;

impl<const TEXT_TS: bool> FieldToColumn<ChatField> for ChatMapper<TEXT_TS> {
    type Column = chat::Column;

    fn map_field(field: ChatField) -> chat::Column {
        match field {
            ChatField::UpdatedAt => chat::Column::UpdatedAt,
            ChatField::Id => chat::Column::Id,
            ChatField::Title => chat::Column::Title,
        }
    }

    fn map_value(_field: ChatField, _op: FilterOp, value: &ODataValue) -> Result<ODataValue, String> {
        Ok(map_timestamp_value(TEXT_TS, value))
    }
}

impl<const TEXT_TS: bool> ODataFieldMapping<ChatField> for ChatMapper<TEXT_TS> {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: ChatField) -> sea_orm::Value {
        match field {
            ChatField::UpdatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatField::Id => sea_orm::Value::Uuid(Some(model.id)),
            // Untitled chats sort as the empty string in cursors (a NULL key cannot be encoded).
            ChatField::Title => sea_orm::Value::String(Some(model.title.clone().unwrap_or_default())),
        }
    }
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
