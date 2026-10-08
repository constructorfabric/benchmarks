//! Message listing (with attachment and reaction enrichment) and reactions.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::collections::HashMap;

use sea_orm::{ColumnTrait, Condition, Set};
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_odata::filter::{FilterOp, ODataValue};
use toolkit_odata::{ODataQuery, OrderKey, Page, SortDir};
use toolkit_odata_macros::ODataFilterable;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::Service;
use super::chats::normalize_datetime_value;
use crate::domain::authz::{actions, tenant_scope, with_owner};
use crate::domain::clock;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::storage::entity::{attachment, message, message_attachment, message_reaction};

/// Attachment summary embedded in a message.
#[derive(Debug, Clone)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    /// `(webp bytes, width, height)` for ready images.
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
}

/// A message with its enrichments.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: message::Model,
    pub request_id: Uuid,
    pub attachments: Vec<AttachmentSummary>,
    pub my_reaction: Option<String>,
}

/// A stored reaction.
#[derive(Debug, Clone)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: time::OffsetDateTime,
}

/// `OData` fields of the message list.
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct MessageQuery {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: time::OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub role: String,
}

/// Column mapping of the message list.
pub struct MessageMapper;

impl FieldToColumn<MessageQueryFilterField> for MessageMapper {
    type Column = message::Column;

    fn map_field(field: MessageQueryFilterField) -> message::Column {
        match field {
            MessageQueryFilterField::CreatedAt => message::Column::CreatedAt,
            MessageQueryFilterField::Id => message::Column::Id,
            MessageQueryFilterField::Role => message::Column::Role,
        }
    }

    fn map_value(
        _field: MessageQueryFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        Ok(normalize_datetime_value(value))
    }
}

impl ODataFieldMapping<MessageQueryFilterField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(
        model: &message::Model,
        field: MessageQueryFilterField,
    ) -> sea_orm::Value {
        match field {
            MessageQueryFilterField::CreatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.created_at))
            }
            MessageQueryFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MessageQueryFilterField::Role => sea_orm::Value::String(Some(model.role.clone())),
        }
    }
}

/// Allowed reaction values.
pub const REACTIONS: &[&str] = &["like", "dislike"];

impl Service {
    /// Non-deleted attachment summaries of messages (in association order).
    pub(crate) async fn attachment_summaries<R: DBRunner>(
        runner: &R,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> DomainResult<HashMap<Uuid, Vec<AttachmentSummary>>> {
        let mut out: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
        if message_ids.is_empty() {
            return Ok(out);
        }
        let scope = tenant_scope(tenant_id);
        let links = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.is_in(message_ids.to_vec())),
            )
            .order_by(message_attachment::Column::CreatedAt, sea_orm::Order::Asc)
            .all(runner)
            .await?;
        if links.is_empty() {
            return Ok(out);
        }
        let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let atts: HashMap<Uuid, attachment::Model> = attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.is_in(ids))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .all(runner)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
        for link in links {
            if let Some(a) = atts.get(&link.attachment_id) {
                out.entry(link.message_id)
                    .or_default()
                    .push(attachment_summary(a));
            }
        }
        Ok(out)
    }

    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// Authz errors, `ChatNotFound`, `OData` errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> DomainResult<Page<MessageView>> {
        let (chat, scope) = self
            .authorized_chat(ctx, actions::LIST_MESSAGES, chat_id)
            .await?;
        let mut query = query.clone();
        if query.cursor.is_none() && query.order.0.is_empty() {
            query.order.0.push(OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            });
        }
        let conn = self.db.conn()?;
        let select = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope);
        let page = paginate_odata::<MessageQueryFilterField, MessageMapper, _, _, _, _>(
            select,
            &conn,
            &query,
            ("id", SortDir::Asc),
            LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut attachments =
            Self::attachment_summaries(&conn, chat.tenant_id, chat_id, &ids).await?;
        let reactions: HashMap<Uuid, String> = if ids.is_empty() {
            HashMap::new()
        } else {
            message_reaction::Entity::find()
                .secure()
                .scope_with(&with_owner(&tenant_scope(chat.tenant_id), ctx))
                .filter(
                    Condition::all()
                        .add(message_reaction::Column::MessageId.is_in(ids.clone()))
                        .add(message_reaction::Column::UserId.eq(ctx.subject_id())),
                )
                .all(&conn)
                .await?
                .into_iter()
                .map(|r| (r.message_id, r.reaction))
                .collect()
        };
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let request_id = m.request_id.ok_or_else(|| {
                DomainError::internal(format!("message {} has no request_id", m.id))
            })?;
            let my_reaction = if m.role == "assistant" {
                reactions.get(&m.id).cloned()
            } else {
                None
            };
            let atts = attachments.remove(&m.id).unwrap_or_default();
            items.push(MessageView {
                message: m,
                request_id,
                attachments: atts,
                my_reaction,
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> DomainResult<(AccessScope, message::Model)> {
        let (chat, scope) = self.authorized_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let msg = message::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message::Column::Id.eq(message_id))
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::MessageNotFound { id: message_id })?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok((with_owner(&tenant_scope(chat.tenant_id), ctx), msg))
    }

    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction` (upsert).
    ///
    /// # Errors
    /// `InvalidReaction`, authz errors, not found, `ReactionTargetNotAssistant`.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> DomainResult<ReactionView> {
        if !REACTIONS.contains(&reaction) {
            return Err(DomainError::InvalidReaction);
        }
        let (scope, msg) = self
            .reaction_target(ctx, actions::SET_REACTION, chat_id, message_id)
            .await?;
        let now = clock::now();
        let am = message_reaction::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(msg.id),
            user_id: Set(ctx.subject_id()),
            tenant_id: Set(msg.tenant_id),
            reaction: Set(reaction.to_owned()),
            created_at: Set(now),
        };
        let on_conflict = SecureOnConflict::<message_reaction::Entity>::columns([
            message_reaction::Column::MessageId,
            message_reaction::Column::UserId,
        ])
        .update_columns([
            message_reaction::Column::Reaction,
            message_reaction::Column::CreatedAt,
        ])?;
        let conn = self.db.conn()?;
        message_reaction::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict(on_conflict)
            .exec(&conn)
            .await?;
        Ok(ReactionView {
            message_id: msg.id,
            reaction: reaction.to_owned(),
            created_at: now,
        })
    }

    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    /// Authz errors, not found, `ReactionTargetNotAssistant`.
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> DomainResult<()> {
        let (scope, msg) = self
            .reaction_target(ctx, actions::DELETE_REACTION, chat_id, message_id)
            .await?;
        let conn = self.db.conn()?;
        message_reaction::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(msg.id))
                    .add(message_reaction::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        Ok(())
    }
}

/// Summary of one attachment row.
#[must_use]
pub fn attachment_summary(a: &attachment::Model) -> AttachmentSummary {
    let thumbnail = if a.status == "ready" && a.attachment_kind == "image" {
        a.img_thumbnail.clone().map(|bytes| {
            (
                bytes,
                a.img_thumbnail_width.unwrap_or_default(),
                a.img_thumbnail_height.unwrap_or_default(),
            )
        })
    } else {
        None
    };
    AttachmentSummary {
        attachment_id: a.id,
        kind: a.attachment_kind.clone(),
        filename: a.filename.clone(),
        status: a.status.clone(),
        thumbnail,
    }
}
