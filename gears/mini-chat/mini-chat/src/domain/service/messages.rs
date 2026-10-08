//! Message listing and reactions (DESIGN §3.3).

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use sea_orm::sea_query::Expr;
use toolkit_db::odata::{FieldMap, LimitCfg, paginate_with_odata};
use toolkit_odata::filter::FieldKind;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::chats::load_chat;
use super::{AppServices, now};
use crate::domain::error::{DomainError, NotFoundKind};
use crate::infra::db::entity::{attachments, message_attachments, message_reactions, messages};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailView {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryView {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub img_thumbnail: Option<ThumbnailView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryView>,
    pub my_reaction: Option<String>,
    pub created_at: DateTime<Utc>,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: DateTime<Utc>,
}

static MESSAGE_FIELDS: LazyLock<FieldMap<messages::Entity>> = LazyLock::new(|| {
    FieldMap::new()
        .insert_with_extractor("created_at", messages::Column::CreatedAt, FieldKind::DateTimeUtc, |m: &messages::Model| {
            m.created_at.to_rfc3339()
        })
        .insert_with_extractor("id", messages::Column::Id, FieldKind::Uuid, |m: &messages::Model| m.id.to_string())
        .insert_with_extractor("role", messages::Column::Role, FieldKind::String, |m: &messages::Model| m.role.clone())
});

/// Thumbnail of an attachment row when it is a ready image with bytes.
#[must_use]
pub fn thumbnail_of(a: &attachments::Model) -> Option<ThumbnailView> {
    if a.status != "ready" || a.attachment_kind != "image" {
        return None;
    }
    let data = a.img_thumbnail.clone()?;
    Some(ThumbnailView {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data,
    })
}

/// Attachment summaries of messages (non-deleted attachments only).
///
/// # Errors
/// Database failure.
pub async fn attachments_by_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<AttachmentSummaryView>>, DomainError> {
    let mut out: HashMap<Uuid, Vec<AttachmentSummaryView>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let scope = AccessScope::for_tenant(tenant_id);
    let links = message_attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(out);
    }
    let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts = attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(attachments::Column::Id.is_in(att_ids))
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .all(runner)
        .await?;
    let by_id: HashMap<Uuid, &attachments::Model> = atts.iter().map(|a| (a.id, a)).collect();
    let mut links = links;
    links.sort_by_key(|l| (l.created_at, l.attachment_id));
    for l in links {
        if let Some(a) = by_id.get(&l.attachment_id) {
            out.entry(l.message_id).or_default().push(AttachmentSummaryView {
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

fn validate_reaction(r: &str) -> Result<String, DomainError> {
    match r {
        "like" | "dislike" => Ok(r.to_owned()),
        _ => Err(DomainError::InvalidReaction),
    }
}

async fn load_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    msg_id: Uuid,
) -> Result<messages::Model, DomainError> {
    messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(messages::Column::Id.eq(msg_id))
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?
        .ok_or(DomainError::NotFound(NotFoundKind::Message))
}

impl AppServices {
    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// OData, authorization and database errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, "list_messages", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let mut q = query.clone();
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy(vec![
                OrderKey {
                    field: "created_at".into(),
                    dir: SortDir::Asc,
                },
                OrderKey {
                    field: "id".into(),
                    dir: SortDir::Asc,
                },
            ]);
        }
        let select = messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .into_inner();
        let page = paginate_with_odata::<messages::Entity, messages::Model, _, _>(
            select,
            &conn,
            &q,
            &MESSAGE_FIELDS,
            ("id", SortDir::Asc),
            LimitCfg { default: 20, max: 100 },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut atts = attachments_by_message(&conn, chat.tenant_id, chat_id, &ids).await?;
        let reactions: HashMap<Uuid, String> = if ids.is_empty() {
            HashMap::new()
        } else {
            message_reactions::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(
                    Condition::all()
                        .add(message_reactions::Column::MessageId.is_in(ids.clone()))
                        .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
                )
                .all(&conn)
                .await?
                .into_iter()
                .map(|r| (r.message_id, r.reaction))
                .collect()
        };
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let request_id = m
                .request_id
                .ok_or_else(|| DomainError::internal(format!("message {} has no request_id", m.id)))?;
            items.push(MessageView {
                id: m.id,
                request_id,
                my_reaction: if m.role == "assistant" {
                    reactions.get(&m.id).cloned()
                } else {
                    None
                },
                attachments: atts.remove(&m.id).unwrap_or_default(),
                model: if m.role == "assistant" { m.model } else { None },
                input_tokens: (m.input_tokens > 0).then_some(m.input_tokens),
                output_tokens: (m.output_tokens > 0).then_some(m.output_tokens),
                role: m.role,
                content: m.content,
                created_at: m.created_at,
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
    ///
    /// # Errors
    /// `InvalidReaction`, `NotFound`, `ReactionTarget`, authorization and database errors.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let reaction = validate_reaction(reaction)?;
        let scope = self.authz.chat_scope(ctx, "set_reaction", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let msg = load_message(&conn, chat.tenant_id, chat_id, msg_id).await?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTarget);
        }
        let ts = now();
        let existing = message_reactions::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg_id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .one(&conn)
            .await?;
        if let Some(r) = existing {
            if r.reaction == reaction {
                return Ok(ReactionView {
                    message_id: msg_id,
                    reaction,
                    created_at: r.created_at,
                });
            }
            message_reactions::Entity::update_many()
                .col_expr(message_reactions::Column::Reaction, Expr::value(reaction.clone()))
                .col_expr(message_reactions::Column::CreatedAt, Expr::value(ts))
                .filter(Condition::all().add(message_reactions::Column::Id.eq(r.id)))
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await?;
        } else {
            let am = message_reactions::ActiveModel {
                id: sea_orm::Set(Uuid::new_v4()),
                message_id: sea_orm::Set(msg_id),
                user_id: sea_orm::Set(ctx.subject_id()),
                tenant_id: sea_orm::Set(chat.tenant_id),
                reaction: sea_orm::Set(reaction.clone()),
                created_at: sea_orm::Set(ts),
            };
            match message_reactions::Entity::insert(am)
                .secure()
                .scope_unchecked(&scope)?
                .exec(&conn)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    let err = DomainError::from(e);
                    if !matches!(err, DomainError::UniqueViolation) {
                        return Err(err);
                    }
                    // concurrent PUT: last writer wins
                    message_reactions::Entity::update_many()
                        .col_expr(message_reactions::Column::Reaction, Expr::value(reaction.clone()))
                        .filter(
                            Condition::all()
                                .add(message_reactions::Column::MessageId.eq(msg_id))
                                .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await?;
                }
            }
        }
        Ok(ReactionView {
            message_id: msg_id,
            reaction,
            created_at: ts,
        })
    }

    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    /// `NotFound`, `ReactionTarget`, authorization and database errors.
    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid) -> Result<(), DomainError> {
        let scope = self.authz.chat_scope(ctx, "delete_reaction", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let msg = load_message(&conn, chat.tenant_id, chat_id, msg_id).await?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTarget);
        }
        message_reactions::Entity::delete_many()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg_id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .exec(&conn)
            .await?;
        Ok(())
    }
}
