//! Idempotent replay of a completed turn: a pure read-and-relay path with no
//! access to settlement, outbox or provider calls (DESIGN §4 "Idempotency Rules").

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::AccessScope;

use super::{DoneData, StreamEvent};
use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{chat_turns, chats, messages};
use crate::infra::llm::DeltaKind;

/// Build the replay events from stored rows.
#[must_use]
pub fn replay_events(chat: &chats::Model, turn: &chat_turns::Model, msg: &messages::Model) -> Vec<StreamEvent> {
    let effective = msg
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| chat.model.clone());
    let downgraded = effective != chat.model;
    vec![
        StreamEvent::StreamStarted {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            thread_summary_applied: None,
        },
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: msg.content.clone(),
        },
        StreamEvent::Done(Box::new(DoneData {
            input_tokens: msg.input_tokens,
            output_tokens: msg.output_tokens,
            effective_model: effective,
            selected_model: chat.model.clone(),
            downgraded,
            downgrade_from: downgraded.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        })),
    ]
}

impl MiniChat {
    /// Load the stored assistant message of a completed turn and build the replay.
    ///
    /// # Errors
    /// Database failure, or internal error when the content is missing.
    pub async fn replay(&self, chat: &chats::Model, turn: &chat_turns::Model) -> Result<Vec<StreamEvent>, DomainError> {
        let conn = self.db.conn()?;
        let msg_id = turn
            .assistant_message_id
            .ok_or_else(|| DomainError::Internal("completed turn without assistant message".into()))?;
        let msg = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::Id.eq(msg_id))
                    .add(messages::Column::ChatId.eq(chat.id)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::Internal("assistant message of a completed turn is missing".into()))?;
        Ok(replay_events(chat, turn, &msg))
    }
}
