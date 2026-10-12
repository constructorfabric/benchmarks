//! Chat model.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// A conversation of one user within a tenant.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    /// Chat identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning user.
    pub user_id: Uuid,
    /// The selected model, locked for the lifetime of the chat.
    pub model: String,
    /// Chat title; `None` for an untitled chat.
    pub title: Option<String>,
    /// Temporary-chat flag (reserved for P2; always `false`).
    pub is_temporary: bool,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last activity time.
    pub updated_at: OffsetDateTime,
}

/// A chat together with its live message count.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDetail {
    /// The chat.
    pub chat: Chat,
    /// Number of non-deleted messages.
    pub message_count: u64,
}

/// A chat to insert.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChat {
    /// Chat identifier.
    pub id: Uuid,
    /// Owning tenant, from the security context.
    pub tenant_id: Uuid,
    /// Owning user, from the security context.
    pub user_id: Uuid,
    /// The selected model.
    pub model: String,
    /// Normalized title.
    pub title: Option<String>,
    /// Creation (and first update) time.
    pub now: OffsetDateTime,
}
