//! Turns: the `chat_turns` model shared by the stream service and the Turn
//! Status API (DESIGN 3.3 "Turn Status API", 3.7 `chat_turns`, "Turn
//! Lifecycle, Crash Recovery and Orphan Handling").

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::chat_not_found;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{ChatRepository, TurnRepository};

/// Internal state of a turn (`chat_turns.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// Generation in progress.
    Running,
    /// Provider completed (or truncated) and the answer is stored.
    Completed,
    /// Terminal error.
    Failed,
    /// Client disconnected.
    Cancelled,
}

impl TurnState {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses the stored spelling.
    ///
    /// # Errors
    /// `Internal` for a value outside the schema contract.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(DomainError::internal(format!(
                "unknown turn state `{other}`"
            ))),
        }
    }
}

/// State of a turn as the Turn Status API reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    /// `running`.
    Running,
    /// `done` (internal `completed`).
    Done,
    /// `error` (internal `failed`).
    Error,
    /// `cancelled`.
    Cancelled,
}

impl From<TurnState> for TurnStatus {
    fn from(state: TurnState) -> Self {
        match state {
            TurnState::Running => Self::Running,
            TurnState::Completed => Self::Done,
            TurnState::Failed => Self::Error,
            TurnState::Cancelled => Self::Cancelled,
        }
    }
}

/// Built-in tool counted on a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// `file_search`.
    FileSearch,
    /// `web_search`.
    WebSearch,
    /// `code_interpreter`.
    CodeInterpreter,
}

impl ToolKind {
    /// Maps a provider tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "file_search" => Some(Self::FileSearch),
            "web_search" => Some(Self::WebSearch),
            "code_interpreter" => Some(Self::CodeInterpreter),
            _ => None,
        }
    }
}

/// Completed tool calls of a turn (`chat_turns.*_completed_count`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounts {
    /// Completed `web_search` calls.
    pub web_search: u32,
    /// Completed `code_interpreter` calls.
    pub code_interpreter: u32,
    /// Completed file search calls.
    pub file_search: u32,
}

/// A `chat_turns` row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
    /// Turn id.
    pub id: Uuid,
    /// Correlation key within the chat.
    pub request_id: Uuid,
    /// Stored state.
    pub state: String,
    /// Persisted assistant message, when any.
    pub assistant_message_id: Option<Uuid>,
    /// Terminal error code, when any.
    pub error_code: Option<String>,
    /// Model resolved at preflight.
    pub effective_model: Option<String>,
    /// The user the turn was started by (`None` for a system turn).
    pub requester_user_id: Option<Uuid>,
    /// The turn was started with web search enabled.
    pub web_search_enabled: bool,
    /// Soft-delete time (replaced by retry / edit, or deleted).
    pub deleted_at: Option<OffsetDateTime>,
    /// Creation time.
    pub started_at: OffsetDateTime,
    /// Last state change.
    pub updated_at: Option<OffsetDateTime>,
}

/// A turn to insert in the `running` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTurn {
    /// Turn id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Correlation key.
    pub request_id: Uuid,
    /// The requesting user.
    pub requester_user_id: Uuid,
    /// Effective model (catalog id).
    pub effective_model: String,
    /// Policy snapshot version the turn was planned under.
    pub policy_version_applied: u64,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: u64,
    /// Worst-case credits booked in `quota_usage`.
    pub reserved_credits_micro: i64,
    /// `max_output_tokens` sent to the provider.
    pub max_output_tokens_applied: u32,
    /// `min(estimation_budgets.minimal_generation_floor,
    /// max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: u32,
    /// The request enabled web search (stored even when the model has no
    /// web search support).
    pub web_search_enabled: bool,
    /// Creation time.
    pub now: OffsetDateTime,
}

