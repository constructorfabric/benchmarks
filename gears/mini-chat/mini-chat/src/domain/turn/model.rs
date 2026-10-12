//! Turn records.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The internal state of a turn (`chat_turns.state`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// Generation in progress.
    Running,
    /// The provider completed and the assistant message is persisted.
    Completed,
    /// A terminal failure.
    Failed,
    /// The client disconnected.
    Cancelled,
}

impl TurnState {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a stored value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// The Turn Status API state: `running`, `done`, `error` or `cancelled`.
    #[must_use]
    pub fn api_state(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A `chat_turns` row, as far as the stream and status flows read it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    /// Turn identifier.
    pub id: Uuid,
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The idempotency key.
    pub request_id: Uuid,
    /// Internal state.
    pub state: TurnState,
    /// Terminal error code.
    pub error_code: Option<String>,
    /// The persisted assistant message.
    pub assistant_message_id: Option<Uuid>,
    /// The effective model resolved at preflight.
    pub effective_model: Option<String>,
    /// Soft-deleted (replaced by a mutation or deleted).
    pub deleted: bool,
    /// The user who requested the turn.
    pub requester_user_id: Option<Uuid>,
    /// Whether the request enabled web search (retry and edit reuse it).
    pub web_search_enabled: bool,
    /// When the turn started (the order of the latest-turn rule).
    pub started_at: OffsetDateTime,
    /// Last update.
    pub updated_at: OffsetDateTime,
}

/// The Turn Status API view.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatus {
    /// The turn's request id.
    pub request_id: Uuid,
    /// `running`, `done`, `error` or `cancelled`.
    pub state: &'static str,
    /// The terminal error code of a failed turn.
    pub error_code: Option<String>,
    /// The persisted assistant message.
    pub assistant_message_id: Option<Uuid>,
    /// Last update.
    pub updated_at: OffsetDateTime,
}

impl From<TurnRecord> for TurnStatus {
    fn from(turn: TurnRecord) -> Self {
        Self {
            request_id: turn.request_id,
            state: turn.state.api_state(),
            error_code: if turn.state == TurnState::Failed {
                turn.error_code
            } else {
                None
            },
            assistant_message_id: turn.assistant_message_id,
            updated_at: turn.updated_at,
        }
    }
}
