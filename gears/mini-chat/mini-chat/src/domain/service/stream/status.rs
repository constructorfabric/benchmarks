//! Turn status (`cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`).

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::types::{TurnRecord, TurnState};

/// The projection of `GET /v1/chats/{id}/turns/{request_id}`: never the chat
/// id, provider ids, billing outcome, error detail or reserve fields.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-status-non-exposure
pub struct TurnStatus {
    /// Request id.
    pub request_id: Uuid,
    /// Internal state (the API maps it with [`TurnState::api_state`]).
    pub state: TurnState,
    /// Present only for a failed turn with a stored code.
    pub error_code: Option<String>,
    /// Present for a completed turn, and for a cancelled one only when a
    /// partial message was persisted.
    pub assistant_message_id: Option<Uuid>,
    /// Last update.
    pub updated_at: OffsetDateTime,
}
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-status-non-exposure

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-turn-status:p1
/// Projects a turn row.
#[must_use]
pub fn project_status(turn: &TurnRecord) -> TurnStatus {
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-match-state
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-error
    // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-poll-error
    let error_code = if turn.state == TurnState::Failed {
        turn.error_code.clone()
    } else {
        None
    };
    // @cpt-end:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-poll-error
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-error
    let assistant_message_id = match turn.state {
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-done
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-cancelled
        TurnState::Completed | TurnState::Cancelled => turn.assistant_message_id,
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-cancelled
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-done
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-running
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-poll-running
        TurnState::Running | TurnState::Failed => None,
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-poll-running
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-case-running
    };
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-match-state
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-return-status
    TurnStatus {
        request_id: turn.request_id,
        state: turn.state,
        error_code,
        assistant_message_id,
        updated_at: turn.updated_at,
    }
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-return-status
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
