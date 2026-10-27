//! Chat service: chat CRUD, message listing, reactions, turn status.

use std::sync::Arc;

use base64::Engine;

use mini_chat_sdk::ModelTier;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    AttachmentKindDto, AttachmentStatusDto, ChatDetailDto, ImgThumbnailDto, MiniChatMessageDto,
    MiniChatReactionDto, MessageRoleDto, ReactionKindDto,
};
use crate::domain::services::ServiceContext;
use crate::error::{DomainError, ResourceKind};
use crate::infra::outbox::{ChatCleanupPayload, OutboxEnqueuer};

/// The chat service.
pub struct ChatService {
    ctx: Arc<ServiceContext>,
}

/// Validated chat creation input.
pub struct CreateChatInput {
    /// Title (trimmed).
    pub title: Option<String>,
    /// Requested model.
    pub model: Option<String>,
}

impl ChatService {
    /// Create the service.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>) -> Self {
        Self { ctx }
    }

    /// Create a chat.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        input: CreateChatInput,
    ) -> Result<ChatDetailDto, DomainError> {
        // Title validation happens before authorization and model lookup.
        let title = validate_title_opt(input.title.as_deref())?;
        let scope = self.ctx.pep.chat_scope(ctx, "create", None).await?;
        let snapshot = self.policy_snapshot(ctx).await?;
        let model = match &input.model {
            Some(id) => {
                let entry = snapshot.enabled_model(id).ok_or(DomainError::InvalidModel)?;
                entry.id.clone()
            }
            None => default_model(&snapshot).ok_or(DomainError::InvalidModel)?,
        };
        let chat_id = Uuid::new_v4();
        let model_row = self
            .ctx
            .repos
            .chats
            .insert(
                &self.ctx.db.conn()?,
                &scope,
                crate::infra::storage::repo::chats::NewChat {
                    id: chat_id,
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    model,
                    title,
                },
            )
            .await?;
        Ok(chat_dto(&model_row, 0))
    }

    /// Get a chat (metadata + message count).
    pub async fn get_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatDetailDto, DomainError> {
        let scope = self.ctx.pep.chat_scope(ctx, "read", Some(chat_id)).await?;
        let conn = self.ctx.db.conn()?;
        let chat = self
            .ctx
            .repos
            .chats
            .get(&conn, &scope, ctx.subject_tenant_id(), ctx.subject_id(), chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let count = self
            .ctx
            .repos
            .messages
            .count(&conn, &scope, ctx.subject_tenant_id(), chat_id)
            .await?;
        Ok(chat_dto(&chat, count))
    }

    /// List chats (cursor-paginated).
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetailDto>, DomainError> {
        let scope = self.ctx.pep.chat_scope(ctx, "list", None).await?;
        let conn = self.ctx.db.conn()?;
        let page = self
            .ctx
            .repos
            .chats
            .list_page(&conn, &scope, ctx.subject_tenant_id(), ctx.subject_id(), query)
            .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for chat in &page.items {
            let count = self
                .ctx
                .repos
                .messages
                .count(&conn, &scope, ctx.subject_tenant_id(), chat.id)
                .await?;
            items.push(chat_dto(chat, count));
        }
        Ok(Page::new(items, page.page_info))
    }

    /// Rename a chat.
    pub async fn update_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: String,
    ) -> Result<ChatDetailDto, DomainError> {
        let title = validate_title(&title)?;
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "update", Some(chat_id))
            .await?;
        let conn = self.ctx.db.conn()?;
        let chat = self
            .ctx
            .repos
            .chats
            .rename(
                &conn,
                &scope,
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                chat_id,
                Some(title),
            )
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let count = self
            .ctx
            .repos
            .messages
            .count(&conn, &scope, ctx.subject_tenant_id(), chat_id)
            .await?;
        Ok(chat_dto(&chat, count))
    }

