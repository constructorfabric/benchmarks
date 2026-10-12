//! Turn-mutation types (DESIGN section 3.9 "Turn Mutation Rules").

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::stream::AttachmentRef;

/// The kind of a mutation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOp {
    /// Regenerate the answer of the latest turn.
    Retry,
    /// Replace the user message of the latest turn and regenerate.
    Edit,
    /// Soft-delete the latest turn.
    Delete,
}

impl MutationOp {
    /// The `op` metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }
}

/// The user message of the turn a mutation targets.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalMessage {
    /// The message id.
    pub id: Uuid,
    /// The stored text.
    pub content: String,
    /// The attachments of the message that are not soft-deleted.
    pub attachments: Vec<AttachmentRef>,
}

/// The mutation commit of a retry or an edit: the old turn is replaced by a new `running` turn.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ReplaceCommand {
    /// `Retry` or `Edit`.
    pub op: MutationOp,
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The turn that is replaced.
    pub old_request_id: Uuid,
    /// The new turn's `request_id`.
    pub new_request_id: Uuid,
    /// The new turn.
    pub new_turn_id: Uuid,
    /// The new user message.
    pub new_user_message_id: Uuid,
    /// The text of the new user message.
    pub content: String,
    /// `web_search_enabled` of the old turn, reused.
    pub web_search_enabled: bool,
    /// The instant of the commit (the new turn's start).
    pub now: OffsetDateTime,
}

/// The mutation commit of a delete.
#[domain_model]
#[derive(Debug, Clone)]
pub struct DeleteCommand {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The turn that is deleted.
    pub request_id: Uuid,
    /// The instant of the commit.
    pub now: OffsetDateTime,
}
