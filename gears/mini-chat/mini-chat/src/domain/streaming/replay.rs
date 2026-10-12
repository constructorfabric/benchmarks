//! Replay of a completed turn (DESIGN 3.3 "Idempotency", "Replay is
//! side-effect-free invariant"): a pure read-and-relay path. It has no access
//! to the provider, the quota reserve, the finalization or any outbox; it only
//! reads the turn and the stored assistant message.

use toolkit_db::secure::DBRunner;
use toolkit_security::AccessScope;

use super::{DoneEvent, QuotaDecision, StreamEvent, StreamService, TurnEvents};
use crate::domain::chats::ChatRow;
use crate::domain::error::DomainError;
use crate::domain::ports::MessageRepository;
use crate::domain::turns::{TurnRow, TurnState};

impl StreamService {
    /// Serves an existing `(chat_id, request_id)` turn: a completed,
    /// non-deleted turn is replayed from storage; every other state is
    /// `request_id_conflict`. No catalog lookup is needed.
    pub(super) async fn replay<R: DBRunner>(
        &self,
        conn: &R,
        tenant_scope: &AccessScope,
        chat: &ChatRow,
        turn: &TurnRow,
    ) -> Result<TurnEvents, DomainError> {
        let completed = TurnState::parse(&turn.state)? == TurnState::Completed;
        if !completed || turn.deleted_at.is_some() {
            return Err(DomainError::RequestIdConflict);
        }
        let message_id = turn.assistant_message_id.ok_or_else(|| {
            DomainError::internal(format!(
                "completed turn {} has no assistant message",
                turn.id
            ))
        })?;
        let message = conn
            .find_live_message(tenant_scope, chat.id, message_id)
            .await?
            .ok_or_else(|| {
                DomainError::internal(format!("assistant message of turn {} is missing", turn.id))
            })?;

        let effective_model = turn
            .effective_model
            .clone()
            .or_else(|| message.model.clone())
            .unwrap_or_else(|| chat.model.clone());
        let downgraded = effective_model != chat.model;
        let done = DoneEvent {
            input_tokens: u64::try_from(message.input_tokens).unwrap_or_default(),
            output_tokens: u64::try_from(message.output_tokens).unwrap_or_default(),
            effective_model,
            selected_model: chat.model.clone(),
            quota_decision: if downgraded {
                QuotaDecision::Downgrade
            } else {
                QuotaDecision::Allow
            },
            downgrade_from: downgraded.then(|| chat.model.clone()),
            // The downgrade reason is not persisted, so a replay omits it.
            downgrade_reason: None,
            // The warnings are the finalization's; a replay never carries them.
            quota_warnings: None,
        };
        let mut events = vec![StreamEvent::Started {
            request_id: turn.request_id,
            message_id,
            is_new_turn: false,
            thread_summary_applied: None,
        }];
        if !message.content.is_empty() {
            events.push(StreamEvent::Delta(message.content));
        }
        events.push(StreamEvent::Done(done));
        Ok(TurnEvents::fixed(events))
    }
}