#[allow(clippy::items_after_statements, reason = "the closure returns the wake to fire after commit")]
    /// Soft-delete a chat and enqueue provider cleanup.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "delete", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let svc_ctx = Arc::clone(&self.ctx);
        let user_id = ctx.subject_id();
        let deleted = self
            .ctx
            .db
            .transaction(|tx| {
                let scope = scope.clone();
                let svc_ctx = Arc::clone(&svc_ctx);
                Box::pin(async move {
                    let deleted = svc_ctx
                        .repos
                        .chats
                        .soft_delete(tx, &scope, tenant_id, user_id, chat_id)
                        .await?;
                    if !deleted {
                        return Ok((false, None));
                    }
                    svc_ctx
                        .repos
                        .attachments
                        .mark_cleanup_pending_for_chat(tx, tenant_id, chat_id)
                        .await?;
                    let payload = ChatCleanupPayload {
                        tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: crate::api::rest::dto::fmt_ts(&time::OffsetDateTime::now_utc()),
                    };
                    let wake = svc_ctx.outbox.enqueue_chat_cleanup(tx, &payload).await?;
                    Ok((true, Some(wake)))
                })
            })
            .await?;
        if let Some(wake) = deleted.1 {
            wake.fire();
        }
        if !deleted.0 {
            return Err(DomainError::NotFound(ResourceKind::Chat));
        }
        Ok(())
    }

    /// List messages of a chat (cursor-paginated, enriched with attachments
    /// and the caller's reactions).
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MiniChatMessageDto>, DomainError> {
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "list_messages", Some(chat_id))
            .await?;
        let conn = self.ctx.db.conn()?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        // The chat must be visible (owner-scoped) — 404 masks foreign chats.
        self.ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let page = self
            .ctx
            .repos
            .messages
            .list_page(&conn, &scope, tenant_id, chat_id, query)
            .await?;
        let message_ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let links = self
            .ctx
            .repos
            .message_attachments
            .for_messages(&conn, &scope, tenant_id, chat_id, &message_ids)
            .await?;
        let attachment_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let attachments = self
            .ctx
            .repos
            .attachments
            .get_many(&conn, &scope, tenant_id, chat_id, &attachment_ids)
            .await?;
        let reactions = self
            .ctx
            .repos
            .reactions
            .for_messages(&conn, &scope, tenant_id, user_id, &message_ids)
            .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for msg in &page.items {
            let msg_attachments: Vec<_> = links
                .iter()
                .filter(|l| l.message_id == msg.id)
                .filter_map(|l| attachments.iter().find(|a| a.id == l.attachment_id))
                .map(attachment_summary)
                .collect();
            items.push(message_dto(msg, msg_attachments, reactions.get(&msg.id)));
        }
        Ok(Page::new(items, page.page_info))
    }

    /// Set or update a reaction on an assistant message.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<MiniChatReactionDto, DomainError> {
        // The reaction value is validated before authorization.
        let kind = match reaction {
            "like" => ReactionKindDto::Like,
            "dislike" => ReactionKindDto::Dislike,
            _ => return Err(DomainError::InvalidReaction),
        };
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "set_reaction", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        self.ensure_chat(&conn, &scope, tenant_id, user_id, chat_id).await?;
        let message = self
            .ctx
            .repos
            .messages
            .get(&conn, &scope, tenant_id, chat_id, message_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Message))?;
        if message.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        let row = self
            .ctx
            .repos
            .reactions
            .set(&conn, &scope, tenant_id, user_id, message_id, reaction)
            .await?;
        Ok(MiniChatReactionDto {
            message_id,
            reaction: kind,
            created_at: row.created_at,
        })
    }

    /// Remove a reaction from an assistant message (idempotent).
    pub async fn delete_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "delete_reaction", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        self.ensure_chat(&conn, &scope, tenant_id, user_id, chat_id).await?;
        let message = self
            .ctx
            .repos
            .messages
            .get(&conn, &scope, tenant_id, chat_id, message_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Message))?;
        if message.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        self.ctx
            .repos
            .reactions
            .remove(&conn, &scope, tenant_id, user_id, message_id)
            .await?;
        Ok(())
    }

    async fn ensure_chat(
        &self,
        conn: &impl toolkit_db::secure::DBRunner,
        scope: &toolkit_db::secure::AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        self.ctx
            .repos
            .chats
            .get(conn, scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        Ok(())
    }

    async fn policy_snapshot(
        &self,
        ctx: &SecurityContext,
    ) -> Result<mini_chat_sdk::PolicySnapshot, DomainError> {
        let user_id = ctx.subject_id();
        let version = self
            .ctx
            .policy
            .current_policy_version(user_id)
            .await
            .map_err(policy_err)?;
        let snapshot = self
            .ctx
            .policy
            .policy_snapshot(user_id, version)
            .await
            .map_err(policy_err)?;
        Ok(snapshot)
    }
}

/// Map a policy gateway failure to an internal error (policy resolution
/// failure before streaming is 500 `internal`).
pub fn policy_err(e: crate::infra::gateways::GatewayError) -> DomainError {
    tracing::error!("mini-chat: policy resolution failed: {e}");
    DomainError::Internal
}

