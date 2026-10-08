//! Read models returned by the domain service to the REST layer.

use base64::Engine;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::infra::db::entity::{attachments, chats, messages};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatView {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatView {
    #[must_use]
    pub fn from_model(m: chats::Model, message_count: i64) -> Self {
        Self {
            id: m.id,
            model: m.model,
            title: m.title,
            is_temporary: m.is_temporary,
            message_count,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailView {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

/// Thumbnail of an attachment row (only for ready images).
#[must_use]
pub fn thumbnail_of(a: &attachments::Model) -> Option<ThumbnailView> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let bytes = a.img_thumbnail.as_ref()?;
    Some(ThumbnailView {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
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
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub attachments: Vec<AttachmentSummaryView>,
    pub my_reaction: Option<String>,
    pub created_at: OffsetDateTime,
}

impl MessageView {
    /// Build from a row; `None` when the row has no `request_id`.
    #[must_use]
    pub fn from_model(m: messages::Model) -> Option<Self> {
        let request_id = m.request_id?;
        let assistant = m.role == "assistant";
        Some(Self {
            id: m.id,
            request_id,
            role: m.role,
            content: m.content,
            model: if assistant { m.model } else { None },
            input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
            attachments: Vec::new(),
            my_reaction: None,
            created_at: m.created_at,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentView {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub kind: String,
    pub error_code: Option<String>,
    pub img_thumbnail: Option<ThumbnailView>,
    pub created_at: OffsetDateTime,
}

impl AttachmentView {
    #[must_use]
    pub fn from_model(a: &attachments::Model) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            status: a.status.clone(),
            kind: a.attachment_kind.clone(),
            error_code: if a.status == "failed" { a.error_code.clone() } else { None },
            img_thumbnail: thumbnail_of(a),
            created_at: a.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    /// `running` | `done` | `error` | `cancelled`
    pub state: String,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelView {
    pub model_id: String,
    pub display_name: String,
    pub tier: String,
    pub multiplier_display: String,
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: OffsetDateTime,
}