/// A retry / edit turn as the mutation commit inserts it: `running`, without
/// the preflight columns (they are written by the reserve that follows the
/// context assembly, DESIGN 3.7 "Preflight columns on retry/edit").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUnstartedTurn {
    /// Turn id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Server generated correlation key.
    pub request_id: Uuid,
    /// The requesting user.
    pub requester_user_id: Uuid,
    /// Copied from the replaced turn.
    pub web_search_enabled: bool,
    /// Creation time.
    pub now: OffsetDateTime,
}

/// The preflight columns of a retry / edit turn, written together with the
/// quota reserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPreflightColumns {
    /// Effective model (catalog id).
    pub effective_model: String,
    /// Policy snapshot version the turn was planned under.
    pub policy_version_applied: u64,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: u64,
    /// Worst-case credits booked in `quota_usage`.
    pub reserved_credits_micro: i64,
    /// `max_output_tokens` sent to the provider.
    pub max_output_tokens_applied: u32,
    /// `min(estimation_budgets.minimal_generation_floor,
    /// max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: u32,
    /// Write time.
    pub now: OffsetDateTime,
}

/// The `chat_turns` columns the orphan watchdog needs to settle a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanCandidate {
    /// Turn id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Correlation key.
    pub request_id: Uuid,
    /// The user the turn was started by.
    pub requester_user_id: Option<Uuid>,
    /// Creation time (the quota periods derive from it).
    pub started_at: OffsetDateTime,
    /// Effective model, absent before the preflight columns were written.
    pub effective_model: Option<String>,
    /// Policy version the turn was planned under.
    pub policy_version_applied: Option<u64>,
    /// `reserve_tokens`.
    pub reserve_tokens: Option<u64>,
    /// `max_output_tokens_applied`.
    pub max_output_tokens_applied: Option<u32>,
    /// `reserved_credits_micro`.
    pub reserved_credits_micro: Option<i64>,
    /// `minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: Option<u32>,
}

/// Terminal outcome written by the finalization CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFinalization {
    /// Turn to finalize.
    pub turn_id: Uuid,
    /// Terminal state.
    pub state: TurnState,
    /// Error code (`failed` only).
    pub error_code: Option<String>,
    /// Persisted assistant message, when any.
    pub assistant_message_id: Option<Uuid>,
    /// Provider response id (internal only).
    pub provider_response_id: Option<String>,
    /// Finalization time.
    pub now: OffsetDateTime,
}

/// Turn status as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnView {
    /// Correlation key.
    pub request_id: Uuid,
    /// Reported state.
    pub state: TurnStatus,
    /// Terminal error code (state `error` only).
    pub error_code: Option<String>,
    /// Persisted assistant message (state `done`, or `cancelled` with content).
    pub assistant_message_id: Option<Uuid>,
    /// Last state change.
    pub updated_at: OffsetDateTime,
}

/// Turn status (read-only).
pub struct TurnsService {
    db: Arc<DBProvider<DomainError>>,
    enforcer: PolicyEnforcer,
}

impl TurnsService {
    /// Creates the service.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, enforcer: PolicyEnforcer) -> Self {
        Self { db, enforcer }
    }

    /// The status of the turn `request_id` of the caller's chat.
    ///
    /// # Errors
    /// `NotFound` for an unknown, foreign or soft-deleted chat and for an
    /// unknown or soft-deleted turn, PDP errors, `Internal`.
    pub async fn get_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnView, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::READ_TURN, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        conn.find_live_chat(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        // The chat is the caller's: the tenant scope is enough for its turns.
        let turn = conn
            .find_live_turn(&scope.tenant_only(), chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: ResourceKind::Turn,
                id: request_id.to_string(),
            })?;
        let state = TurnState::parse(&turn.state)?;
        let status = TurnStatus::from(state);
        Ok(TurnView {
            request_id: turn.request_id,
            state: status,
            error_code: (status == TurnStatus::Error)
                .then_some(turn.error_code)
                .flatten(),
            assistant_message_id: turn.assistant_message_id,
            updated_at: turn.updated_at.unwrap_or(turn.started_at),
        })
    }
}