/// Validate an optional title (trim; 1–255 chars; whitespace-only rejected).
pub fn validate_title_opt(title: Option<&str>) -> Result<Option<String>, DomainError> {
    match title {
        None => Ok(None),
        Some(raw) => validate_title(raw).map(Some),
    }
}

/// Validate a required title.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 255 {
        return Err(DomainError::InvalidTitle);
    }
    Ok(trimmed.to_owned())
}

/// The default model: first enabled entry marked `is_default`, else the
/// first enabled entry (catalog order; tier not considered).
#[must_use]
pub fn default_model(snapshot: &mini_chat_sdk::PolicySnapshot) -> Option<String> {
    snapshot
        .model_catalog
        .iter()
        .find(|m| m.enabled && m.preference.is_some_and(|p| p.is_default))
        .or_else(|| snapshot.model_catalog.iter().find(|m| m.enabled))
        .map(|m| m.id.clone())
}

/// Chat model row -> DTO.
#[must_use]
pub fn chat_dto(row: &crate::infra::storage::entity::chats::Model, message_count: i64) -> ChatDetailDto {
    ChatDetailDto {
        id: row.id,
        model: row.model.clone(),
        title: row.title.clone(),
        is_temporary: row.is_temporary,
        message_count,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Attachment row -> summary DTO.
#[must_use]
pub fn attachment_summary(
    row: &crate::infra::storage::entity::attachments::Model,
) -> crate::api::rest::dto::AttachmentSummaryDto {
    crate::api::rest::dto::AttachmentSummaryDto {
        attachment_id: row.id,
        kind: if row.attachment_kind == "image" {
            AttachmentKindDto::Image
        } else {
            AttachmentKindDto::Document
        },
        filename: row.filename.clone(),
        status: attachment_status(row),
        img_thumbnail: thumbnail_dto(row),
    }
}

/// Attachment status string -> DTO.
#[must_use]
pub fn attachment_status(
    row: &crate::infra::storage::entity::attachments::Model,
) -> AttachmentStatusDto {
    match row.status.as_str() {
        "pending" => AttachmentStatusDto::Pending,
        "uploaded" => AttachmentStatusDto::Uploaded,
        "failed" => AttachmentStatusDto::Failed,
        _ => AttachmentStatusDto::Ready,
    }
}

/// Thumbnail DTO: present only for ready images with a stored thumbnail.
#[must_use]
pub fn thumbnail_dto(
    row: &crate::infra::storage::entity::attachments::Model,
) -> Option<ImgThumbnailDto> {
    if row.attachment_kind != "image" || row.status != "ready" {
        return None;
    }
    let bytes = row.img_thumbnail.as_ref()?;
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width: row.img_thumbnail_width.unwrap_or(0),
        height: row.img_thumbnail_height.unwrap_or(0),
        data_base64,
    })
}

/// Message row -> DTO.
#[must_use]
pub fn message_dto(
    row: &crate::infra::storage::entity::messages::Model,
    attachments: Vec<crate::api::rest::dto::AttachmentSummaryDto>,
    my_reaction: Option<&String>,
) -> MiniChatMessageDto {
    let role = match row.role.as_str() {
        "assistant" => MessageRoleDto::Assistant,
        "system" => MessageRoleDto::System,
        _ => MessageRoleDto::User,
    };
    let reaction = if role == MessageRoleDto::Assistant {
        my_reaction.map(|r| {
            if r == "like" {
                ReactionKindDto::Like
            } else {
                ReactionKindDto::Dislike
            }
        })
    } else {
        None
    };
    MiniChatMessageDto {
        id: row.id,
        // A stored message with a null request_id is an internal error; the
        // invariant is that API-exposed messages always carry one.
        request_id: row.request_id.unwrap_or_else(|| {
            tracing::error!("mini-chat: message {} has NULL request_id", row.id);
            Uuid::nil()
        }),
        role,
        content: row.content.clone(),
        attachments,
        my_reaction: reaction,
        model: row.model.clone(),
        input_tokens: (row.input_tokens != 0).then_some(row.input_tokens),
        output_tokens: (row.output_tokens != 0).then_some(row.output_tokens),
        created_at: row.created_at,
    }
}

/// Lowercase tier label for DTOs.
#[must_use]
pub fn tier_dto(tier: ModelTier) -> crate::api::rest::dto::ModelTierDto {
    match tier {
        ModelTier::Premium => crate::api::rest::dto::ModelTierDto::Premium,
        ModelTier::Standard => crate::api::rest::dto::ModelTierDto::Standard,
    }
}
