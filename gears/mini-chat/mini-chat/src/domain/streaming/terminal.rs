//! The terminal emission matrix (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`, algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-terminal-emission`): which events the relay gets
//! once the finalization result is known.

use mini_chat_sdk::UsageTokens;
use toolkit_macros::domain_model;

use crate::domain::error::{DomainError, StreamErrorCode};
use crate::domain::quota::QuotaWarning;
use crate::domain::streaming::finalizer::StreamFinalization;
use crate::domain::streaming::plan::ProviderCallPlan;
use crate::domain::streaming::provider_errors::generic_message;
use crate::domain::streaming::relay::{CitationsPayload, ErrorEvent, RelayEvent};
use crate::domain::streaming::replay::Done;
use crate::domain::turns::TurnState;

/// What the stream wanted to end with, before the finalization result is known.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalIntent {
    /// The provider completed (or was incomplete): `done` after the commit.
    Completed { usage: Option<UsageTokens> },
    /// A provider failure, a tool-limit breach or an unexpected tool use: `error` with the
    /// sanitized message.
    Failed {
        code: StreamErrorCode,
        message: String,
    },
    /// The client went away.
    Cancelled,
}

/// The events to send after the finalization: committed `completed` gives `citations` (when
/// mapped) then `done` (usage, models, quota decision, `downgrade_from` and `downgrade_reason`
/// only for a downgrade, `quota_warnings` when read; no internal id); committed `failed` with
/// `message_persistence_failed` gives that `error`; committed `failed` otherwise gives the
/// intent's `error`; a finalization failure gives `finalization_failed` for a completed intent
/// and the original error for a failed intent; a lost CAS and a cancelled turn give nothing
/// (the relay synthesizes `stream_interrupted` for a lost CAS while the client is connected).
#[must_use]
pub fn terminal_emission(
    plan: &ProviderCallPlan,
    intent: &TerminalIntent,
    finalization: &Result<StreamFinalization, DomainError>,
    citations: Option<CitationsPayload>,
    warnings: Option<Vec<QuotaWarning>>,
) -> Vec<RelayEvent> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-hold
    // the provider terminal is held back until the finalization result is known: this function
    // runs after it; `delta`, `tool` and `citations` of a running stream are not gated
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-hold
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-match
    match finalization {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-finalization-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-error-path-failure
        Err(_) => match intent {
            // a failure on a completed or incomplete stream is never `done`
            TerminalIntent::Completed { .. } => vec![error_event(
                StreamErrorCode::FinalizationFailed,
                generic_message(StreamErrorCode::FinalizationFailed),
            )],
            // an error path keeps its original code
            TerminalIntent::Failed { code, message } => vec![error_event(*code, message)],
            TerminalIntent::Cancelled => Vec::new(),
        },
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-error-path-failure
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-finalization-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-cas-lost
        // nothing from the provider task: the relay synthesizes `stream_interrupted`
        Ok(StreamFinalization::AlreadyFinalized) => Vec::new(),
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-cas-lost
        Ok(StreamFinalization::Committed(committed)) => match committed.state {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-done
            TurnState::Completed => match intent {
                TerminalIntent::Completed { usage } => {
                    let mut events = Vec::new();
                    if let Some(citations) = citations {
                        events.push(RelayEvent::Citations(citations));
                    }
                    events.push(RelayEvent::Done(done_of(plan, usage.as_ref(), warnings)));
                    events
                }
                _ => Vec::new(),
            },
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-done
            TurnState::Failed => {
                if committed.error_code.as_deref()
                    == Some(StreamErrorCode::MessagePersistenceFailed.as_str())
                {
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-persistence-failed
                    return vec![error_event(
                        StreamErrorCode::MessagePersistenceFailed,
                        generic_message(StreamErrorCode::MessagePersistenceFailed),
                    )];
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-persistence-failed
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-error
                match intent {
                    TerminalIntent::Failed { code, message } => vec![error_event(*code, message)],
                    _ => Vec::new(),
                }
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-error
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-cancelled
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-no-event
            // the stream is already broken: nothing is emitted, neither `error` nor anything else
            TurnState::Cancelled | TurnState::Running => Vec::new(),
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-no-event
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-cancelled
        },
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-match
}

fn error_event(code: StreamErrorCode, message: &str) -> RelayEvent {
    RelayEvent::Error(ErrorEvent {
        code,
        message: message.to_owned(),
    })
}

/// The `done` payload of a committed completed turn.
fn done_of(
    plan: &ProviderCallPlan,
    usage: Option<&UsageTokens>,
    warnings: Option<Vec<QuotaWarning>>,
) -> Done {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-no-internal-ids
    // `request_id`, `message_id`, `turn_id`, the provider response id, the reserve and the
    // billing outcome are not part of `done`; cache and reasoning counts are never serialized;
    // a failed warnings read arrives as `None` and `done` goes without `quota_warnings`
    let tokens = |count: u64| i64::try_from(count).unwrap_or(i64::MAX);
    Done {
        input_tokens: usage.map_or(0, |u| tokens(u.input_tokens)),
        output_tokens: usage.map_or(0, |u| tokens(u.output_tokens)),
        effective_model: plan.decision.effective_model.clone(),
        selected_model: plan.selected_model.clone(),
        quota_decision: plan.decision.quota_decision,
        downgrade_from: plan.decision.downgrade_from.clone(),
        downgrade_reason: plan
            .decision
            .downgrade_reason
            .map(|reason| reason.as_str().to_owned()),
        quota_warnings: warnings,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-no-internal-ids
}
