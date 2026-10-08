use async_trait::async_trait;
use toolkit_db::secure::DBRunner;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entity::chat_turn::{Model as TurnModel, TurnState};

/// Parameters for creating a new turn.
#[domain_model]
pub struct CreateTurnParams {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_type: String,
    pub requester_user_id: Option<Uuid>,
    /// Preflight fields — NULL in P2, populated by P3 quota service.
    pub reserve_tokens: Option<i64>,
    pub max_output_tokens_applied: Option<i32>,
    pub reserved_credits_micro: Option<i64>,
    pub policy_version_applied: Option<i64>,
    pub effective_model: Option<String>,
    pub minimal_generation_floor_applied: Option<i32>,
    pub web_search_enabled: bool,
}

/// Parameters for CAS update to completed state.
#[domain_model]
// Fields are read in infra::db::repo::turn_repo — #[domain_model] hides access from clippy.
#[allow(clippy::struct_field_names, dead_code)]
pub struct CasCompleteParams {
    pub turn_id: Uuid,
    pub assistant_message_id: Uuid,
    pub provider_response_id: Option<String>,
}

/// Parameters for CAS update to a terminal state (completed/failed/cancelled).
///
/// Unified CAS method: handles all terminal transitions. For completed turns,
/// `assistant_message_id` and `provider_response_id` are set; for failed/cancelled
/// they are `None` (content durability invariant — DESIGN.md §5.7).
#[domain_model]
pub struct CasTerminalParams {
    pub turn_id: Uuid,
    pub state: TurnState,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    /// Set only for completed turns — links to the persisted assistant message.
    pub assistant_message_id: Option<Uuid>,
    /// Provider response ID (e.g. `OpenAI` `response_id`); set for completed turns.
    pub provider_response_id: Option<String>,
}

/// Preflight fields to backfill on a mutation turn after quota evaluation.
///
/// The mutation path creates turns with NULL quota fields; this struct carries
/// the computed preflight values so they can be persisted before the provider
/// task spawns (ensuring the orphan watchdog has complete data).
#[domain_model]
pub struct UpdatePreflightParams {
    pub turn_id: Uuid,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

/// Identifies which completed tool call counter to increment.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub enum ToolCallType {
    WebSearch,
    CodeInterpreter,
    /// Knowledge search (RAG) via the `search_knowledge` function tool.
    FileSearch,
}

/// Repository trait for turn persistence operations.
#[async_trait]
#[allow(dead_code)]
pub trait TurnRepository: Send + Sync {
    /// INSERT a new turn with `state = running`.
    async fn create_turn<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        params: CreateTurnParams,
    ) -> Result<TurnModel, DomainError>;

    /// SELECT by `(chat_id, request_id)` for idempotency check.
    async fn find_by_chat_and_request_id<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnModel>, DomainError>;

    /// SELECT the running turn for a chat (state=running, `deleted_at` IS NULL).
    async fn find_running_by_chat_id<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnModel>, DomainError>;

    /// CAS state transition to a terminal state.
    /// Returns `rows_affected` (0 = another finalizer won, 1 = success).
    async fn cas_update_state<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        params: CasTerminalParams,
    ) -> Result<u64, DomainError>;

    /// CAS transition to completed, setting `assistant_message_id` and
    /// `provider_response_id`.
    async fn cas_update_completed<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        params: CasCompleteParams,
    ) -> Result<u64, DomainError>;

    /// Set `assistant_message_id` on a turn after the message has been persisted.
    ///
    /// Called within the finalization transaction, AFTER the assistant message
    /// INSERT and CAS guard. Separate from the CAS step because
    /// `assistant_message_id` has a FK to `messages(id)`.
    async fn set_assistant_message_id<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        turn_id: Uuid,
        assistant_message_id: Uuid,
    ) -> Result<(), DomainError>;

    /// Soft-delete a turn, linking to a replacement `request_id`.
    async fn soft_delete<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        turn_id: Uuid,
        replaced_by_request_id: Option<Uuid>,
    ) -> Result<(), DomainError>;

    /// SELECT the most recent non-deleted turn for a chat.
    async fn find_latest_turn<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnModel>, DomainError>;

    /// Update `last_progress_at = now()` for a running turn.
    ///
    /// Called from the streaming task which passes the request-scoped `AccessScope`
    /// for defense-in-depth tenant scoping.
    ///
    /// Returns `rows_affected` (0 if turn is no longer running — benign, no error).
    async fn update_progress_at<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        turn_id: Uuid,
    ) -> Result<u64, DomainError>;

    /// Find running turns with stale progress (orphan candidates).
    ///
    /// No `AccessScope` — system-level background worker query under leader election.
    /// Returns at most `limit` rows ordered by oldest progress first.
    async fn find_orphan_candidates<C: DBRunner>(
        &self,
        runner: &C,
        timeout_secs: u64,
        limit: u32,
    ) -> Result<Vec<TurnModel>, DomainError>;

    /// CAS update for orphan finalization with full predicate re-check.
    ///
    /// The terminal UPDATE re-checks ALL orphan predicates:
    /// `state = 'running' AND deleted_at IS NULL AND last_progress_at <= now() - timeout`.
    /// This prevents "false orphan finalization after renewed progress" (DESIGN.md P1 invariant).
    ///
    /// Returns `rows_affected`:
    /// - `0`: turn is no longer orphan-eligible (already finalized, soft-deleted, or progress renewed)
    /// - `1`: turn transitioned to `Failed` with `error_code = 'orphan_timeout'`
    async fn cas_finalize_orphan<C: DBRunner>(
        &self,
        runner: &C,
        turn_id: Uuid,
        timeout_secs: u64,
    ) -> Result<u64, DomainError>;

    /// SELECT the most recent non-deleted turn for a chat with `FOR UPDATE` row lock.
    /// Used within mutation transactions to serialize concurrent retry/edit/delete.
    async fn find_latest_for_update<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnModel>, DomainError>;

    /// Backfill preflight quota fields on a running turn.
    ///
    /// The mutation path (`mutate_for_stream`) creates the turn with NULL quota
    /// fields because preflight runs after the mutation transaction. This method
    /// persists the computed preflight values so the orphan watchdog can settle
    /// quota correctly if the pod crashes after preflight.
    ///
    /// Only updates the row if `state = 'running'` (CAS guard prevents writing
    /// to already-finalized turns).
    ///
    /// Returns `rows_affected` (0 if turn is no longer running).
    async fn update_preflight_fields<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        params: UpdatePreflightParams,
    ) -> Result<u64, DomainError>;

    /// Atomically increment the completed tool call counter for a turn.
    ///
    /// Called from the streaming task on `ToolPhase::Done`. Best-effort:
    /// callers are expected to log and swallow errors.
    async fn increment_tool_calls<C: DBRunner>(
        &self,
        runner: &C,
        scope: &AccessScope,
        turn_id: Uuid,
        tool: ToolCallType,
    ) -> Result<(), DomainError>;
}
