//! Repository ports of the stream flow.

use std::collections::HashMap;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::model::{AttachmentRef, FinalizeCommand, FinalizeOutcome, ReserveCommand, TurnContext};
use crate::domain::error::DomainError;
use crate::domain::turn::TurnRecord;

/// A persisted assistant message, as a replay reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAssistantMessage {
    /// The message id.
    pub id: Uuid,
    /// The stored text.
    pub content: String,
    /// Provider-reported input tokens.
    pub input_tokens: i64,
    /// Provider-reported output tokens.
    pub output_tokens: i64,
}

/// Persistence of turns. Every method applies the caller's scope.
#[async_trait]
pub trait TurnRepository: Send + Sync {
    /// The turn with the idempotency key, soft-deleted ones included.
    async fn find_by_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError>;

    /// A running, non-deleted turn of the chat.
    async fn find_running(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError>;

    /// The persisted assistant message of a completed turn.
    async fn assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError>;

    /// The reserve transaction: quota reserve with limit re-check, user message, attachment links,
    /// chat touch and the `running` turn. Any failure rolls everything back.
    async fn reserve(
        &self,
        scope: &AccessScope,
        command: ReserveCommand,
    ) -> Result<(), DomainError>;

    /// The finalization transaction of one terminal path.
    async fn finalize(
        &self,
        scope: &AccessScope,
        command: FinalizeCommand,
    ) -> Result<FinalizeOutcome, DomainError>;

    /// Refreshes `last_progress_at` of a running turn.
    async fn touch_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Increments the completed-call counter of a built-in tool on a running turn.
    async fn record_tool_completed(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        tool: &str,
    ) -> Result<(), DomainError>;
}

/// Read access to the conversation of a chat.
#[async_trait]
pub trait ConversationRepository: Send + Sync {
    /// Everything the preflight reads: prior context tokens, thread summary, recent messages
    /// (snapshot boundary, summary frontier, `recent_messages_limit`) and the chat's attachment
    /// facts. The messages of `exclude_request` are left out of the history, the prior context
    /// and the boundary: a retry or edit reads the conversation without the turn it replaces
    /// (preflight) or without its own, already stored, user message (assembly).
    async fn turn_context(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        recent_messages_limit: u32,
        exclude_request: Option<Uuid>,
    ) -> Result<TurnContext, DomainError>;

    /// The attachments with the given ids that are ready, not deleted, in the chat and uploaded by
    /// `user_id`. Missing ids are simply absent from the result.
    async fn ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<AttachmentRef>, DomainError>;

    /// `provider_file_id -> (attachment id, filename)` of the chat's ready documents.
    async fn file_citation_map(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<HashMap<String, (Uuid, String)>, DomainError>;
}
