//! Message history, reactions, models API and quota status use cases.

use std::collections::HashMap;

use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::paginate_odata;
use toolkit_db::secure::SecureEntityExt;
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::chats::{LIST_LIMITS, with_default_order};
use super::error::{DomainError, Res};
use super::quota::{PeriodStatus, Periods, Usage, statuses};
use super::service::Services;
use crate::infra::db::entities::{attachment, message};
use crate::infra::db::odata::{MessageMapper, MessageQueryFilterField};
use crate::infra::db::repo::{attachments, messages, quota, reactions};
use crate::infra::db::{now_ts, tenant_scope};

/// Thumbnail of an image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailView {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub bytes: Vec<u8>,
}

/// Thumbnail of a ready image attachment.
#[must_use]
pub fn thumbnail_of(a: &attachment::Model) -> Option<ThumbnailView> {
    if a.attachment_kind != attachments::KIND_IMAGE || a.status != attachments::STATUS_READY {
        return None;
    }
    let bytes = a.img_thumbnail.clone()?;
    Some(ThumbnailView {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        bytes,
    })
}

/// Attachment summary embedded in messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryView {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub img_thumbnail: Option<ThumbnailView>,
}

/// A message of the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryView>,
    pub my_reaction: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub created_at: OffsetDateTime,
}

/// A reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}

/// Quota status of a user.
#[derive(Debug, Clone)]
pub struct QuotaStatusView {
    pub periods: Vec<PeriodStatus>,
    pub warning_threshold_pct: u8,
}

fn reaction_scope(tenant: Uuid, user: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant).ensure_owner(user)
}

/// Validate a reaction value (`like` / `dislike`).
pub fn validate_reaction(v: &str) -> Result<&'static str, DomainError> {
    match v {
        "like" => Ok("like"),
        "dislike" => Ok("dislike"),
        _ => Err(DomainError::invalid(
            Res::Message,
            "reaction",
            "INVALID_REACTION",
            "Reaction must be 'like' or 'dislike'",
        )),
    }
}

impl Services {
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let chat = self.load_chat(ctx, "list_messages", chat_id).await?;
        let scope = tenant_scope(chat.tenant_id);
        let query = with_default_order(query, "created_at", SortDir::Asc);
        let conn = self.db.conn()?;
        let select = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope);
        let page = paginate_odata::<
            MessageQueryFilterField,
            MessageMapper,
            message::Entity,
            message::Model,
            _,
            _,
        >(
            select,
            &conn,
            &query,
            ("id", SortDir::Asc),
            LIST_LIMITS,
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut atts: HashMap<Uuid, Vec<AttachmentSummaryView>> = HashMap::new();
        for (mid, a) in attachments::summaries_for_messages(&conn, &scope, chat_id, &ids).await? {
            atts.entry(mid).or_default().push(AttachmentSummaryView {
                attachment_id: a.id,
                kind: a.attachment_kind.clone(),
                filename: a.filename.clone(),
                status: a.status.clone(),
                img_thumbnail: thumbnail_of(&a),
            });
        }
        let my = reactions::for_messages(
            &conn,
            &reaction_scope(chat.tenant_id, ctx.subject_id()),
            ctx.subject_id(),
            &ids,
        )
        .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let Some(request_id) = m.request_id else {
                return Err(DomainError::internal(format!(
                    "message {} has a null request_id",
                    m.id
                )));
            };
            let is_assistant = m.role == "assistant";
            items.push(MessageView {
                id: m.id,
                request_id,
                role: m.role,
                content: m.content,
                attachments: atts.remove(&m.id).unwrap_or_default(),
                my_reaction: if is_assistant {
                    my.get(&m.id).cloned()
                } else {
                    None
                },
                model: m.model.clone(),
                input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
                output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
                created_at: m.created_at,
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
        msg_id: Uuid,
    ) -> Result<message::Model, DomainError> {
        let chat = self.load_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let msg = messages::find_in_chat(&conn, &tenant_scope(chat.tenant_id), chat_id, msg_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Message, msg_id.to_string()))?;
        if msg.role != "assistant" {
            return Err(DomainError::precondition(
                Res::Message,
                "reaction_target",
                "STATE",
                "Reactions are allowed on assistant messages only",
            ));
        }
        Ok(msg)
    }

    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        let reaction = validate_reaction(reaction)?;
        let msg = self
            .reaction_target(ctx, "set_reaction", chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        let row = reactions::upsert(
            &conn,
            &reaction_scope(msg.tenant_id, ctx.subject_id()),
            msg.tenant_id,
            ctx.subject_id(),
            msg.id,
            reaction,
            now_ts(),
        )
        .await?;
        Ok(ReactionView {
            message_id: row.message_id,
            reaction: row.reaction,
            created_at: row.created_at,
        })
    }

    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<(), DomainError> {
        let msg = self
            .reaction_target(ctx, "delete_reaction", chat_id, msg_id)
            .await?;
        let conn = self.db.conn()?;
        reactions::delete(
            &conn,
            &reaction_scope(msg.tenant_id, ctx.subject_id()),
            msg.id,
            ctx.subject_id(),
        )
        .await
    }

    pub async fn list_models(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.model_permission(ctx, "list").await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snapshot
            .model_catalog
            .into_iter()
            .filter(|m| m.enabled)
            .collect())
    }

    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        self.model_permission(ctx, "read").await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        snapshot
            .find_enabled_model(model_id)
            .cloned()
            .ok_or_else(|| DomainError::not_found(Res::Model, model_id.to_owned()))
    }

    pub async fn quota_status(
        &self,
        ctx: &SecurityContext,
    ) -> Result<QuotaStatusView, DomainError> {
        let scope = self.quota_scope(ctx).await?;
        let user = ctx.subject_id();
        let tenant = ctx.subject_tenant_id();
        let snapshot = self.policy.current_snapshot(user).await?;
        let limits = self
            .policy
            .user_limits(user, snapshot.policy_version)
            .await?;
        let periods = Periods::at(OffsetDateTime::now_utc());
        let conn = self.db.conn()?;
        let rows = quota::rows_for_periods(&conn, &scope, tenant, user, &periods.list()).await?;
        let usage = Usage::from_rows(&rows, periods);
        Ok(QuotaStatusView {
            periods: statuses(
                &usage,
                &limits,
                periods,
                self.cfg.quota.warning_threshold_pct,
            ),
            warning_threshold_pct: self.cfg.quota.warning_threshold_pct,
        })
    }
}
