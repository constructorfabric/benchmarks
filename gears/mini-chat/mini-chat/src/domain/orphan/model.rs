//! Orphan candidates.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The preflight facts persisted with a turn's reserve. Present only when every one of the six
/// reserve columns is set.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanReserve {
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: i64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: i64,
    /// `chat_turns.reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// `chat_turns.policy_version_applied`.
    pub policy_version: i64,
    /// `chat_turns.effective_model`.
    pub effective_model: String,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i64,
}

/// A `running` turn found without recent progress.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanCandidate {
    /// The turn.
    pub turn_id: Uuid,
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The idempotency key.
    pub request_id: Uuid,
    /// The requesting user.
    pub user_id: Option<Uuid>,
    /// When the turn started (selects the quota period rows of its reserve).
    pub started_at: OffsetDateTime,
    /// `chat_turns.last_progress_at`; `None` when the turn never reported progress.
    pub last_progress_at: Option<OffsetDateTime>,
    /// Completed `web_search` calls recorded so far.
    pub web_search_calls: u32,
    /// Completed `code_interpreter` calls recorded so far.
    pub code_interpreter_calls: u32,
    /// Completed `file_search` calls recorded so far.
    pub file_search_calls: u32,
    /// The reserve of the turn; `None` when the turn never booked one.
    pub reserve: Option<OrphanReserve>,
}
