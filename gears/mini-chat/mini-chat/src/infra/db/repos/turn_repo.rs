//! `chat_turns` queries. Turns are children of a chat: callers authorize the
//! chat first and pass its tenant and id.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Condition, Expr};
use sea_orm::{ColumnTrait, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use super::tenant_scope;
use crate::domain::enums::{RequesterType, TurnState};
use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_turn;

/// The turn of `request_id` in the chat, soft-deleted or not (callers decide
/// how a deleted turn is reported).
///
/// # Errors
/// Database failure.
pub async fn find_by_request(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .filter(chat_turn::Column::RequestId.eq(request_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// The live `running` turn of the chat, if any (at most one by index).
///
/// # Errors
/// Database failure.
pub async fn find_running(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .filter(chat_turn::Column::State.eq(TurnState::Running.as_str()))
        .filter(chat_turn::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// The most recently started live turn of the chat.
///
/// # Errors
/// Database failure.
pub async fn latest_live(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .filter(chat_turn::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .order_by(chat_turn::Column::StartedAt, Order::Desc)
        .order_by(chat_turn::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Column values written by a terminal CAS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalUpdate {
    pub state: TurnState,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub provider_response_id: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    /// `completed_at` and `updated_at`.
    pub now: OffsetDateTime,
}

/// The finalization CAS (DESIGN section 5.7): `UPDATE chat_turns SET state,
/// completed_at, updated_at, error_code, error_detail, provider_response_id,
/// assistant_message_id WHERE id = ? AND state = 'running'`. Returns the
/// affected row count (1 = this finalizer won).
///
/// # Errors
/// Database failure.
pub async fn cas_finalize(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    u: &TerminalUpdate,
) -> Result<u64, DomainError> {
    Ok(chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::State, Expr::value(u.state.as_str()))
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(u.now)))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(u.now))
        .col_expr(
            chat_turn::Column::ErrorCode,
            Expr::value(u.error_code.clone()),
        )
        .col_expr(
            chat_turn::Column::ErrorDetail,
            Expr::value(u.error_detail.clone()),
        )
        .col_expr(
            chat_turn::Column::ProviderResponseId,
            Expr::value(u.provider_response_id.clone()),
        )
        .col_expr(
            chat_turn::Column::AssistantMessageId,
            Expr::value(u.assistant_message_id),
        )
        .filter(chat_turn::Column::Id.eq(turn_id))
        .filter(chat_turn::Column::State.eq(TurnState::Running.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// The orphan watchdog CAS (DESIGN section 4, "Orphan Turn Watchdog"):
/// `state = 'failed'`, `error_code = 'orphan_timeout'` where the turn is
/// still running, not deleted, and its progress (`last_progress_at`, else
/// `started_at`) is at or before `cutoff`. Returns the affected row count.
///
/// # Errors
/// Database failure.
pub async fn cas_finalize_orphan(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::State,
            Expr::value(TurnState::Failed.as_str()),
        )
        .col_expr(
            chat_turn::Column::ErrorCode,
            Expr::value(Some(ORPHAN_TIMEOUT.to_owned())),
        )
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(chat_turn::Column::Id.eq(turn_id))
        .filter(chat_turn::Column::State.eq(TurnState::Running.as_str()))
        .filter(chat_turn::Column::DeletedAt.is_null())
        .filter(stale_progress(cutoff))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// `chat_turns.error_code` of an orphan-finalized turn.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// `last_progress_at <= cutoff OR (last_progress_at IS NULL AND started_at
/// <= cutoff)`: the stale-progress predicate shared by the orphan scan and
/// the orphan CAS.
fn stale_progress(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turn::Column::LastProgressAt.is_null())
                .add(chat_turn::Column::StartedAt.lte(cutoff)),
        )
}

/// Orphan candidates across all tenants (system job, unscoped): live
/// `running` turns whose progress is at or before `cutoff`, oldest start
/// first, at most `limit`. Discovery only: the orphan CAS re-checks every
/// predicate.
///
/// # Errors
/// Database failure.
pub async fn orphan_candidates(
    runner: &impl DBRunner,
    cutoff: OffsetDateTime,
    limit: u64,
) -> Result<Vec<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::State.eq(TurnState::Running.as_str()))
        .filter(chat_turn::Column::DeletedAt.is_null())
        .filter(stale_progress(cutoff))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(chat_turn::Column::StartedAt, Order::Asc)
        .order_by(chat_turn::Column::Id, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// The turn `turn_id` (soft-deleted or not).
///
/// # Errors
/// Database failure.
pub async fn find_by_id(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::Id.eq(turn_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// The preflight columns of a turn (DESIGN section 3.7, `chat_turns`):
/// written once and never changed afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnPreflight {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

/// A new `running` user turn (DESIGN section 3.7, `chat_turns`). The send
/// path sets the preflight columns on INSERT; retry/edit insert them NULL
/// and fill them in the reserve transaction ([`fill_preflight`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRunningTurn {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Uuid,
    pub preflight: Option<TurnPreflight>,
    pub web_search_enabled: bool,
    /// `started_at`, `last_progress_at` and `updated_at`.
    pub now: OffsetDateTime,
}

/// Inserts a running turn.
///
/// # Errors
/// Database failure, `UniqueViolation` for `(chat_id, request_id)` or the
/// one-running-turn-per-chat index.
pub async fn insert_running(
    runner: &impl DBRunner,
    t: NewRunningTurn,
) -> Result<chat_turn::Model, DomainError> {
    let p = t.preflight;
    let am = chat_turn::ActiveModel {
        id: Set(t.id),
        tenant_id: Set(t.tenant_id),
        chat_id: Set(t.chat_id),
        request_id: Set(t.request_id),
        requester_type: Set(RequesterType::User.as_str().to_owned()),
        requester_user_id: Set(Some(t.requester_user_id)),
        state: Set(TurnState::Running.as_str().to_owned()),
        // Reserved column, never populated (DESIGN section 3.7).
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(p.as_ref().map(|p| p.reserve_tokens)),
        max_output_tokens_applied: Set(p.as_ref().map(|p| p.max_output_tokens_applied)),
        reserved_credits_micro: Set(p.as_ref().map(|p| p.reserved_credits_micro)),
        policy_version_applied: Set(p.as_ref().map(|p| p.policy_version_applied)),
        effective_model: Set(p.as_ref().map(|p| p.effective_model.clone())),
        minimal_generation_floor_applied: Set(p.map(|p| p.minimal_generation_floor_applied)),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(t.now),
        last_progress_at: Set(Some(t.now)),
        web_search_enabled: Set(t.web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(t.now),
    };
    Ok(secure_insert::<chat_turn::Entity>(am, &tenant_scope(t.tenant_id), runner).await?)
}

/// Completed tool call counters of a running turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompletedToolCounts {
    pub web_search: i32,
    pub code_interpreter: i32,
    pub file_search: i32,
}

/// Liveness refresh of a running turn: `last_progress_at = now` plus the
/// current completed tool counters (read by the orphan watchdog). Returns the
/// affected row count (0 once the turn is no longer running).
///
/// # Errors
/// Database failure.
pub async fn record_progress(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    now: OffsetDateTime,
    counts: CompletedToolCounts,
) -> Result<u64, DomainError> {
    Ok(chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(now)))
        .col_expr(
            chat_turn::Column::WebSearchCompletedCount,
            Expr::value(counts.web_search),
        )
        .col_expr(
            chat_turn::Column::CodeInterpreterCompletedCount,
            Expr::value(counts.code_interpreter),
        )
        .col_expr(
            chat_turn::Column::FileSearchCompletedCount,
            Expr::value(counts.file_search),
        )
        .filter(chat_turn::Column::Id.eq(turn_id))
        .filter(chat_turn::Column::State.eq(TurnState::Running.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Soft-deletes a live terminal turn (turn mutation, DESIGN section 3.9):
/// `deleted_at = updated_at = now`, `replaced_by_request_id` (retry/edit)
/// `WHERE id = ? AND deleted_at IS NULL AND state <> 'running'`. Returns the
/// affected row count (0 = the turn was deleted or is running).
///
/// # Errors
/// Database failure.
pub async fn soft_delete_terminal(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    replaced_by_request_id: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(
            chat_turn::Column::ReplacedByRequestId,
            Expr::value(replaced_by_request_id),
        )
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(chat_turn::Column::Id.eq(turn_id))
        .filter(chat_turn::Column::DeletedAt.is_null())
        .filter(chat_turn::Column::State.ne(TurnState::Running.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Writes the preflight columns of a retry/edit turn (DESIGN section 3.7,
/// "Preflight columns on retry/edit") `WHERE id = ? AND state = 'running'
/// AND reserve_tokens IS NULL`: written once, never changed. Returns the
/// affected row count.
///
/// # Errors
/// Database failure.
pub async fn fill_preflight(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    p: &TurnPreflight,
) -> Result<u64, DomainError> {
    Ok(chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::ReserveTokens,
            Expr::value(Some(p.reserve_tokens)),
        )
        .col_expr(
            chat_turn::Column::MaxOutputTokensApplied,
            Expr::value(Some(p.max_output_tokens_applied)),
        )
        .col_expr(
            chat_turn::Column::ReservedCreditsMicro,
            Expr::value(Some(p.reserved_credits_micro)),
        )
        .col_expr(
            chat_turn::Column::PolicyVersionApplied,
            Expr::value(Some(p.policy_version_applied)),
        )
        .col_expr(
            chat_turn::Column::EffectiveModel,
            Expr::value(Some(p.effective_model.clone())),
        )
        .col_expr(
            chat_turn::Column::MinimalGenerationFloorApplied,
            Expr::value(Some(p.minimal_generation_floor_applied)),
        )
        .filter(chat_turn::Column::Id.eq(turn_id))
        .filter(chat_turn::Column::State.eq(TurnState::Running.as_str()))
        .filter(chat_turn::Column::ReserveTokens.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}
