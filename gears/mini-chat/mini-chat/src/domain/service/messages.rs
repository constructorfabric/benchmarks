//! `GET /v1/chats/{id}/messages` with attachment and reaction enrichment.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder};
use toolkit::Page;
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::MiniChatService;
use super::chats::LIMIT_CFG;
use crate::domain::authz::actions;
use crate::domain::error::DomainResult;
use crate::infra::db::entities::{attachments, message_attachments, message_reactions, messages};

/// Attachment summary embedded in messages.
#[derive(Debug, Clone)]
pub struct AttachmentSummary {
    pub attachment: attachments::Model,
}

/// Message with its enrichments.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: messages::Model,
    pub attachments: Vec<attachments::Model>,
    pub my_reaction: Option<String>,
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
    type Column = messages::Column;

    fn map_field(field: MessageField) -> messages::Column {
        match field {
            MessageField::CreatedAt => messages::Column::CreatedAt,
            MessageField::Id => messages::Column::Id,
            MessageField::Role => messages::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageField> for MessageMapper {
    type Entity = messages::Entity;

    fn extract_cursor_value(model: &messages::Model, field: MessageField) -> sea_orm::Value {
        match field {
            MessageField::CreatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.created_at)),
            MessageField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MessageField::Role => sea_orm::Value::String(Some(model.role.clone())),
        }
    }
}

/// Non-deleted attachments linked to each message (via `message_attachments`).
pub(crate) async fn attachments_by_message(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> DomainResult<HashMap<Uuid, Vec<attachments::Model>>> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let links = message_attachments::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .order_by(message_attachments::Column::CreatedAt, sea_orm::Order::Asc)
        .secure()
        .scope_with(tenant_scope)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(HashMap::new());
    }
    let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts: HashMap<Uuid, attachments::Model> = attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::Id.is_in(att_ids))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(tenant_scope)
        .all(runner)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let mut out: HashMap<Uuid, Vec<attachments::Model>> = HashMap::new();
    for l in links {
        if let Some(a) = atts.get(&l.attachment_id) {
            out.entry(l.message_id).or_default().push(a.clone());
        }
    }
    Ok(out)
}

impl MiniChatService {
    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// Authorization, not found, `OData` or database errors.
    pub async fn list_messages(&self, ctx: &SecurityContext, chat_id: Uuid, mut query: ODataQuery) -> DomainResult<Page<MessageView>> {
        let scopes = self.scopes(ctx, actions::LIST_MESSAGES, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        self.load_chat(&conn, &scopes, chat_id).await?;
        if query.order.is_empty() && query.cursor.is_none() {
            query.order = ODataOrderBy(vec![OrderKey { field: "created_at".into(), dir: SortDir::Asc }]);
        }
        let select = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scopes.tenant);
        let page: Page<messages::Model> =
            paginate_odata::<MessageField, MessageMapper, _, _, _, _>(select, &conn, &query, ("id", SortDir::Asc), LIMIT_CFG, |m| m)
                .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut atts = attachments_by_message(&conn, &scopes.tenant, chat_id, &ids).await?;
        let reactions: HashMap<Uuid, String> = if ids.is_empty() {
            HashMap::new()
        } else {
            message_reactions::Entity::find()
                .filter(message_reactions::Column::MessageId.is_in(ids.clone()))
                .secure()
                .scope_with(&scopes.owner)
                .all(&conn)
                .await?
                .into_iter()
                .map(|r| (r.message_id, r.reaction))
                .collect()
        };
        Ok(page.map_items(|m| {
            let attachments = atts.remove(&m.id).unwrap_or_default();
            let my_reaction = if m.role == "assistant" { reactions.get(&m.id).cloned() } else { None };
            MessageView { message: m, attachments, my_reaction }
        }))
    }
}
