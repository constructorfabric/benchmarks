//! REST DTOs (`OpenAPI` names `ModelDto`, `ModelListDto`, `ModelTierDto`,
//! `ChatDetailDto`, `CreateChatReq`, `UpdateChatReq`, `MiniChatMessageDto`,
//! `AttachmentSummaryDto`, `ImgThumbnailDto`, `MiniChatReactionDto`,
//! `SetReactionReq`).
//!
//! The wire form is the public projection only: provider ids, credit
//! multipliers, token limits, the default flag and the enabled flag never
//! appear (DESIGN 3.3 "Non-Exposure Rules (Models API)").

use base64::Engine as _;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::chats::ChatView;
use crate::domain::messages::{
    AttachmentKind, AttachmentStatus, AttachmentSummaryView, MessageRole, MessageView,
    ThumbnailView,
};
use crate::domain::models::{ModelTierView, ModelView};
use crate::domain::reactions::{ReactionKind, ReactionView};

/// Model tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    /// Standard tier.
    Standard,
    /// Premium tier.
    Premium,
}

impl From<ModelTierView> for ModelTierDto {
    fn from(value: ModelTierView) -> Self {
        match value {
            ModelTierView::Standard => Self::Standard,
            ModelTierView::Premium => Self::Premium,
        }
    }
}

/// Response DTO for a single model.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    /// Public model identifier.
    pub model_id: String,
    /// Display name.
    pub display_name: String,
    /// Model tier.
    pub tier: ModelTierDto,
    /// Human readable credit multiplier (for example `3x`).
    pub multiplier_display: String,
    /// Description; omitted when the catalog has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Multimodal capabilities (for example `VISION_INPUT`).
    pub multimodal_capabilities: Vec<String>,
    /// Context window in tokens.
    pub context_window: u32,
}

impl From<ModelView> for ModelDto {
    fn from(view: ModelView) -> Self {
        Self {
            model_id: view.model_id,
            display_name: view.display_name,
            tier: view.tier.into(),
            multiplier_display: view.multiplier_display,
            description: view.description,
            multimodal_capabilities: view.multimodal_capabilities,
            context_window: view.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    /// Models visible to the caller.
    pub items: Vec<ModelDto>,
}

// ── chats ───────────────────────────────────────────────────────────────────

/// Request DTO for creating a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    /// Optional title (1-255 characters after trimming).
    pub title: Option<String>,
    /// Optional model id; defaults to the first enabled default model.
    pub model: Option<String>,
}

/// Request DTO for renaming a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    /// New title (1-255 characters after trimming).
    pub title: String,
}

/// Response DTO for chat details.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    /// Chat id.
    pub id: Uuid,
    /// Model of the chat, locked for its lifetime.
    pub model: String,
    /// Title; omitted for an untitled chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Temporary-chat flag (always `false` in P1).
    pub is_temporary: bool,
    /// Number of non-deleted messages.
    pub message_count: i64,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last activity time.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<ChatView> for ChatDetailDto {
    fn from(view: ChatView) -> Self {
        Self {
            id: view.id,
            model: view.model,
            title: view.title,
            is_temporary: view.is_temporary,
            message_count: view.message_count,
            created_at: view.created_at,
            updated_at: view.updated_at,
        }
    }
}

// ── messages ────────────────────────────────────────────────────────────────

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum MessageRoleDto {
    /// Written by the user.
    User,
    /// Produced by the model.
    Assistant,
    /// Background / system message.
    System,
}

impl From<MessageRole> for MessageRoleDto {
    fn from(value: MessageRole) -> Self {
        match value {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::System => Self::System,
        }
    }
}

/// Reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ReactionKindDto {
    /// Positive.
    Like,
    /// Negative.
    Dislike,
}

impl From<ReactionKind> for ReactionKindDto {
    fn from(value: ReactionKind) -> Self {
        match value {
            ReactionKind::Like => Self::Like,
            ReactionKind::Dislike => Self::Dislike,
        }
    }
}

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentKindDto {
    /// A document.
    Document,
    /// An image.
    Image,
}

impl From<AttachmentKind> for AttachmentKindDto {
    fn from(value: AttachmentKind) -> Self {
        match value {
            AttachmentKind::Document => Self::Document,
            AttachmentKind::Image => Self::Image,
        }
    }
}

/// Attachment lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentStatusDto {
    /// Upload started.
    Pending,
    /// Provider upload done, indexing running.
    Uploaded,
    /// Usable.
    Ready,
    /// Failed.
    Failed,
}

impl From<AttachmentStatus> for AttachmentStatusDto {
    fn from(value: AttachmentStatus) -> Self {
        match value {
            AttachmentStatus::Pending => Self::Pending,
            AttachmentStatus::Uploaded => Self::Uploaded,
            AttachmentStatus::Ready => Self::Ready,
            AttachmentStatus::Failed => Self::Failed,
        }
    }
}

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    /// Always `image/webp`.
    pub content_type: String,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Standard base64 of the thumbnail bytes.
    pub data_base64: String,
}

impl From<ThumbnailView> for ImgThumbnailDto {
    fn from(view: ThumbnailView) -> Self {
        Self {
            content_type: "image/webp".to_owned(),
            width: view.width,
            height: view.height,
            data_base64: base64::engine::general_purpose::STANDARD.encode(view.bytes),
        }
    }
}

/// Lightweight attachment metadata embedded in message responses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    /// Attachment id.
    pub attachment_id: Uuid,
    /// Kind.
    pub kind: AttachmentKindDto,
    /// File name.
    pub filename: String,
    /// Status.
    pub status: AttachmentStatusDto,
    /// Thumbnail; only for a ready image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummaryView> for AttachmentSummaryDto {
    fn from(view: AttachmentSummaryView) -> Self {
        Self {
            attachment_id: view.attachment_id,
            kind: view.kind.into(),
            filename: view.filename,
            status: view.status.into(),
            img_thumbnail: view.thumbnail.map(Into::into),
        }
    }
}

/// Response DTO for a message in the list endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    /// Message id.
    pub id: Uuid,
    /// Turn correlation key, never null.
    pub request_id: Uuid,
    /// Author role.
    pub role: MessageRoleDto,
    /// Message text.
    pub content: String,
    /// Model that produced an assistant message; omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider-reported input tokens; omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Provider-reported output tokens; omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Linked, non-deleted attachments (empty when none).
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction; `null` when there is none.
    pub my_reaction: Option<ReactionKindDto>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(view: MessageView) -> Self {
        Self {
            id: view.id,
            request_id: view.request_id,
            role: view.role.into(),
            content: view.content,
            model: view.model,
            input_tokens: (view.input_tokens != 0).then_some(view.input_tokens),
            output_tokens: (view.output_tokens != 0).then_some(view.output_tokens),
            attachments: view.attachments.into_iter().map(Into::into).collect(),
            my_reaction: view.my_reaction.map(Into::into),
            created_at: view.created_at,
        }
    }
}

// ── reactions ───────────────────────────────────────────────────────────────

/// Request DTO for setting a reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    /// `like` or `dislike`; validated by the service (400 `INVALID_REACTION`).
    pub reaction: String,
}

/// Response DTO for a reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    /// Reacted message.
    pub message_id: Uuid,
    /// Reaction value.
    pub reaction: ReactionKindDto,
    /// Time of the (last) reaction.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(view: ReactionView) -> Self {
        Self {
            message_id: view.message_id,
            reaction: view.reaction.into(),
            created_at: view.created_at,
        }
    }
}
