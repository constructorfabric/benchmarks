//! Idempotent replay of a completed turn (S§6.4, D "Replay is side-effect-free
//! invariant", ADR-0010).
//!
//! A separate, read-only code path: it only reads the turn's assistant
//! message, so it cannot reserve quota, settle, enqueue outbox events or call
//! the provider.

use toolkit_db::secure::{AccessScope, DBRunner};

use crate::api::rest::dto::{DoneData, QuotaDecisionKind, StreamStartedData, Usage};
use crate::domain::error::DomainError;
use crate::infra::db::entity::{chat, chat_turn};
use crate::infra::db::repos::MessageRepo;

/// The buffered replay stream: `stream_started`, one `delta` with the full
/// text, `done` (no citations, no `quota_warnings`, no `downgrade_reason`).
#[derive(Debug, Clone)]
pub struct ReplayTurn {
    pub started: StreamStartedData,
    pub text: String,
    pub done: DoneData,
}

/// Rebuild the replay of the completed, non-deleted `turn` of `chat`.
///
/// # Errors
/// `Internal` when the turn has no persisted assistant message (a completed
/// turn always has one); database errors.
pub async fn load(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat: &chat::Model,
    turn: &chat_turn::Model,
) -> Result<ReplayTurn, DomainError> {
    let message_id = turn.assistant_message_id.ok_or_else(|| {
        DomainError::Internal(format!("completed turn {} has no message", turn.id))
    })?;
    let msg = MessageRepo
        .find_by_id(runner, scope, message_id)
        .await?
        .ok_or_else(|| DomainError::Internal(format!("assistant message {message_id} missing")))?;
    let effective = msg
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| chat.model.clone());
    let downgraded = effective != chat.model;
    Ok(ReplayTurn {
        started: StreamStartedData {
            request_id: turn.request_id,
            message_id,
            is_new_turn: false,
            thread_summary_applied: None,
        },
        text: msg.content,
        done: DoneData {
            usage: Usage {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
            },
            effective_model: effective,
            selected_model: chat.model.clone(),
            quota_decision: if downgraded {
                QuotaDecisionKind::Downgrade
            } else {
                QuotaDecisionKind::Allow
            },
            downgrade_from: downgraded.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        },
    })
}
