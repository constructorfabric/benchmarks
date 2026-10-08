//! Idempotent replay of a completed turn (DESIGN "Replay is side-effect-free invariant",
//! ADR-0010): `stream_started` (`is_new_turn: false`), one `delta` with the stored answer, then
//! `done` rebuilt from the stored models. This module only reads; it has no access to the quota,
//! the outbox or the provider.

use futures::stream;
use toolkit_db::secure::{AccessScope, DBRunner};
use uuid::Uuid;

use super::events::{DeltaKind, DonePayload, EventStream, StreamEvent, UsageDto};
use crate::domain::error::DomainError;
use crate::domain::quota::QuotaDecision;
use crate::infra::db::entity::{chat_turns, chats};
use crate::infra::db::repo::messages as message_repo;

/// The stored outcome of a completed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub content: String,
    pub usage: UsageDto,
    pub effective_model: String,
    pub selected_model: String,
}

/// Reads the assistant message of the completed `turn` of `chat`.
///
/// # Errors
/// `Internal` when the completed turn has no live assistant message, or on a database error.
pub async fn load(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: &chats::Model,
    turn: &chat_turns::Model,
) -> Result<Replay, DomainError> {
    let missing = || {
        DomainError::Internal(format!(
            "completed turn {} has no assistant message",
            turn.id
        ))
    };
    let message_id = turn.assistant_message_id.ok_or_else(missing)?;
    let message = message_repo::find(conn, scope, chat.id, message_id)
        .await?
        .ok_or_else(missing)?;
    Ok(Replay {
        request_id: turn.request_id,
        message_id,
        content: message.content,
        usage: UsageDto {
            input_tokens: message.input_tokens,
            output_tokens: message.output_tokens,
        },
        effective_model: message
            .model
            .or_else(|| turn.effective_model.clone())
            .unwrap_or_else(|| chat.model.clone()),
        selected_model: chat.model.clone(),
    })
}

/// The replay events. `quota_decision` is `downgrade` iff the stored effective model differs
/// from the chat's model; `downgrade_reason` and `quota_warnings` are not replayed.
#[must_use]
pub fn events(r: Replay) -> EventStream {
    let downgraded = r.effective_model != r.selected_model;
    let decision = if downgraded {
        QuotaDecision::Downgrade
    } else {
        QuotaDecision::Allow
    };
    let downgrade_from = downgraded.then(|| r.selected_model.clone());
    let done = DonePayload {
        usage: r.usage,
        effective_model: r.effective_model,
        selected_model: r.selected_model,
        quota_decision: decision.as_str(),
        downgrade_from,
        downgrade_reason: None,
        quota_warnings: None,
    };
    Box::pin(stream::iter([
        StreamEvent::StreamStarted {
            request_id: r.request_id,
            message_id: r.message_id,
            is_new_turn: false,
            thread_summary_applied: None,
        },
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: r.content,
        },
        StreamEvent::Done(done),
    ]))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    fn replay(effective: &str) -> Replay {
        Replay {
            request_id: Uuid::from_u128(1),
            message_id: Uuid::from_u128(2),
            content: "answer".into(),
            usage: UsageDto {
                input_tokens: 3,
                output_tokens: 4,
            },
            effective_model: effective.into(),
            selected_model: "premium".into(),
        }
    }

    #[tokio::test]
    async fn downgrade_is_rebuilt_from_the_stored_models() {
        let got: Vec<StreamEvent> = events(replay("standard")).collect().await;
        let StreamEvent::Done(done) = &got[2] else {
            panic!("{got:?}")
        };
        assert_eq!(done.quota_decision, "downgrade");
        assert_eq!(done.downgrade_from.as_deref(), Some("premium"));
        assert!(done.downgrade_reason.is_none() && done.quota_warnings.is_none());

        let got: Vec<StreamEvent> = events(replay("premium")).collect().await;
        let names: Vec<&str> = got.iter().map(StreamEvent::event_name).collect();
        assert_eq!(names, ["stream_started", "delta", "done"]);
        let StreamEvent::Done(done) = &got[2] else {
            panic!("{got:?}")
        };
        assert_eq!(done.quota_decision, "allow");
        assert!(done.downgrade_from.is_none());
    }
}
