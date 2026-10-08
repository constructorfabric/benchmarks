//! Idempotent replay of a completed turn (DESIGN §3.3 "Idempotency", ADR-0010). Pure read-and-relay:
//! no provider call, no reserve, no settlement, no outbox event.

use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::domain::stream::events::{DeltaData, DeltaKind, DoneData, StreamEvent, StreamStartedData, Usage};
use crate::domain::stream::queries;
use crate::infra::db::entities::{chat, chat_turn};

/// Events of the replay stream: `stream_started` (`is_new_turn: false`), one `delta`, `done`.
///
/// # Errors
/// Internal error when the completed turn has no persisted assistant message.
pub async fn replay_events(app: &AppServices, chat: &chat::Model, turn: &chat_turn::Model) -> Result<Vec<StreamEvent>, DomainError> {
    let msg_id = turn
        .assistant_message_id
        .ok_or_else(|| DomainError::internal(format!("completed turn {} has no assistant message", turn.id)))?;
    let conn = app.db.conn()?;
    let msg = queries::message_by_id(&conn, turn.tenant_id, turn.chat_id, msg_id)
        .await?
        .ok_or_else(|| DomainError::internal(format!("assistant message {msg_id} not found")))?;
    let effective = msg.model.clone().or_else(|| turn.effective_model.clone()).unwrap_or_else(|| chat.model.clone());
    let downgraded = effective != chat.model;
    Ok(vec![
        StreamEvent::StreamStarted(StreamStartedData {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        StreamEvent::Delta(DeltaData { kind: DeltaKind::Text, content: msg.content.clone() }),
        StreamEvent::Done(DoneData {
            usage: Usage { input_tokens: msg.input_tokens, output_tokens: msg.output_tokens },
            effective_model: effective,
            selected_model: chat.model.clone(),
            quota_decision: if downgraded { "downgrade" } else { "allow" }.to_owned(),
            downgrade_from: downgraded.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        }),
    ])
}
