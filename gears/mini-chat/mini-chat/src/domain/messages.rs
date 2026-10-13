//! Message list service (DESIGN §3.3 "List Messages", §3.7 messages / `message_attachments`).

use std::collections::HashMap;

use sea_orm::{ColumnTrait, Condition, EntityTrait, JoinType, QueryFilter, QueryOrder, QuerySelect, RelationDef};
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, ODataValue};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::chats::{LIST_LIMITS, load_chat, map_timestamp_value, text_timestamps, with_default_order};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{attachment, message, message_attachment, message_reaction};

/// Image thumbnail of an attachment summary (WebP bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

/// `AttachmentSummary` of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    /// `document` | `image`
    pub kind: String,
    pub filename: String,
    /// `pending` | `uploaded` | `ready` | `failed`
    pub status: String,
    /// Present only for ready images with a stored thumbnail.
    pub thumbnail: Option<Thumbnail>,
}

/// A listed message with its batch-enriched fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub message: message::Model,
    /// Never null (a stored NULL fails the request).
    pub request_id: Uuid,
    pub attachments: Vec<AttachmentSummary>,
    /// The caller's reaction (`like` | `dislike`); always `None` for non-assistant messages.
    pub my_reaction: Option<String>,
}

/// `GET /chats/{id}/messages`: the chat's non-deleted messages, `OData` filter/order/cursor,
/// default `created_at asc, id asc`.
///
/// # Errors
/// Authz errors, 404 Chat, `OData` errors (400), NULL `request_id` (500), DB errors.
pub async fn list_messages(
    app: &AppServices,
    ctx: &SecurityContext,
    chat_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<MessageView>, DomainError> {
    let scope = app.authz.chat_scope(ctx, "list_messages", Some(chat_id)).await?;
    let chat = load_chat(app, &scope, chat_id).await?;
    let tenant_scope = AccessScope::for_tenant(chat.tenant_id);
    let query = with_default_order(query, &[("created_at", SortDir::Asc), ("id", SortDir::Asc)]);
    let conn = app.db.conn()?;
    let select = message::Entity::find()
        .secure()
        .scope_with(&tenant_scope)
        .filter(Condition::all().add(message::Column::ChatId.eq(chat.id)).add(message::Column::DeletedAt.is_null()));
    let tiebreaker = ("id", SortDir::Asc);
    let page = if text_timestamps(app) {
        paginate_odata::<MessageField, MessageMapper<true>, _, _, _, _>(select, &conn, &query, tiebreaker, LIST_LIMITS, |m| m)
            .await?
    } else {
        paginate_odata::<MessageField, MessageMapper<false>, _, _, _, _>(select, &conn, &query, tiebreaker, LIST_LIMITS, |m| m)
            .await?
    };

    let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
    let mut attachments = attachment_summaries(&conn, chat.tenant_id, chat.id, &ids).await?;
    let assistant_ids: Vec<Uuid> = page.items.iter().filter(|m| m.role == "assistant").map(|m| m.id).collect();
    let reactions = user_reactions(&conn, chat.tenant_id, ctx.subject_id(), &assistant_ids).await?;

    let mut items = Vec::with_capacity(page.items.len());
    for m in page.items {
        let request_id = m.request_id.ok_or_else(|| {
            DomainError::internal(format!("message {} of chat {} has a NULL request_id", m.id, m.chat_id))
        })?;
        let my_reaction = if m.role == "assistant" { reactions.get(&m.id).cloned() } else { None };
        let message_attachments = attachments.remove(&m.id).unwrap_or_default();
        items.push(MessageView { message: m, request_id, attachments: message_attachments, my_reaction });
    }
    Ok(Page { items, page_info: page.page_info })
}

/// `message_attachments.attachment_id -> attachments.id` (the entities declare no relations).
fn attachment_join() -> RelationDef {
    message_attachment::Entity::belongs_to(attachment::Entity)
        .from(message_attachment::Column::AttachmentId)
        .to(attachment::Column::Id)
        .into()
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct AttachmentRow {
    message_id: Uuid,
    attachment_id: Uuid,
    attachment_kind: String,
    filename: String,
    status: String,
    img_thumbnail: Option<Vec<u8>>,
    img_thumbnail_width: Option<i32>,
    img_thumbnail_height: Option<i32>,
}

/// One batch query: `message_attachments ⋈ attachments` (non-deleted attachments) for the
/// page's messages, grouped by message id in association order.
async fn attachment_summaries(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<AttachmentSummary>>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_attachment::Entity::find()
        .join(JoinType::InnerJoin, attachment_join())
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.is_in(message_ids.to_vec()))
                .add(attachment::Column::TenantId.eq(tenant_id))
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .order_by_asc(message_attachment::Column::CreatedAt)
        .order_by_asc(message_attachment::Column::AttachmentId)
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .project_all(runner, |q| {
            q.select_only()
                .column(message_attachment::Column::MessageId)
                .column_as(attachment::Column::Id, "attachment_id")
                .column(attachment::Column::AttachmentKind)
                .column(attachment::Column::Filename)
                .column(attachment::Column::Status)
                .column(attachment::Column::ImgThumbnail)
                .column(attachment::Column::ImgThumbnailWidth)
                .column(attachment::Column::ImgThumbnailHeight)
                .into_model::<AttachmentRow>()
        })
        .await?;
    let mut out: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
    for r in rows {
        let thumbnail = match (r.attachment_kind.as_str(), r.status.as_str(), r.img_thumbnail, r.img_thumbnail_width, r.img_thumbnail_height)
        {
            ("image", "ready", Some(data), Some(width), Some(height)) => Some(Thumbnail { data, width, height }),
            _ => None,
        };
        out.entry(r.message_id).or_default().push(AttachmentSummary {
            attachment_id: r.attachment_id,
            kind: r.attachment_kind,
            filename: r.filename,
            status: r.status,
            thumbnail,
        });
    }
    Ok(out)
}

/// Batch lookup of the caller's reactions on the given messages.
async fn user_reactions(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, String>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_reaction::Entity::find()
        .filter(
            Condition::all()
                .add(message_reaction::Column::MessageId.is_in(message_ids.to_vec()))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id).ensure_owner(user_id))
        .all(runner)
        .await?;
    Ok(rows.into_iter().map(|r| (r.message_id, r.reaction)).collect())
}

// ───────────────────────────── OData mapping ─────────────────────────────

/// `OData` fields of `GET /chats/{id}/messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageField {
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

/// Column mapping of `MessageField`; `TEXT_TS` selects the SQLite timestamp binding.
pub struct MessageMapper<const TEXT_TS: bool>;

impl<const TEXT_TS: bool> FieldToColumn<MessageField> for MessageMapper<TEXT_TS> {
    type Column = message::Column;

    fn map_field(field: MessageField) -> message::Column {
        match field {
            MessageField::CreatedAt => message::Column::CreatedAt,
            MessageField::Id => message::Column::Id,
            MessageField::Role => message::Column::Role,
        }
    }

    fn map_value(_field: MessageField, _op: FilterOp, value: &ODataValue) -> Result<ODataValue, String> {
        Ok(map_timestamp_value(TEXT_TS, value))
    }
}

impl<const TEXT_TS: bool> ODataFieldMapping<MessageField> for MessageMapper<TEXT_TS> {
    type Entity = message::Entity;

    fn extract_cursor_value(model: &message::Model, field: MessageField) -> sea_orm::Value {
        match field {
            MessageField::CreatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.created_at)),
            MessageField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MessageField::Role => sea_orm::Value::String(Some(model.role.clone())),
        }
    }
}
