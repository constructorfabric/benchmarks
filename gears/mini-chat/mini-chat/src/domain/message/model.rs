//! Message views.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The role of a stored message.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// A user message.
    User,
    /// An assistant message.
    Assistant,
    /// A system message.
    System,
}

impl MessageRole {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    /// Parses a stored value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// A server-generated preview of an image attachment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The encoded (WebP) bytes.
    pub data: Vec<u8>,
}

/// An attachment of a message, as the message list shows it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummary {
    /// The attachment id.
    pub attachment_id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    /// The filename.
    pub filename: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// The preview of a ready image.
    pub thumbnail: Option<Thumbnail>,
}

/// A reaction value.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// Like.
    Like,
    /// Dislike.
    Dislike,
}

impl Reaction {
    /// Parses a request value; anything but `like` and `dislike` is `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "like" => Some(Self::Like),
            "dislike" => Some(Self::Dislike),
            _ => None,
        }
    }

    /// The stored and wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }
}

/// A stored reaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionView {
    /// The reacted message.
    pub message_id: Uuid,
    /// The reaction.
    pub reaction: Reaction,
    /// When the reaction was set.
    pub created_at: OffsetDateTime,
}

/// A message with its batch enrichment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// Message id.
    pub id: Uuid,
    /// The turn correlation key.
    pub request_id: Uuid,
    /// The role.
    pub role: MessageRole,
    /// The text.
    pub content: String,
    /// The effective model of an assistant message.
    pub model: Option<String>,
    /// Provider-reported input tokens.
    pub input_tokens: i64,
    /// Provider-reported output tokens.
    pub output_tokens: i64,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Non-deleted attachments.
    pub attachments: Vec<AttachmentSummary>,
    /// The caller's reaction; assistant messages only.
    pub my_reaction: Option<Reaction>,
}
