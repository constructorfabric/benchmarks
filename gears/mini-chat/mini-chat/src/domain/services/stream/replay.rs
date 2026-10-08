//! Idempotent replay of a completed turn (DESIGN §4 "Idempotency Rules",
//! "Replay Side-Effect-Free Invariant"; ADR-0010).
//!
//! A separate, read-only code path: it only reads the assistant message and has
//! no access to quota, outbox or provider functions.

use toolkit_db::secure::DBRunner;

use crate::api::rest::dto::{
    DeltaData, DeltaKind, DoneData, QuotaDecisionKind, StreamStartedData, Usage,
};
use crate::api::rest::sse::SseEvent;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::db::entities::{chat, chat_turn};
use crate::infra::db::repos::MessageRepo;

/// `stream_started{is_new_turn: false}`, one `delta` with the full persisted
/// text and `done` rebuilt from the stored models (no `downgrade_reason`, no
/// `quota_warnings`, no citations).
///
/// # Errors
/// `Internal` when the completed turn has no persisted assistant message;
/// database failures.
pub(super) async fn replay_events(
    runner: &impl DBRunner,
    chat: &chat::Model,
    turn: &chat_turn::Model,
) -> DomainResult<Vec<SseEvent>> {
    let missing = || {
        DomainError::internal(format!(
            "completed turn {} has no persisted assistant message",
            turn.id
        ))
    };
    let message_id = turn.assistant_message_id.ok_or_else(missing)?;
    let message = MessageRepo::find_live(runner, turn.tenant_id, turn.chat_id, message_id)
        .await?
        .ok_or_else(missing)?;

    let selected = chat.model.clone().unwrap_or_default();
    let effective = message
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| selected.clone());
    let downgrade = effective != selected;
    Ok(vec![
        SseEvent::StreamStarted(StreamStartedData {
            request_id: turn.request_id,
            message_id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        SseEvent::Delta(DeltaData {
            kind: DeltaKind::Text,
            content: message.content,
        }),
        SseEvent::Done(DoneData {
            usage: Usage {
                input_tokens: message.input_tokens,
                output_tokens: message.output_tokens,
            },
            effective_model: effective,
            selected_model: selected.clone(),
            quota_decision: if downgrade {
                QuotaDecisionKind::Downgrade
            } else {
                QuotaDecisionKind::Allow
            },
            downgrade_from: downgrade.then_some(selected),
            downgrade_reason: None,
            quota_warnings: None,
        }),
    ])
}
