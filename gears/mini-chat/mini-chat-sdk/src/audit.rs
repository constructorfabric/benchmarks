//! Audit event contract (DESIGN 3.9 "Audit Events for Turn Mutations", ADR-0009).
//!
//! Events are delivered to the audit plugin through the `mini-chat.audit`
//! outbox queue. The JSON form is internally tagged by `event_type`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::usage::UsageTokens;

/// Payload of the turn finalization events (`turn_completed`, `turn_failed`).
///
/// Carries identities, model, token usage, latency, tool-call counts and the
/// quota decision. Prompt, response and attachment content are never part of
/// it (ADR-0009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// Tenant of the turn.
    pub tenant_id: Uuid,
    /// User of the turn.
    pub user_id: Uuid,
    /// Chat of the turn.
    pub chat_id: Uuid,
    /// Request identifier of the turn.
    pub request_id: Uuid,
    /// Model selected for the chat.
    pub selected_model: String,
    /// Model that served the turn.
    pub effective_model: String,
    /// Provider usage, when reported.
    pub usage: Option<UsageTokens>,
    /// Total latency in milliseconds.
    #[serde(default)]
    pub latency_ms: u64,
    /// Completed `web_search` calls.
    #[serde(default)]
    pub web_search_calls: u32,
    /// Completed file-search calls.
    #[serde(default)]
    pub file_search_calls: u32,
    /// Quota decision label (`allow` or the downgrade reason).
    #[serde(default)]
    pub quota_decision: String,
    /// Terminal error code (failed turns only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event delivered to `MiniChatAuditPluginClientV1::emit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type")]
#[allow(
    clippy::enum_variant_names,
    reason = "variant names mirror the `turn_*` wire discriminators"
)]
pub enum AuditEvent {
    /// A turn finished in state `completed`.
    #[serde(rename = "turn_completed")]
    TurnCompleted(TurnAuditEvent),
    /// A turn finished in any other terminal state (failed, cancelled, orphan).
    #[serde(rename = "turn_failed")]
    TurnFailed(TurnAuditEvent),
    /// The last turn was retried.
    #[serde(rename = "turn_retry")]
    TurnRetry {
        /// Tenant of the chat.
        tenant_id: Uuid,
        /// User who retried.
        actor_user_id: Uuid,
        /// Chat of the turn.
        chat_id: Uuid,
        /// Request id of the replaced turn.
        original_request_id: Uuid,
        /// Request id of the new turn.
        new_request_id: Uuid,
        /// Event time.
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// The last turn was edited.
    #[serde(rename = "turn_edit")]
    TurnEdit {
        /// Tenant of the chat.
        tenant_id: Uuid,
        /// User who edited.
        actor_user_id: Uuid,
        /// Chat of the turn.
        chat_id: Uuid,
        /// Request id of the replaced turn.
        original_request_id: Uuid,
        /// Request id of the new turn.
        new_request_id: Uuid,
        /// Event time.
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// The last turn was deleted.
    #[serde(rename = "turn_delete")]
    TurnDelete {
        /// Tenant of the chat.
        tenant_id: Uuid,
        /// User who deleted.
        actor_user_id: Uuid,
        /// Chat of the turn.
        chat_id: Uuid,
        /// Request id of the deleted turn.
        request_id: Uuid,
        /// Event time.
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
}

impl AuditEvent {
    /// `event_type` of a completed turn.
    pub const TURN_COMPLETED: &'static str = "turn_completed";
    /// `event_type` of a failed, cancelled or orphaned turn.
    pub const TURN_FAILED: &'static str = "turn_failed";
    /// `event_type` of a retry.
    pub const TURN_RETRY: &'static str = "turn_retry";
    /// `event_type` of an edit.
    pub const TURN_EDIT: &'static str = "turn_edit";
    /// `event_type` of a delete.
    pub const TURN_DELETE: &'static str = "turn_delete";

    /// The `event_type` discriminator of this event.
    #[must_use]
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::TurnCompleted(_) => Self::TURN_COMPLETED,
            Self::TurnFailed(_) => Self::TURN_FAILED,
            Self::TurnRetry { .. } => Self::TURN_RETRY,
            Self::TurnEdit { .. } => Self::TURN_EDIT,
            Self::TurnDelete { .. } => Self::TURN_DELETE,
        }
    }
}
