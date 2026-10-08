//! Statements on `chat_turns`: idempotency lookups, the running-turn guard, the insert of a new
//! turn, the finalization CAS and the turn mutations (latest turn, soft delete, preflight fill). Callers pass a tenant scope (`scope.tenant_only()` of an
//! authorized chat, or the turn's tenant for the background finalization).

use sea_orm::sea_query::{Expr, ExprTrait as _, Func};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::chat_turns::{self, Column};
use crate::infra::db::{TurnState, ts};

/// The turn of `(chat_id, request_id)`, soft-deleted or not.
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_by_request(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    chat_turns::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::RequestId.eq(request_id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Whether `chat_id` has a non-deleted `running` turn.
///
/// # Errors
/// `Internal` on a database error.
pub async fn has_running(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    let running = chat_turns::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::State.eq(TurnState::Running.as_str()))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(running.is_some())
}

/// The chat's latest non-deleted turn: the greatest `(started_at, id)` (DESIGN 3.9).
///
/// # Errors
/// `Internal` on a database error.
pub async fn latest(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    chat_turns::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .order_by_desc(Column::StartedAt)
        .order_by_desc(Column::Id)
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Soft-deletes turn `id` when it is terminal and not deleted yet: sets `deleted_at` and
/// `replaced_by_request_id` (the new turn of a retry/edit, `None` for a delete). `false` when no
/// row matched (running or already deleted).
///
/// # Errors
/// `Internal` on a database error.
pub async fn soft_delete_terminal(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    replaced_by: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let now = ts::normalize(now);
    let res = chat_turns::Entity::update_many()
        .col_expr(Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::Id.eq(id))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::State.ne(TurnState::Running.as_str()))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected == 1)
}

/// The preflight columns of a retry/edit turn, written by its reserve transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightColumns {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

/// Fills the preflight columns of the `running` turn `id` inserted without them; `false` when the
/// turn is no longer running or already has them.
///
/// # Errors
/// `Internal` on a database error.
pub async fn fill_preflight(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    p: &PreflightColumns,
) -> Result<bool, DomainError> {
    let res = chat_turns::Entity::update_many()
        .col_expr(Column::ReserveTokens, Expr::value(Some(p.reserve_tokens)))
        .col_expr(
            Column::MaxOutputTokensApplied,
            Expr::value(Some(p.max_output_tokens_applied)),
        )
        .col_expr(
            Column::ReservedCreditsMicro,
            Expr::value(Some(p.reserved_credits_micro)),
        )
        .col_expr(
            Column::PolicyVersionApplied,
            Expr::value(Some(p.policy_version_applied)),
        )
        .col_expr(
            Column::EffectiveModel,
            Expr::value(Some(p.effective_model.clone())),
        )
        .col_expr(
            Column::MinimalGenerationFloorApplied,
            Expr::value(Some(p.minimal_generation_floor_applied)),
        )
        .filter(Column::Id.eq(id))
        .filter(Column::State.eq(TurnState::Running.as_str()))
        .filter(Column::ReserveTokens.is_null())
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected == 1)
}

/// Inserts a turn. A unique violation (same `request_id`, or a second running turn of the chat)
/// is `Conflict{unique_violation}`; on `PostgreSQL` it aborts the transaction.
///
/// # Errors
/// `Conflict`, `AccessDenied` outside `scope`, `Internal` on a database error.
pub async fn insert(
    tx: &impl DBRunner,
    scope: &AccessScope,
    turn: chat_turns::ActiveModel,
) -> Result<(), DomainError> {
    secure_insert::<chat_turns::Entity>(turn, scope, tx)
        .await
        .map(drop)
        .map_err(map_scope_err)
}

/// Terminal values written by [`finalize_running`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTerminal {
    /// `completed`, `failed` or `cancelled`.
    pub state: TurnState,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub provider_response_id: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub now: OffsetDateTime,
}

