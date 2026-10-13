//! Completed turn replay
//! (`cpt-cf-mini-chat-algo-streaming-turns-replay-completed`): a code path
//! that has no access to the quota, the outbox, the provider or the metrics.

use toolkit_security::AccessScope;

use super::ports::{StoredAssistantMessage, TurnRepository};
use super::types::{DeltaKind, DoneEvent, StreamEvent, StreamStarted, TurnRecord};
use crate::domain::error::DomainError;
use crate::domain::model::Chat;
use crate::domain::service::quota::preflight::QuotaDecision;

/// Builds the three replay events from the stored data: `stream_started`
/// (`is_new_turn` false, `message_id` the assistant message), one `delta`
/// `text` with the full content, and `done` with the rebuilt decision.
#[must_use]
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-replay-side-effect-free
pub fn build_replay(
    chat: &Chat,
    turn: &TurnRecord,
    assistant: &StoredAssistantMessage,
) -> Vec<StreamEvent> {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-rebuild-decision
    let selected_model = chat.model.clone();
    let effective_model = turn
        .effective_model
        .clone()
        .unwrap_or_else(|| selected_model.clone());
    let downgraded = effective_model != selected_model;
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-rebuild-decision
    vec![
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-send-replay-started
        StreamEvent::StreamStarted(StreamStarted {
            request_id: turn.request_id,
            message_id: assistant.id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-send-replay-started
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-send-replay-delta
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: assistant.content.clone(),
        },
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-send-replay-delta
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-send-replay-done
        StreamEvent::Done(DoneEvent {
            usage: (assistant.input_tokens, assistant.output_tokens),
            effective_model,
            selected_model: selected_model.clone(),
            quota_decision: if downgraded {
                QuotaDecision::Downgrade
            } else {
                QuotaDecision::Allow
            },
            downgrade_from: downgraded.then_some(selected_model),
            downgrade_reason: None,
            quota_warnings: None,
        }),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-send-replay-done
    ]
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-replay-side-effect-free

/// Loads the assistant message and builds the replay.
///
/// # Errors
///
/// [`DomainError::Internal`] when the completed turn has no assistant message
/// (corrupt data, logged at `error`), and the errors of the repository.
pub async fn replay_completed(
    repo: &dyn TurnRepository,
    scope: &AccessScope,
    chat: &Chat,
    turn: &TurnRecord,
) -> Result<Vec<StreamEvent>, DomainError> {
    let assistant = match turn.assistant_message_id {
        Some(message_id) => {
            repo.load_assistant_message(scope, chat.id, message_id)
                .await?
        }
        None => None,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-if-assistant-missing
    let Some(assistant) = assistant else {
        tracing::error!(
            chat_id = %chat.id,
            turn_id = %turn.id,
            request_id = %turn.request_id,
            "a completed turn has no assistant message"
        );
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-return-replay-internal
        return Err(DomainError::Internal(
            "a completed turn has no assistant message".to_owned(),
        ));
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-return-replay-internal
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-if-assistant-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-return-replay-stream
    Ok(build_replay(chat, turn, &assistant))
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-return-replay-stream
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
