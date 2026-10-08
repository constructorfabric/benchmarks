//! Message history (`GET /v1/chats/{id}/messages`).

use std::collections::HashMap;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::Core;
use super::chats::{PAGE_LIMITS, tenant_scope};
use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::infra::db::entities::{attachment, message, message_attachment, reaction};

/// Thumbnail projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// `AttachmentSummary` projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryView {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub img_thumbnail: Option<Thumbnail>,
}

#[must_use]
pub fn thumbnail_of(a: &attachment::Model) -> Option<Thumbnail> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let data = a.img_thumbnail.clone()?;
    Some(Thumbnail {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data,
    })
}

/// `Message` projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryView>,
    pub my_reaction: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
}

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

pub struct MessageMapper;

impl FieldToColumn<MessageField> for MessageMapper {
    type Column = message::Column;

    fn map_field(field: MessageField) -> message::Column {
        match field {
            MessageField::CreatedAt => message::Column::CreatedAt,
            MessageField::Id => message::Column::Id,
            MessageField::Role => message::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(m: &message::Model, field: MessageField) -> sea_orm::Value {
        match field {
            MessageField::CreatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.created_at)),
            MessageField::Id => sea_orm::Value::Uuid(Some(m.id)),
            MessageField::Role => sea_orm::Value::String(Some(m.role.clone())),
        }
    }
}

/// Loads attachment summaries (non-deleted attachments) of the given messages.
///
/// # Errors
/// DB errors.
pub async fn attachments_by_message(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<AttachmentSummaryView>>, DomainError> {
    let mut out: HashMap<Uuid, Vec<AttachmentSummaryView>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let scope = tenant_scope(tenant_id);
    let links = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.is_in(message_ids.iter().copied())),
        )
        .secure()
        .scope_with(&scope)
        .all(db)
        .await?;
    if links.is_empty() {
        return Ok(out);
    }
    let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts = attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(ids))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .all(db)
        .await?;
    let by_id: HashMap<Uuid, &attachment::Model> = atts.iter().map(|a| (a.id, a)).collect();
    let mut links = links;
    links.sort_by_key(|l| (l.created_at, l.attachment_id));
    for l in links {
        if let Some(a) = by_id.get(&l.attachment_id) {
            out.entry(l.message_id)
                .or_default()
                .push(AttachmentSummaryView {
                    attachment_id: a.id,
                    kind: a.attachment_kind.clone(),
                    filename: a.filename.clone(),
                    status: a.status.clone(),
                    img_thumbnail: thumbnail_of(a),
                });
        }
    }
    Ok(out)
}

impl Core {
    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// 404 / 400 `OData` / PEP errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        mut query: ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let chat = self
            .authorize_chat(ctx, actions::LIST_MESSAGES, chat_id)
            .await?;
        if query.cursor.is_none() && query.order.is_empty() {
            query.order = ODataOrderBy(vec![OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let conn = self.db.conn()?;
        let scope = tenant_scope(chat.tenant_id);
        let base = message::Entity::find()
            .filter(message::Column::ChatId.eq(chat_id))
            .filter(message::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope);
        let page =
            paginate_odata::<MessageField, MessageMapper, message::Entity, message::Model, _, _>(
                base,
                &conn,
                &query,
                ("id", SortDir::Asc),
                PAGE_LIMITS,
                |m| m,
            )
            .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut atts = attachments_by_message(&conn, chat.tenant_id, chat_id, &ids).await?;
        let reactions = if ids.is_empty() {
            Vec::new()
        } else {
            reaction::Entity::find()
                .filter(
                    Condition::all()
                        .add(reaction::Column::UserId.eq(ctx.subject_id()))
                        .add(reaction::Column::MessageId.is_in(ids.clone())),
                )
                .secure()
                .scope_with(&scope)
                .all(&conn)
                .await?
        };
        let my: HashMap<Uuid, String> = reactions
            .into_iter()
            .map(|r| (r.message_id, r.reaction))
            .collect();
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let request_id = m.request_id.ok_or_else(|| {
                DomainError::internal(format!("message {} has no request_id", m.id))
            })?;
            let my_reaction = if m.role == "assistant" {
                my.get(&m.id).cloned()
            } else {
                None
            };
            let model = if m.role == "assistant" { m.model } else { None };
            items.push(MessageView {
                id: m.id,
                request_id,
                role: m.role,
                content: m.content,
                attachments: atts.remove(&m.id).unwrap_or_default(),
                my_reaction,
                model,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
                created_at: m.created_at,
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }
}
