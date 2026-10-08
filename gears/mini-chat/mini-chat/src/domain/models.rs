//! Domain read models.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::infra::db::entity::{attachment, chat};

/// Chat metadata as returned by the chat API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDetail {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    /// Non-deleted messages of the chat.
    pub message_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatDetail {
    /// Detail of a chat row with its message count.
    #[must_use]
    pub fn from_row(row: chat::Model, message_count: i64) -> Self {
        Self {
            id: row.id,
            model: row.model,
            title: row.title,
            is_temporary: row.is_temporary,
            message_count,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// Attachment kind (`attachments.attachment_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Document,
    Image,
}

impl AttachmentKind {
    /// Column / wire value (`document` / `image`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }
}

/// Attachment summary of a listed message (non-deleted attachments only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    /// `document` | `image`.
    pub kind: String,
    pub filename: String,
    /// `pending` | `uploaded` | `ready` | `failed`.
    pub status: String,
    /// WebP thumbnail `(bytes, width, height)` of a ready image.
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
}

/// WebP thumbnail `(bytes, width, height)` of a ready image row (`None`
/// for documents, other states, or when no thumbnail was generated).
#[must_use]
pub fn ready_image_thumbnail(row: &attachment::Model) -> Option<(Vec<u8>, i32, i32)> {
    match (
        &row.img_thumbnail,
        row.img_thumbnail_width,
        row.img_thumbnail_height,
    ) {
        (Some(bytes), Some(w), Some(h))
            if row.attachment_kind == "image" && row.status == "ready" =>
        {
            Some((bytes.clone(), w, h))
        }
        _ => None,
    }
}

/// An attachment as returned by the attachment API (`AttachmentDetail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetail {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    /// `pending` | `uploaded` | `ready` | `failed`.
    pub status: String,
    /// `document` | `image`.
    pub kind: String,
    /// Only for `failed` rows.
    pub error_code: Option<String>,
    /// WebP thumbnail `(bytes, width, height)` of a ready image.
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
    pub created_at: OffsetDateTime,
}

impl AttachmentDetail {
    /// Public view of a row (no provider identifiers).
    #[must_use]
    pub fn from_row(row: &attachment::Model) -> Self {
        Self {
            id: row.id,
            filename: row.filename.clone(),
            content_type: row.content_type.clone(),
            size_bytes: row.size_bytes,
            status: row.status.clone(),
            kind: row.attachment_kind.clone(),
            error_code: row.error_code.clone().filter(|_| row.status == "failed"),
            thumbnail: ready_image_thumbnail(row),
            created_at: row.created_at,
        }
    }
}

/// A message as returned by the messages list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: Uuid,
    pub request_id: Uuid,
    /// `user` | `assistant` | `system`.
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummary>,
    /// The caller's reaction (`like` / `dislike`), assistant messages only.
    pub my_reaction: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
}

/// Public turn state (`chat_turns.state` mapping, D "Turn Status API").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

impl TurnStatusState {
    /// Map an internal state (`running` / `completed` / `failed` / `cancelled`).
    #[must_use]
    pub fn from_internal(state: &str) -> Option<Self> {
        match state {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Done),
            "failed" => Some(Self::Error),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A stored reaction as returned by `PUT .../reaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    pub message_id: Uuid,
    /// `like` | `dislike`.
    pub reaction: String,
    pub created_at: time::OffsetDateTime,
}

/// Turn status as returned by `GET /turns/{request_id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}
