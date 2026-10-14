//! The replay stream (`cpt-cf-mini-chat-algo-message-streaming-replay`): the three events of a
//! side-effect-free replay of a completed turn.

use mini_chat_sdk::QuotaDecision;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::messages::MessageRow;
use crate::domain::quota::QuotaWarning;
use crate::domain::turns::TurnRow;

/// `stream_started`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStarted {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    /// The stored `token_estimate` of the applied thread summary, only for a new turn that used
    /// one.
    pub thread_summary_token_estimate: Option<i32>,
}

/// The kind of a `delta`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// `delta`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub kind: DeltaKind,
    pub content: String,
}

/// `done`.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecision,
    /// Equal to `selected_model`, only for a downgrade.
    pub downgrade_from: Option<String>,
    /// Never set by a replay.
    pub downgrade_reason: Option<String>,
    /// Never set by a replay.
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// An event of the stream; the provider-driven events (`tool`, `citations`, `error`, `ping`) are
/// added by slice S2.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted(StreamStarted),
    Delta(Delta),
    Done(Done),
}

/// The events of the replay of a completed turn: `stream_started` (`is_new_turn` false, the
/// persisted assistant message id, no thread summary), one text `delta` with the full persisted
/// content (also when empty) and `done` (usage from the persisted token counts, the effective
/// model of the turn, `selected_model` from the chat, `quota_decision` `downgrade` with
/// `downgrade_from` when the two models differ, no `downgrade_reason`, no `quota_warnings`).
///
/// # Errors
///
/// `Internal` when the assistant message is missing, its `request_id` is NULL, or the turn holds
/// no effective model.
pub fn build_replay(
    turn: &TurnRow,
    assistant: Option<&MessageRow>,
    selected_model: &str,
) -> Result<Vec<StreamEvent>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-return-500
    // a completed turn always has an assistant message with its `request_id`; anything else is
    // an inconsistency: a generic 500 and no stream
    let assistant = assistant
        .filter(|message| message.request_id.is_some())
        .ok_or_else(|| {
            tracing::error!(
                turn_id = %turn.id,
                "a completed turn has no replayable assistant message"
            );
            DomainError::internal("a completed turn has no replayable assistant message")
        })?;
    let effective_model = turn.effective_model.clone().ok_or_else(|| {
        tracing::error!(turn_id = %turn.id, "a completed turn has no effective model");
        DomainError::internal("a completed turn has no effective model")
    })?;
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-return-500
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-started
    // the persisted assistant message id, `is_new_turn` false, no thread summary
    let started = StreamEvent::StreamStarted(StreamStarted {
        request_id: turn.request_id,
        message_id: assistant.id,
        is_new_turn: false,
        thread_summary_token_estimate: None,
    });
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-started
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-delta
    // one text delta with the full persisted text, also when it is empty
    let delta = StreamEvent::Delta(Delta {
        kind: DeltaKind::Text,
        content: assistant.content.clone(),
    });
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-delta
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-done
    // `quota_decision` and `downgrade_from` are rebuilt from the two models; neither
    // `downgrade_reason` nor `quota_warnings` is persisted
    let downgraded = effective_model != selected_model;
    let done = StreamEvent::Done(Done {
        input_tokens: assistant.input_tokens,
        output_tokens: assistant.output_tokens,
        effective_model,
        selected_model: selected_model.to_owned(),
        quota_decision: if downgraded {
            QuotaDecision::Downgrade
        } else {
            QuotaDecision::Allow
        },
        downgrade_from: downgraded.then(|| selected_model.to_owned()),
        downgrade_reason: None,
        quota_warnings: None,
    });
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-done
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-no-other-events
    // no `ping`, `tool` or `citations` event: exactly three events
    Ok(vec![started, delta, done])
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-no-other-events
}
