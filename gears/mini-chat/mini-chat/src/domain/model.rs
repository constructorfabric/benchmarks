//! Domain entities of the mini-chat gear.
//!
//! Plain data without persistence or transport types: the `SeaORM` entities
//! live in `infra::db::entities`, the REST DTOs in `api::rest`.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// A chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    /// Chat id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning user.
    pub user_id: Uuid,
    /// Model selected at creation, immutable for the chat lifetime.
    pub model: String,
    /// Title, if any.
    pub title: Option<String>,
    /// Whether the chat is temporary.
    pub is_temporary: bool,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last activity time.
    pub updated_at: OffsetDateTime,
}

/// A message of a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Message id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Client idempotency key of the request that produced the message.
    pub request_id: Option<Uuid>,
    /// `user`, `assistant` or `system`.
    pub role: String,
    /// Message content.
    pub content: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A turn of a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurn {
    /// Turn id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Client idempotency key.
    pub request_id: Uuid,
    /// `running`, `completed`, `failed` or `cancelled`.
    pub state: String,
    /// Creation time (application clock).
    pub started_at: OffsetDateTime,
}

/// An attachment of a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Attachment id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Original file name.
    pub filename: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// `document` or `image`.
    pub kind: String,
}

/// Link between a message and an attachment of the same chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAttachment {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Chat of both linked rows.
    pub chat_id: Uuid,
    /// Linked message.
    pub message_id: Uuid,
    /// Linked attachment.
    pub attachment_id: Uuid,
    /// Association creation time.
    pub created_at: OffsetDateTime,
}

/// Committed thread summary of a chat (at most one per chat).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    /// Summary id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Summarized chat.
    pub chat_id: Uuid,
    /// Summary text.
    pub summary_text: String,
    /// Inclusive frontier: creation time of the last summarized message.
    pub summarized_up_to_created_at: OffsetDateTime,
    /// Inclusive frontier: id of the last summarized message.
    pub summarized_up_to_message_id: Uuid,
}

/// Provider vector store bound to a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatVectorStore {
    /// Record id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning chat.
    pub chat_id: Uuid,
    /// Provider vector store id; `None` while creation is in progress.
    pub vector_store_id: Option<String>,
    /// Storage backend label of the provider.
    pub provider: String,
}

/// Quota counters of one `(user, period, bucket)`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaUsage {
    /// Record id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// User.
    pub user_id: Uuid,
    /// `daily` or `monthly`.
    pub period_type: String,
    /// `total` or `tier:premium`.
    pub bucket: String,
    /// Settled credits in micro-credits.
    pub spent_credits_micro: i64,
    /// In-flight reserved credits in micro-credits.
    pub reserved_credits_micro: i64,
}

/// Reaction of a user to an assistant message.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageReaction {
    /// Reaction id.
    pub id: Uuid,
    /// Reacted message.
    pub message_id: Uuid,
    /// Reacting user.
    pub user_id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// `like` or `dislike`.
    pub reaction: String,
}