/// The finalization CAS: moves turn `id` from `running` to `t.state`. `false` when the turn is
/// no longer running (another finalizer won).
///
/// # Errors
/// `Internal` on a database error.
pub async fn finalize_running(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    t: &TurnTerminal,
) -> Result<bool, DomainError> {
    let now = ts::normalize(t.now);
    let res = chat_turns::Entity::update_many()
        .col_expr(Column::State, Expr::value(t.state.as_str()))
        .col_expr(Column::ErrorCode, Expr::value(t.error_code.clone()))
        .col_expr(Column::ErrorDetail, Expr::value(t.error_detail.clone()))
        .col_expr(Column::CompletedAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .col_expr(
            Column::ProviderResponseId,
            Expr::value(t.provider_response_id.clone()),
        )
        .col_expr(
            Column::AssistantMessageId,
            Expr::value(t.assistant_message_id),
        )
        .filter(Column::Id.eq(id))
        .filter(Column::State.eq(TurnState::Running.as_str()))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected == 1)
}

/// The orphan predicate (DESIGN 4022-4107): a non-deleted `running` turn whose
/// `last_progress_at` (`started_at` when NULL) is at or before `cutoff`. Shared by the candidate
/// scan and the finalization CAS. `cutoff` must be [`ts::normalize`]d (text comparison).
fn orphan_condition(cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(Column::State.eq(TurnState::Running.as_str()))
        .add(Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(Column::LastProgressAt.lte(cutoff))
                .add(
                    Condition::all()
                        .add(Column::LastProgressAt.is_null())
                        .add(Column::StartedAt.lte(cutoff)),
                ),
        )
}

/// At most `limit` orphan candidates, stalest first (`COALESCE(last_progress_at, started_at)`). Advisory: only [`finalize_orphan`] decides.
///
/// # Errors
/// `Internal` on a database error.
pub async fn orphan_candidates(
    conn: &impl DBRunner,
    scope: &AccessScope,
    cutoff: OffsetDateTime,
    limit: u64,
) -> Result<Vec<chat_turns::Model>, DomainError> {
    chat_turns::Entity::find()
        .filter(orphan_condition(ts::normalize(cutoff)))
        .order_by_asc(Func::coalesce([
            Expr::col(Column::LastProgressAt),
            Expr::col(Column::StartedAt),
        ]))
        .order_by_asc(Column::Id)
        .limit(limit)
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}

/// The orphan finalization CAS: `running` -> `failed` / `orphan_timeout` while the stale-progress
/// predicate still holds (a refreshed `last_progress_at`, a soft delete or another finalizer makes
/// it `false`).
///
/// # Errors
/// `Internal` on a database error.
pub async fn finalize_orphan(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let now = ts::normalize(now);
    let res = chat_turns::Entity::update_many()
        .col_expr(Column::State, Expr::value(TurnState::Failed.as_str()))
        .col_expr(Column::ErrorCode, Expr::value(ORPHAN_TIMEOUT))
        .col_expr(Column::CompletedAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::Id.eq(id))
        .filter(orphan_condition(ts::normalize(cutoff)))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected == 1)
}

/// `chat_turns.error_code` of a turn finalized by the orphan watchdog.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// Refreshes `last_progress_at` of turn `id` while it is `running` (no CAS on anything else);
/// a finalized turn is left unchanged.
///
/// # Errors
/// `Internal` on a database error.
pub async fn touch_progress(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chat_turns::Entity::update_many()
        .col_expr(Column::LastProgressAt, Expr::value(ts::normalize(now)))
        .filter(Column::Id.eq(id))
        .filter(Column::State.eq(TurnState::Running.as_str()))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// A per-turn counter of completed built-in tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCounter {
    WebSearch,
    CodeInterpreter,
    FileSearch,
}

impl ToolCounter {
    fn column(self) -> Column {
        match self {
            Self::WebSearch => Column::WebSearchCompletedCount,
            Self::CodeInterpreter => Column::CodeInterpreterCompletedCount,
            Self::FileSearch => Column::FileSearchCompletedCount,
        }
    }
}

/// Adds one completed call to `counter` of turn `id` while it is `running` (so the orphan
/// watchdog can report the calls of a turn the stream never finalized).
///
/// # Errors
/// `Internal` on a database error.
pub async fn add_tool_completion(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    counter: ToolCounter,
) -> Result<(), DomainError> {
    let column = counter.column();
    chat_turns::Entity::update_many()
        .col_expr(column, Expr::col(column).add(1))
        .filter(Column::Id.eq(id))
        .filter(Column::State.eq(TurnState::Running.as_str()))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}
