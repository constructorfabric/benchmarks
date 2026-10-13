//! Idempotent replay of a completed turn (DESIGN section 3.3 "Idempotency",
//! section 4 "Idempotency Rules", ADR-0010).
//!
//! A pure read: this module has no access to the quota service, the
//! finalization service or the outbox, so a replay can never reserve, settle
//! or emit events.

use toolkit_db::secure::DBRunner;

use super::events::DeltaKind;
use super::events::{DoneData, StreamEvent, StreamStartedData, UsageCounts};
use crate::domain::enums::TurnState;
use crate::domain::error::DomainError;
use crate::domain::services::quota_service::QuotaDecisionKind;
use crate::infra::db::entities::{chat, chat_turn};
use crate::infra::db::repos::message_repo;

/// Replay events for an existing turn of the request id: `stream_started`
/// (`is_new_turn: false`), one `delta` with the persisted text and a rebuilt
/// `done`. `quota_decision` is `downgrade` iff the stored model differs from
/// the chat's model (`downgrade_from` = chat model, no `downgrade_reason`).
///
/// # Errors
/// `RequestIdConflict` for a turn that is not completed or is soft-deleted;
/// `Internal` when a completed turn has no persisted assistant message;
/// database failure.
pub(super) async fn replay_or_conflict(
    runner: &impl DBRunner,
    chat: &chat::Model,
    turn: &chat_turn::Model,
) -> Result<Vec<StreamEvent>, DomainError> {
    if turn.deleted_at.is_some() || TurnState::parse(&turn.state) != Some(TurnState::Completed) {
        return Err(DomainError::RequestIdConflict);
    }
    let message_id = turn.assistant_message_id.ok_or_else(|| {
        DomainError::Internal(format!(
            "completed turn {} has no assistant message",
            turn.id
        ))
    })?;
    let msg = message_repo::find_live(runner, chat.tenant_id, chat.id, message_id)
        .await?
        .ok_or_else(|| {
            DomainError::Internal(format!(
                "assistant message of completed turn {} is missing",
                turn.id
            ))
        })?;
    let effective_model = msg
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| chat.model.clone());
    let downgraded = effective_model != chat.model;
    Ok(vec![
        StreamEvent::StreamStarted(StreamStartedData {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: msg.content,
        },
        StreamEvent::Done(DoneData {
            usage: UsageCounts {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
            },
            effective_model,
            selected_model: chat.model.clone(),
            quota_decision: if downgraded {
                QuotaDecisionKind::Downgrade
            } else {
                QuotaDecisionKind::Allow
            },
            downgrade_from: downgraded.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        }),
    ])
}
