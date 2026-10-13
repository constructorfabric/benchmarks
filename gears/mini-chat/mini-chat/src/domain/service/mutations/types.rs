//! Shared vocabulary of the turn mutations feature.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::stream::types::TurnRecord;

/// The three mutation operations (the `op` label of the mutation metrics).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MutationOp {
    /// `POST /turns/{request_id}/retry`
    Retry,
    /// `PATCH /turns/{request_id}`
    Edit,
    /// `DELETE /turns/{request_id}`
    Delete,
}

impl MutationOp {
    /// Label value of `mini_chat_turn_mutation{op}`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }
}

/// The `result` label of `mini_chat_turn_mutation`
/// (`cpt-cf-mini-chat-algo-turn-mutations-record-metrics`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MutationResult {
    /// Committed; for retry and edit, the stream was handed over.
    Ok,
    /// 409 `NOT_LATEST_TURN`.
    NotLatestTurn,
    /// 409 `GENERATION_IN_PROGRESS`.
    GenerationInProgress,
    /// 400 `turn_state` (the target is running).
    NotTerminal,
    /// Every other refusal before the commit (404, 403, 400, 429, 503).
    Rejected,
    /// A failure after the commit (the replacement turn was marked failed).
    SetupFailed,
    /// 500 before the commit.
    Error,
}

impl MutationResult {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotLatestTurn => "not_latest_turn",
            Self::GenerationInProgress => "generation_in_progress",
            Self::NotTerminal => "not_terminal",
            Self::Rejected => "rejected",
            Self::SetupFailed => "setup_failed",
            Self::Error => "error",
        }
    }

    /// The label of an error returned before the commit.
    #[must_use]
    pub fn before_commit(error: &DomainError) -> Self {
        match error {
            DomainError::NotLatestTurn => Self::NotLatestTurn,
            DomainError::GenerationInProgress => Self::GenerationInProgress,
            DomainError::TurnNotTerminal => Self::NotTerminal,
            DomainError::Database(_)
            | DomainError::Internal(_)
            | DomainError::PolicyResolution(_)
            | DomainError::ProviderResolution(_)
            | DomainError::UniqueViolation => Self::Error,
            _ => Self::Rejected,
        }
    }
}

/// The `error_code` a failed replacement setup is marked with
/// (`cpt-cf-mini-chat-algo-turn-mutations-fail-setup`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupFailureCode {
    /// `context_length_exceeded`
    ContextLengthExceeded,
    /// `turn_setup_failed`
    TurnSetupFailed,
    /// `quota_exceeded`
    QuotaExceeded,
}

impl SetupFailureCode {
    /// `chat_turns.error_code` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextLengthExceeded => "context_length_exceeded",
            Self::TurnSetupFailed => "turn_setup_failed",
            Self::QuotaExceeded => "quota_exceeded",
        }
    }

    /// The code of a failed setup step.
    #[must_use]
    pub fn of_error(error: &DomainError) -> Self {
        match error {
            DomainError::ContextBudgetExceeded => Self::ContextLengthExceeded,
            DomainError::QuotaExceeded(_) => Self::QuotaExceeded,
            _ => Self::TurnSetupFailed,
        }
    }
}

/// A turn row of the mutation preview: the stream record plus the flag the
/// replacement carries forward.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetTurn {
    /// The turn row (soft-deleted rows included).
    pub turn: TurnRecord,
    /// `chat_turns.web_search_enabled`.
    pub web_search_enabled: bool,
}

/// The live user message of the target turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetMessage {
    /// Message id.
    pub id: Uuid,
    /// Content, as stored.
    pub content: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// The latest live turn of a chat by `(started_at, id)`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestTurn {
    /// Turn id.
    pub id: Uuid,
    /// Start time.
    pub started_at: OffsetDateTime,
}

/// What the mutation preview returns.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationTarget {
    /// The target turn.
    pub turn: TurnRecord,
    /// `web_search_enabled` of the target turn.
    pub web_search_enabled: bool,
    /// The live user message of the target turn.
    pub user_message: TargetMessage,
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
