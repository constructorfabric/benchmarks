//! Message listing with attachment/reaction enrichment, and reactions.

use std::collections::HashMap;

use base64::Engine as _;
use sea_orm::EntityTrait;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, Set};
use time::OffsetDateTime;
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::domain::odata_fields::{MessageField, MessageMapper};
use crate::domain::state::AppState;
use crate::infra::db::entities::{attachments, message_reactions, messages};
use crate::infra::db::repo;

pub const MESSAGE_LIMITS: LimitCfg = LimitCfg { default: 20, max: 100 };

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub img_thumbnail: Option<Thumbnail>,
}

/// Thumbnail projection: only ready images carry it.
#[must_use]
pub fn thumbnail_of(a: &attachments::Model) -> Option<Thumbnail> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let data = a.img_thumbnail.as_ref()?;
    Some(Thumbnail {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(data),
    })
}

#[must_use]
pub fn summary_of(a: &attachments::Model) -> AttachmentSummary {
    AttachmentSummary {
        attachment_id: a.id,
        kind: a.attachment_kind.clone(),
        filename: a.filename.clone(),
        status: a.status.clone(),
        img_thumbnail: thumbnail_of(a),
    }
}

#[derive(Debug, Clone)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummary>,
    pub my_reaction: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}

/// Validate a reaction value.
///
/// # Errors
/// 400 `INVALID_REACTION`.
pub fn validate_reaction(v: &str) -> DomainResult<&'static str> {
    match v {
        "like" => Ok("like"),
        "dislike" => Ok("dislike"),
        _ => Err(DomainError::field(
            Res::Message,
            "reaction",
            "INVALID_REACTION",
            "reaction must be 'like' or 'dislike'",
        )),
    }
}

impl AppState {
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        mut q: ODataQuery,
    ) -> DomainResult<Page<MessageView>> {
        let scopes = self.chat_scope(ctx, "list_messages", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy(vec![OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let select = messages::Entity::find()
            .secure()
            .scope_with(&scopes.tenant)
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            );
        let page = paginate_odata::<MessageField, MessageMapper, messages::Entity, messages::Model, _, _>(
            select,
            &conn,
            &q,
            ("id", SortDir::Asc),
            MESSAGE_LIMITS,
            |m| m,
        )
        .await
        .map_err(DomainError::from)?;
        let items = self
            .enrich_messages(&conn, &scopes.tenant, ctx.subject_id(), chat_id, page.items)
            .await?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    pub(crate) async fn enrich_messages(
        &self,
        conn: &impl toolkit_db::secure::DBRunner,
        tenant_scope: &toolkit_security::AccessScope,
        user_id: Uuid,
        chat_id: Uuid,
        rows: Vec<messages::Model>,
    ) -> DomainResult<Vec<MessageView>> {
        let ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
        let links = repo::message_attachment_links(conn, tenant_scope, chat_id, &ids).await?;
        let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let atts: HashMap<Uuid, attachments::Model> = repo::attachments_by_ids(conn, tenant_scope, &att_ids)
            .await?
            .into_iter()
            .filter(|a| a.deleted_at.is_none() && a.chat_id == chat_id)
            .map(|a| (a.id, a))
            .collect();
        let mut by_msg: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
        for l in &links {
            if let Some(a) = atts.get(&l.attachment_id) {
                by_msg.entry(l.message_id).or_default().push(summary_of(a));
            }
        }
        let reactions: HashMap<Uuid, String> = repo::reactions_for(conn, tenant_scope, user_id, &ids)
            .await?
            .into_iter()
            .map(|r| (r.message_id, r.reaction))
            .collect();
        let mut out = Vec::with_capacity(rows.len());
        for m in rows {
            let Some(request_id) = m.request_id else {
                return Err(DomainError::internal(format!("message {} has no request_id", m.id)));
            };
            out.push(MessageView {
                id: m.id,
                request_id,
                my_reaction: if m.role == "assistant" { reactions.get(&m.id).cloned() } else { None },
                attachments: by_msg.remove(&m.id).unwrap_or_default(),
                role: m.role,
                content: m.content,
                model: m.model,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
                created_at: m.created_at,
            });
        }
        Ok(out)
    }

    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> DomainResult<(crate::domain::state::ChatScopes, messages::Model)> {
        let scopes = self.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let msg = repo::find_message(&conn, &scopes.tenant, chat_id, message_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Message, message_id.to_string()))?;
        if msg.role != "assistant" {
            return Err(DomainError::precondition(
                Res::Message,
                "reaction_target",
                "STATE",
                "reactions are allowed on assistant messages only",
            ));
        }
        Ok((scopes, msg))
    }

    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> DomainResult<ReactionView> {
        let reaction = validate_reaction(reaction)?;
        let (scopes, msg) = self.reaction_target(ctx, "set_reaction", chat_id, message_id).await?;
        let conn = self.db.conn()?;
        let now = repo::now();
        let am = message_reactions::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(msg.id),
            user_id: Set(ctx.subject_id()),
            tenant_id: Set(msg.tenant_id),
            reaction: Set(reaction.to_owned()),
            created_at: Set(now),
        };
        let on_conflict = OnConflict::columns([message_reactions::Column::MessageId, message_reactions::Column::UserId])
            .value(message_reactions::Column::Reaction, Expr::value(reaction))
            .value(message_reactions::Column::CreatedAt, Expr::value(now))
            .to_owned();
        message_reactions::Entity::insert(am)
            .secure()
            .scope_unchecked(&scopes.chat)?
            .on_conflict_raw(on_conflict)
            .exec(&conn)
            .await?;
        Ok(ReactionView {
            message_id: msg.id,
            reaction: reaction.to_owned(),
            created_at: now,
        })
    }

    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, message_id: Uuid) -> DomainResult<()> {
        let (scopes, msg) = self.reaction_target(ctx, "delete_reaction", chat_id, message_id).await?;
        let conn = self.db.conn()?;
        message_reactions::Entity::delete_many()
            .secure()
            .scope_with(&scopes.chat)
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg.id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .exec(&conn)
            .await?;
        Ok(())
    }
}
