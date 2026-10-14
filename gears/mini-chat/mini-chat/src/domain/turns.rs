//! The turn read model and the turn status projection (feature
//! `cpt-cf-mini-chat-feature-message-streaming`, slice S1-setup-path): the `chat_turns` row as
//! the send pipeline and the status API read it, and the state mapping of
//! `cpt-cf-mini-chat-algo-message-streaming-turn-status`.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// `chat_turns.state`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TurnState {
    /// The stored value: `running`, `completed`, `failed` or `cancelled`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The state of a stored value, `None` for an unknown one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Running,
            Self::Completed,
            Self::Failed,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|state| state.as_str() == value)
    }
}

/// A `chat_turns` row as the send pipeline (admission checks, replay) and the turn status API
/// read it. Soft-deleted rows are returned by the key lookup of the admission checks and carry
/// `deleted_at`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
    pub id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Option<Uuid>,
    pub state: TurnState,
    pub assistant_message_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub effective_model: Option<String>,
    pub deleted_at: Option<OffsetDateTime>,
    pub started_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The turn state as the status endpoint reports it (`TurnStatusState`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

impl TurnStatusState {
    /// The wire value: `running`, `done`, `error` or `cancelled`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The `TurnStatusResponse` read model: no `chat_id`, no billing outcome, no settlement, no
/// reserve and no provider identifier.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    /// Only for the `error` state.
    pub error_code: Option<String>,
    /// For `done`, and for `cancelled` with a persisted partial message.
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

/// The turn status projection of a non-deleted turn
/// (`cpt-cf-mini-chat-algo-message-streaming-turn-status`).
#[must_use]
pub fn project_turn_status(row: &TurnRow) -> TurnStatusView {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-match
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-running
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-done
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-error
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-cancelled
    let state = match row.state {
        TurnState::Running => TurnStatusState::Running,
        TurnState::Completed => TurnStatusState::Done,
        TurnState::Failed => TurnStatusState::Error,
        TurnState::Cancelled => TurnStatusState::Cancelled,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-cancelled
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-error
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-done
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-running
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-match
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-error-code
    // the code of a failed turn only; a stray code on another state is never reported
    let error_code = match state {
        TurnStatusState::Error => row.error_code.clone(),
        _ => None,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-error-code
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-message-id
    // `done`, and `cancelled` with a persisted partial message; never while running or on error
    let assistant_message_id = match state {
        TurnStatusState::Done | TurnStatusState::Cancelled => row.assistant_message_id,
        TurnStatusState::Running | TurnStatusState::Error => None,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-message-id
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-base-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-no-internal
    // no `chat_id` (it is in the path), no billing outcome, settlement, reserve or provider
    // identifier: the read model has no such field
    let view = TurnStatusView {
        request_id: row.request_id,
        state,
        error_code,
        assistant_message_id,
        updated_at: row.updated_at,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-no-internal
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-base-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-return
    view
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-status:p1:inst-ts-return
}

#[cfg(test)]
#[path = "turns_tests.rs"]
mod turns_tests;
