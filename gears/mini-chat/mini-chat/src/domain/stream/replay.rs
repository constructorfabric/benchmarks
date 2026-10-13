//! Replay of a completed turn: a pure read-and-relay path (DESIGN "Replay is side-effect-free
//! invariant", ADR-0010). It holds no quota, settlement or outbox collaborator on purpose.

use std::sync::Arc;

use tokio::sync::mpsc;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;

use super::model::{DeltaKind, DonePayload, StreamEvent};
use super::ports::TurnRepository;
use crate::domain::error::DomainError;
use crate::domain::turn::TurnRecord;

/// Builds the replay stream of a completed turn from the stored rows.
#[domain_model]
pub struct ReplayService {
    turns: Arc<dyn TurnRepository>,
}

impl ReplayService {
    /// Creates the service.
    #[must_use]
    pub fn new(turns: Arc<dyn TurnRepository>) -> Self {
        Self { turns }
    }

    /// The events of the replay: `stream_started` (`is_new_turn = false`), one `delta` with the
    /// persisted text and `done`. Citations are not persisted and are not replayed;
    /// `downgrade_reason` is not persisted and is omitted.
    ///
    /// # Errors
    /// An internal error when the completed turn has no persisted assistant message.
    pub async fn events(
        &self,
        scope: &AccessScope,
        turn: &TurnRecord,
        selected_model: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, DomainError> {
        let message_id = turn
            .assistant_message_id
            .ok_or_else(|| DomainError::internal("a completed turn has no assistant message"))?;
        let message = self
            .turns
            .assistant_message(scope, turn.chat_id, message_id)
            .await?
            .ok_or_else(|| {
                DomainError::internal("the assistant message of a completed turn is missing")
            })?;
        let effective_model = turn
            .effective_model
            .clone()
            .unwrap_or_else(|| selected_model.to_owned());
        let downgraded = effective_model != selected_model;
        let (tx, rx) = mpsc::channel(4);
        let events = [
            StreamEvent::Started {
                request_id: turn.request_id,
                message_id: message.id,
                is_new_turn: false,
                thread_summary_tokens: None,
            },
            StreamEvent::Delta {
                kind: DeltaKind::Text,
                content: message.content,
            },
            StreamEvent::Done(DonePayload {
                input_tokens: message.input_tokens,
                output_tokens: message.output_tokens,
                effective_model,
                selected_model: selected_model.to_owned(),
                quota_decision: if downgraded { "downgrade" } else { "allow" },
                downgrade_from: downgraded.then(|| selected_model.to_owned()),
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ];
        for event in events {
            tx.try_send(event)
                .map_err(|_| DomainError::internal("the replay channel is full"))?;
        }
        Ok(rx)
    }
}
