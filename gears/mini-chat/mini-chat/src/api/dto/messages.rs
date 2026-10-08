//! Message list DTOs (`MiniChatMessageDto`, `AttachmentSummaryDto`, `ImgThumbnailDto` and their
//! enums) and the `OData` field enum of the message list.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use time::OffsetDateTime;
use toolkit_odata::filter::{FieldKind, FilterField};
use uuid::Uuid;

use crate::api::dto::reactions::ReactionKindDto;
use crate::domain::message_service::{AttachmentSummary, MessageView, Thumbnail};
use crate::infra::db::{AttachmentKind, AttachmentStatus, MessageRole};

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`).

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

/// Attachment lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

/// Lightweight attachment metadata embedded in Message responses.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    // Omitted unless the attachment is a ready image with a preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

/// Response DTO for a message in the list endpoint.
///
/// Aliased to `MiniChatMessageDto` in the `OpenAPI`` schema: `chat-engine` also
/// exposes a `MessageDto`, and both gears register into the same api-gateway
/// `OpenAPI`` registry, so the bare ident would collide in `components.schemas`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRoleDto,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction to this message; `null` when there is none.
    #[schema(required = true)]
    pub my_reaction: Option<ReactionKindDto>,
    #[serde(with = "crate::api::dto::timestamp")]
    pub created_at: OffsetDateTime,
    // Omitted for user messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    // Omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    // Omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

impl From<MessageRole> for MessageRoleDto {
    fn from(role: MessageRole) -> Self {
        match role {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::System => Self::System,
        }
    }
}

impl From<AttachmentKind> for AttachmentKindDto {
    fn from(kind: AttachmentKind) -> Self {
        match kind {
            AttachmentKind::Document => Self::Document,
            AttachmentKind::Image => Self::Image,
        }
    }
}

impl From<AttachmentStatus> for AttachmentStatusDto {
    fn from(status: AttachmentStatus) -> Self {
        match status {
            AttachmentStatus::Pending => Self::Pending,
            AttachmentStatus::Uploaded => Self::Uploaded,
            AttachmentStatus::Ready => Self::Ready,
            AttachmentStatus::Failed => Self::Failed,
        }
    }
}

impl From<Thumbnail> for ImgThumbnailDto {
    fn from(t: Thumbnail) -> Self {
        Self {
            content_type: "image/webp".to_owned(),
            width: t.width,
            height: t.height,
            data_base64: STANDARD.encode(t.data),
        }
    }
}

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(a: AttachmentSummary) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: a.kind.into(),
            filename: a.filename,
            status: a.status.into(),
            img_thumbnail: a.thumbnail.map(Into::into),
        }
    }
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(m: MessageView) -> Self {
        Self {
            id: m.id,
            request_id: m.request_id,
            role: m.role.into(),
            content: m.content,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
            my_reaction: m.my_reaction.map(Into::into),
            created_at: m.created_at,
            model: m.model,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
        }
    }
}

/// Fields of the message list admitted in `$filter` and `$orderby`. The order of
/// [`Self::FIELDS`] is the order of the published `x-odata-*` extensions.
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
