//! Message history and reactions (DESIGN §3.3 List Messages, Message Reaction API).

use std::collections::HashMap;

use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, Set};
use toolkit_db::odata::{LimitCfg, paginate_with_odata};
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::authz::actions;
use super::clock;
use super::error::DomainError;

use super::service::MiniChat;
use crate::infra::storage::entity::{attachment, message, message_attachment, message_reaction};

/// Attachment summary embedded in a message.
#[derive(Debug, Clone)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
}

/// A message enriched with attachments and the caller's reaction.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: message::Model,
    pub attachments: Vec<AttachmentSummary>,
    pub my_reaction: Option<String>,
}

/// Valid reaction values.
pub const REACTIONS: [&str; 2] = ["like", "dislike"];

/// Batch-load the non-deleted attachments of messages.
///
/// # Errors
/// DB errors.
pub async fn attachments_for_messages(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<AttachmentSummary>>, DomainError> {
    let mut out: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::ChatId.eq(chat_id))
        .filter(message_attachment::Column::MessageId.is_in(message_ids.to_vec()))
        .order_by_asc(message_attachment::Column::CreatedAt)
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(out);
    }
    let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts = attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::Id.is_in(ids))
        .filter(attachment::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    let by_id: HashMap<Uuid, attachment::Model> = atts.into_iter().map(|a| (a.id, a)).collect();
    for link in links {
        if let Some(a) = by_id.get(&link.attachment_id) {
            let thumbnail = if a.attachment_kind == "image" && a.status == "ready" {
                match (&a.img_thumbnail, a.img_thumbnail_width, a.img_thumbnail_height) {
                    (Some(b), Some(w), Some(h)) => Some((b.clone(), w, h)),
                    _ => None,
                }
            } else {
                None
            };
            out.entry(link.message_id).or_default().push(AttachmentSummary {
                attachment_id: a.id,
                kind: a.attachment_kind.clone(),
                filename: a.filename.clone(),
                status: a.status.clone(),
                thumbnail,
            });
        }
    }
    Ok(out)
}

impl MiniChat {
    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// PDP errors, `ChatNotFound`, `OData` errors, DB errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        mut query: ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let access = self.load_chat(ctx, actions::LIST_MESSAGES, chat_id).await?;
        if query.order.is_empty() && query.cursor.is_none() {
            query.order = ODataOrderBy(vec![OrderKey { field: "created_at".to_owned(), dir: SortDir::Asc }]);
        }
        let conn = self.db.conn()?;
        let select = message::Entity::find()
            .filter(message::Column::ChatId.eq(chat_id))
            .filter(message::Column::DeletedAt.is_null())
            .filter(message::Column::RequestId.is_not_null())
            .secure()
            .scope_with(&access.child_scope)
            .into_inner();
        let page = paginate_with_odata(
            select,
            &conn,
            &query,
            &super::odata_fields::message_field_map(),
            ("id", SortDir::Asc),
            LimitCfg { default: 20, max: 100 },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut atts = attachments_for_messages(&conn, &access.child_scope, chat_id, &ids).await?;
        let reactions: HashMap<Uuid, String> = if ids.is_empty() {
            HashMap::new()
        } else {
            message_reaction::Entity::find()
                .filter(message_reaction::Column::MessageId.is_in(ids.clone()))
                .filter(message_reaction::Column::UserId.eq(ctx.subject_id()))
                .secure()
                .scope_with(&access.scope)
                .all(&conn)
                .await?
                .into_iter()
                .map(|r| (r.message_id, r.reaction))
                .collect()
        };
        for m in &page.items {
            if m.request_id.is_none() {
                return Err(DomainError::Internal(format!("message {} has a null request_id", m.id)));
            }
        }
        Ok(page.map_items(|m| {
            let attachments = atts.remove(&m.id).unwrap_or_default();
            let my_reaction = if m.role == "assistant" { reactions.get(&m.id).cloned() } else { None };
            MessageView { message: m, attachments, my_reaction }
        }))
    }

    async fn load_message_for_reaction(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<(super::service::ChatAccess, message::Model), DomainError> {
        let access = self.load_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let msg = message::Entity::find()
            .filter(message::Column::Id.eq(msg_id))
            .filter(message::Column::ChatId.eq(chat_id))
            .filter(message::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&access.child_scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::MessageNotFound(msg_id))?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok((access, msg))
    }

    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction` (upsert).
    ///
    /// # Errors
    /// `InvalidReaction`, PDP errors, not-found errors, `ReactionTargetNotAssistant`, DB errors.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> Result<message_reaction::Model, DomainError> {
        if !REACTIONS.contains(&reaction) {
            return Err(DomainError::InvalidReaction(format!(
                "reaction must be `like` or `dislike`, got `{reaction}`"
            )));
        }
        let (access, msg) = self.load_message_for_reaction(ctx, actions::SET_REACTION, chat_id, msg_id).await?;
        let conn = self.db.conn()?;
        let now = clock::now();
        let am = message_reaction::ActiveModel {
            id: Set(Uuid::now_v7()),
            message_id: Set(msg.id),
            user_id: Set(ctx.subject_id()),
            tenant_id: Set(access.chat.tenant_id),
            reaction: Set(reaction.to_owned()),
            created_at: Set(now),
        };
        let on_conflict = OnConflict::columns([message_reaction::Column::MessageId, message_reaction::Column::UserId])
            .update_columns([message_reaction::Column::Reaction, message_reaction::Column::CreatedAt])
            .to_owned();
        message_reaction::Entity::insert(am.clone())
            .secure()
            .scope_with_model(&access.scope, &am)?
            .on_conflict_raw(on_conflict)
            .exec(&conn)
            .await?;
        message_reaction::Entity::find()
            .filter(message_reaction::Column::MessageId.eq(msg.id))
            .filter(message_reaction::Column::UserId.eq(ctx.subject_id()))
            .secure()
            .scope_with(&access.scope)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::Internal("reaction vanished after upsert".to_owned()))
    }

    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    /// PDP errors, not-found errors, `ReactionTargetNotAssistant`, DB errors.
    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid) -> Result<(), DomainError> {
        let (access, msg) =
            self.load_message_for_reaction(ctx, actions::DELETE_REACTION, chat_id, msg_id).await?;
        let conn = self.db.conn()?;
        message_reaction::Entity::delete_many()
            .secure()
            .scope_with(&access.scope)
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(msg.id))
                    .add(message_reaction::Column::UserId.eq(ctx.subject_id())),
            )
            .exec(&conn)
            .await?;
        Ok(())
    }
}
