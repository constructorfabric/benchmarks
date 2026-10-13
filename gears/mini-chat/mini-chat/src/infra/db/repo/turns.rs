//! `chat_turns` repository (DESIGN section 3.7 `chat_turns`, section 3.3
//! "Turn Status API", section 5.7 "Turn Finalization Contract").
//!
//! Turn queries run after the parent chat was loaded through the chat scope:
//! they use the tenant predicates of that scope and the parent `chat_id`.
//!
//! - **Reads** — the live turn of a `request_id` (Turn Status), the turn of
//!   an idempotency key including soft-deleted turns, and the running-turn
//!   probe of the parallel turn guard.
//! - **Insert** — the `running` turn of the send path with its preflight
//!   columns, inside the reserve transaction; a violation of
//!   `uq_chat_turns_running` is `turn_already_running`, a violation of
//!   `uq_chat_turns_chat_request` is `request_id_conflict`.
//! - **Finalization CAS** — `UPDATE ... WHERE id = ? AND state = 'running'
//!   RETURNING *`, the first statement of the finalization transaction; an
//!   empty result is the CAS loser.
//! - **Progress** — tool-call counters and `last_progress_at` of a running
//!   turn, one guarded statement.
//! - **Finalization outbox** — [`TurnOutbox`]: the usage, audit and
//!   thread-summary messages of a finalized turn, enqueued in the caller's
//!   finalization transaction.
//! - **Turn mutations** (DESIGN section 3.9) — the mutation target of a
//!   `request_id` (soft-deleted turns included), the latest live turn
//!   (greatest `(started_at, id)`), the newer-turn re-check inside the
//!   mutation transaction (`FOR UPDATE` on PostgreSQL), the guarded soft
//!   delete of a terminal turn, the `running` replacement turn with NULL
//!   preflight columns, the one-time write of those columns in the reserve
//!   transaction and the plain `running → failed` CAS of an unstarted
//!   retry / edit turn.
//!
//! Every timestamp is bound through the database's [`TimestampForm`].

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, UsageEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{DbRepository, child_scope};
use crate::domain::error::{AbortReason, DomainError};
use crate::domain::ports::{
    FinalizedTurnRecord, MutationTargetRecord, NewMutationTurn, NewRunningTurn, ThreadSummaryTask,
    ToolCallCounts, TurnFinalization, TurnKey, TurnKeyRecord, TurnOutbox, TurnPreflightColumns,
    TurnRecord, TurnRepository, TurnSoftDelete, TurnState,
};
use crate::infra::db::entities::{TimestampForm, chat_turns};
use crate::infra::db::error::{UniqueTarget, unique_violation_target};
use crate::infra::outbox::{SystemTaskType, ThreadSummaryPayload};

/// `requester_type` of a user turn.
const REQUESTER_USER: &str = "user";

fn parse_state(model: &chat_turns::Model) -> Result<TurnState, DomainError> {
    TurnState::parse(&model.state).ok_or_else(|| {
        DomainError::internal(format!(
            "turn {} has an unknown stored state `{}`",
            model.id, model.state
        ))
    })
}

/// The domain record of a stored turn.
///
/// # Errors
///
/// [`DomainError::Internal`] when the stored state is outside the value set.
pub fn to_record(model: chat_turns::Model) -> Result<TurnRecord, DomainError> {
    let state = parse_state(&model)?;
    Ok(TurnRecord {
        id: model.id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        state,
        error_code: model.error_code,
        assistant_message_id: model.assistant_message_id,
        updated_at: model.updated_at.at(),
    })
}

fn saturating_u32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn saturating_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn counts_of(model: &chat_turns::Model) -> ToolCallCounts {
    ToolCallCounts {
        web_search: saturating_u32(model.web_search_completed_count),
        code_interpreter: saturating_u32(model.code_interpreter_completed_count),
        file_search: saturating_u32(model.file_search_completed_count),
    }
}

fn to_finalized(model: chat_turns::Model) -> FinalizedTurnRecord {
    let counts = counts_of(&model);
    FinalizedTurnRecord {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        requester_user_id: model.requester_user_id,
        effective_model: model.effective_model,
        policy_version_applied: model.policy_version_applied,
        reserve_tokens: model.reserve_tokens,
        max_output_tokens_applied: model.max_output_tokens_applied,
        reserved_credits_micro: model.reserved_credits_micro,
        minimal_generation_floor_applied: model.minimal_generation_floor_applied,
        started_at: model.started_at.at(),
        counts,
    }
}

/// The non-deleted turn of `chat_id` with `request_id`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn find_active_by_request(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<TurnRecord>, DomainError> {
    chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?
        .map(to_record)
        .transpose()
}

/// The turn of `(chat_id, request_id)`, soft-deleted or not.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored state outside
/// the value set.
pub async fn find_by_request_any(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<TurnKeyRecord>, DomainError> {
    let Some(model) = chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?
    else {
        return Ok(None);
    };
    let state = parse_state(&model)?;
    Ok(Some(TurnKeyRecord {
        id: model.id,
        state,
        deleted: model.deleted_at.is_some(),
        assistant_message_id: model.assistant_message_id,
        effective_model: model.effective_model,
    }))
}

/// Whether `chat_id` has a non-deleted `running` turn.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn running_exists(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    let found = chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::State.eq(TurnState::Running.as_str()))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .limit(1)
        .one(runner)
        .await?;
    Ok(found.is_some())
}

/// Inserts the `running` turn `turn` (timestamps bound as `ts`).
///
/// # Errors
///
/// `409 aborted` `turn_already_running` on `uq_chat_turns_running`,
/// `request_id_conflict` on `uq_chat_turns_chat_request`;
/// [`DomainError::Internal`] on any other failure.
pub async fn insert_running(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    turn: NewRunningTurn,
) -> Result<(), DomainError> {
    let model = chat_turns::ActiveModel {
        id: Set(turn.id),
        tenant_id: Set(turn.tenant_id),
        chat_id: Set(turn.chat_id),
        request_id: Set(turn.request_id),
        requester_type: Set(REQUESTER_USER.to_owned()),
        requester_user_id: Set(Some(turn.requester_user_id)),
        state: Set(TurnState::Running.as_str().to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(Some(turn.reserve_tokens)),
        max_output_tokens_applied: Set(Some(saturating_i32(turn.max_output_tokens_applied))),
        reserved_credits_micro: Set(Some(turn.reserved_credits_micro)),
        policy_version_applied: Set(Some(
            i64::try_from(turn.policy_version_applied).unwrap_or(i64::MAX),
        )),
        effective_model: Set(Some(turn.effective_model)),
        minimal_generation_floor_applied: Set(Some(saturating_i32(
            turn.minimal_generation_floor_applied,
        ))),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(ts.stamp(turn.now)),
        last_progress_at: Set(Some(ts.stamp(turn.now))),
        web_search_enabled: Set(turn.web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(ts.stamp(turn.now)),
    };
    insert_turn_row(runner, chat_scope, model).await
}

/// The finalization CAS (timestamps bound as `ts`): moves the running turn
/// `finalization.turn_id` to its terminal state and returns the updated row;
/// `None` when the turn was no longer running.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or when the terminal state is
/// `running`.
pub async fn finalize(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    finalization: TurnFinalization,
) -> Result<Option<FinalizedTurnRecord>, DomainError> {
    if finalization.state == TurnState::Running {
        return Err(DomainError::internal(
            "finalization to the running state is not a terminal transition",
        ));
    }
    let counts = finalization.counts;
    let updated = chat_turns::Entity::update_many()
        .col_expr(
            chat_turns::Column::State,
            Expr::value(finalization.state.as_str()),
        )
        .col_expr(
            chat_turns::Column::ErrorCode,
            Expr::value(finalization.error_code),
        )
        .col_expr(
            chat_turns::Column::AssistantMessageId,
            Expr::value(finalization.assistant_message_id),
        )
        .col_expr(
            chat_turns::Column::ProviderResponseId,
            Expr::value(finalization.provider_response_id),
        )
        .col_expr(
            chat_turns::Column::WebSearchCompletedCount,
            Expr::value(saturating_i32(counts.web_search)),
        )
        .col_expr(
            chat_turns::Column::CodeInterpreterCompletedCount,
            Expr::value(saturating_i32(counts.code_interpreter)),
        )
        .col_expr(
            chat_turns::Column::FileSearchCompletedCount,
            Expr::value(saturating_i32(counts.file_search)),
        )
        .col_expr(
            chat_turns::Column::CompletedAt,
            Expr::value(Some(ts.stamp(finalization.at))),
        )
        .col_expr(
            chat_turns::Column::UpdatedAt,
            Expr::value(ts.stamp(finalization.at)),
        )
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(finalization.turn_id))
                .add(chat_turns::Column::State.eq(TurnState::Running.as_str())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec_with_returning(runner)
        .await?;
    Ok(updated.into_iter().next().map(to_finalized))
}

/// Writes the tool-call counters of the running turn `turn_id` and, when
/// `progress_at` is set, `last_progress_at` (bound as `ts`); `false` when the
/// turn is no longer running.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn record_progress(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    turn_id: Uuid,
    counts: ToolCallCounts,
    progress_at: Option<OffsetDateTime>,
) -> Result<bool, DomainError> {
    let mut update = chat_turns::Entity::update_many()
        .col_expr(
            chat_turns::Column::WebSearchCompletedCount,
            Expr::value(saturating_i32(counts.web_search)),
        )
        .col_expr(
            chat_turns::Column::CodeInterpreterCompletedCount,
            Expr::value(saturating_i32(counts.code_interpreter)),
        )
        .col_expr(
            chat_turns::Column::FileSearchCompletedCount,
            Expr::value(saturating_i32(counts.file_search)),
        );
    if let Some(at) = progress_at {
        update = update
            .col_expr(
                chat_turns::Column::LastProgressAt,
                Expr::value(Some(ts.stamp(at))),
            )
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts.stamp(at)));
    }
    let result = update
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(TurnState::Running.as_str())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

// ── Turn mutations ───────────────────────────────────────────────────────

const TERMINAL_STATES: [&str; 3] = ["completed", "failed", "cancelled"];

fn to_mutation_target(model: &chat_turns::Model) -> Result<MutationTargetRecord, DomainError> {
    let state = parse_state(model)?;
    Ok(MutationTargetRecord {
        id: model.id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        state,
        deleted: model.deleted_at.is_some(),
        requester_user_id: model.requester_user_id,
        web_search_enabled: model.web_search_enabled,
        started_at: model.started_at.at(),
    })
}

/// The turn of `(chat_id, request_id)`, soft-deleted or not, as the
/// mutation rules see it.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored state outside
/// the value set.
pub async fn find_mutation_target(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<MutationTargetRecord>, DomainError> {
    chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?
        .as_ref()
        .map(to_mutation_target)
        .transpose()
}

/// Order key of the latest non-deleted turn of `chat_id` (greatest
/// `(started_at, id)`).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn latest_active(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<TurnKey>, DomainError> {
    let found = chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(chat_turns::Column::StartedAt, Order::Desc)
        .order_by(chat_turns::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(found.map(|model| TurnKey {
        started_at: model.started_at.at(),
        id: model.id,
    }))
}

/// Whether a non-deleted turn of `chat_id` follows `key` in the latest-turn
/// order (`(started_at, id) > key`, bound as `ts`); the matching rows are
/// locked (`FOR UPDATE` on PostgreSQL, a no-op on `SQLite`).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn newer_active_exists(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    key: TurnKey,
) -> Result<bool, DomainError> {
    let after = Condition::any()
        .add(chat_turns::Column::StartedAt.gt(ts.stamp(key.started_at)))
        .add(
            Condition::all()
                .add(chat_turns::Column::StartedAt.eq(ts.stamp(key.started_at)))
                .add(chat_turns::Column::Id.gt(key.id)),
        );
    let rows = chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(after),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .project_all(runner, |select| {
            select.lock_exclusive().into_model::<chat_turns::Model>()
        })
        .await?;
    Ok(!rows.is_empty())
}

/// Soft-deletes the live, terminal turn `delete.turn_id` (timestamps bound as
/// `ts`) with one guarded `UPDATE`; `false` when the turn is running, already
/// deleted or not visible.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn soft_delete_terminal(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    delete: TurnSoftDelete,
) -> Result<bool, DomainError> {
    let result = chat_turns::Entity::update_many()
        .col_expr(
            chat_turns::Column::DeletedAt,
            Expr::value(Some(ts.stamp(delete.at))),
        )
        .col_expr(
            chat_turns::Column::ReplacedByRequestId,
            Expr::value(delete.replaced_by_request_id),
        )
        .col_expr(
            chat_turns::Column::UpdatedAt,
            Expr::value(ts.stamp(delete.at)),
        )
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(delete.turn_id))
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(chat_turns::Column::State.is_in(TERMINAL_STATES)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Inserts the `running` replacement turn `turn` of a retry / edit with NULL
/// preflight columns (timestamps bound as `ts`).
///
/// # Errors
///
/// `409 aborted` `turn_already_running` on `uq_chat_turns_running`,
/// `request_id_conflict` on `uq_chat_turns_chat_request`;
/// [`DomainError::Internal`] on any other failure.
pub async fn insert_mutation_turn(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    turn: NewMutationTurn,
) -> Result<(), DomainError> {
    let model = chat_turns::ActiveModel {
        id: Set(turn.id),
        tenant_id: Set(turn.tenant_id),
        chat_id: Set(turn.chat_id),
        request_id: Set(turn.request_id),
        requester_type: Set(REQUESTER_USER.to_owned()),
        requester_user_id: Set(Some(turn.requester_user_id)),
        state: Set(TurnState::Running.as_str().to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(None),
        max_output_tokens_applied: Set(None),
        reserved_credits_micro: Set(None),
        policy_version_applied: Set(None),
        effective_model: Set(None),
        minimal_generation_floor_applied: Set(None),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(ts.stamp(turn.now)),
        last_progress_at: Set(Some(ts.stamp(turn.now))),
        web_search_enabled: Set(turn.web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(ts.stamp(turn.now)),
    };
    insert_turn_row(runner, chat_scope, model).await
}

async fn insert_turn_row(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    model: chat_turns::ActiveModel,
) -> Result<(), DomainError> {
    match secure_insert::<chat_turns::Entity>(model, &child_scope(chat_scope), runner).await {
        Ok(_) => Ok(()),
        Err(err) => Err(match unique_violation_target(&err) {
            Some(UniqueTarget::RunningTurn) => {
                DomainError::aborted(AbortReason::TurnAlreadyRunning)
            }
            Some(UniqueTarget::TurnRequestId) => {
                DomainError::aborted(AbortReason::RequestIdConflict)
            }
            _ => DomainError::from(err),
        }),
    }
}

/// Writes the preflight columns of the running turn `turn_id` when they are
/// still NULL (one guarded `UPDATE`); `false` otherwise.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn record_preflight(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    turn_id: Uuid,
    columns: TurnPreflightColumns,
) -> Result<bool, DomainError> {
    let result = chat_turns::Entity::update_many()
        .col_expr(
            chat_turns::Column::ReserveTokens,
            Expr::value(Some(columns.reserve_tokens)),
        )
        .col_expr(
            chat_turns::Column::MaxOutputTokensApplied,
            Expr::value(Some(saturating_i32(columns.max_output_tokens_applied))),
        )
        .col_expr(
            chat_turns::Column::ReservedCreditsMicro,
            Expr::value(Some(columns.reserved_credits_micro)),
        )
        .col_expr(
            chat_turns::Column::PolicyVersionApplied,
            Expr::value(Some(
                i64::try_from(columns.policy_version_applied).unwrap_or(i64::MAX),
            )),
        )
        .col_expr(
            chat_turns::Column::EffectiveModel,
            Expr::value(Some(columns.effective_model)),
        )
        .col_expr(
            chat_turns::Column::MinimalGenerationFloorApplied,
            Expr::value(Some(saturating_i32(
                columns.minimal_generation_floor_applied,
            ))),
        )
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(TurnState::Running.as_str()))
                .add(chat_turns::Column::ReserveTokens.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// The plain `running → failed` CAS of an unstarted retry / edit turn
/// (timestamps bound as `ts`); `false` when the turn was no longer running.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn fail_unstarted(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    turn_id: Uuid,
    error_code: &str,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let result = chat_turns::Entity::update_many()
        .col_expr(
            chat_turns::Column::State,
            Expr::value(TurnState::Failed.as_str()),
        )
        .col_expr(
            chat_turns::Column::ErrorCode,
            Expr::value(Some(error_code.to_owned())),
        )
        .col_expr(
            chat_turns::Column::CompletedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts.stamp(at)))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(TurnState::Running.as_str())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

#[async_trait]
impl TurnRepository for DbRepository {
    async fn find_active_by_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_active_by_request(&conn, scope, chat_id, request_id).await
    }

    async fn find_by_request_any(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnKeyRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_by_request_any(&conn, scope, chat_id, request_id).await
    }

    async fn running_exists(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        running_exists(&conn, scope, chat_id).await
    }

    async fn insert_running_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn: NewRunningTurn,
    ) -> Result<(), DomainError> {
        insert_running(tx, self.timestamps(), scope, turn).await
    }

    async fn finalize_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        finalization: TurnFinalization,
    ) -> Result<Option<FinalizedTurnRecord>, DomainError> {
        finalize(tx, self.timestamps(), scope, finalization).await
    }

    async fn record_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        counts: ToolCallCounts,
        progress_at: Option<OffsetDateTime>,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        record_progress(
            &conn,
            self.timestamps(),
            scope,
            turn_id,
            counts,
            progress_at,
        )
        .await
    }

    async fn find_mutation_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTargetRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_mutation_target(&conn, scope, chat_id, request_id).await
    }

    async fn find_mutation_target_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTargetRecord>, DomainError> {
        find_mutation_target(tx, scope, chat_id, request_id).await
    }

    async fn latest_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnKey>, DomainError> {
        let conn = self.db().conn()?;
        latest_active(&conn, scope, chat_id).await
    }

    async fn newer_active_exists_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        key: TurnKey,
    ) -> Result<bool, DomainError> {
        newer_active_exists(tx, self.timestamps(), scope, chat_id, key).await
    }

    async fn soft_delete_terminal_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        delete: TurnSoftDelete,
    ) -> Result<bool, DomainError> {
        soft_delete_terminal(tx, self.timestamps(), scope, delete).await
    }

    async fn insert_mutation_turn_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn: NewMutationTurn,
    ) -> Result<(), DomainError> {
        insert_mutation_turn(tx, self.timestamps(), scope, turn).await
    }

    async fn record_preflight_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn_id: Uuid,
        columns: TurnPreflightColumns,
    ) -> Result<bool, DomainError> {
        record_preflight(tx, scope, turn_id, columns).await
    }

    async fn fail_unstarted(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        fail_unstarted(&conn, self.timestamps(), scope, turn_id, error_code, at).await
    }
}

/// The outbox payload of a thread-summary work item.
#[must_use]
pub fn thread_summary_payload(task: &ThreadSummaryTask) -> ThreadSummaryPayload {
    ThreadSummaryPayload {
        tenant_id: task.tenant_id,
        chat_id: task.chat_id,
        system_request_id: task.system_request_id,
        system_task_type: SystemTaskType::ThreadSummaryUpdate,
        base_frontier_created_at: task.base_frontier.map(|key| key.created_at),
        base_frontier_message_id: task.base_frontier.map(|key| key.id),
        frozen_target_created_at: task.frozen_target.created_at,
        frozen_target_message_id: task.frozen_target.id,
    }
}

#[async_trait]
impl TurnOutbox for DbRepository {
    async fn enqueue_usage_in_tx(
        &self,
        tx: &DbTx<'_>,
        event: &UsageEvent,
    ) -> Result<Wake, DomainError> {
        Ok(self.outbox().enqueue_usage(tx, event).await?)
    }

    async fn enqueue_audit_in_tx(
        &self,
        tx: &DbTx<'_>,
        event: &AuditEvent,
    ) -> Result<Wake, DomainError> {
        Ok(self.outbox().enqueue_audit(tx, event).await?)
    }

    async fn enqueue_thread_summary_in_tx(
        &self,
        tx: &DbTx<'_>,
        task: &ThreadSummaryTask,
    ) -> Result<Wake, DomainError> {
        Ok(self
            .outbox()
            .enqueue_thread_summary(tx, &thread_summary_payload(task))
            .await?)
    }
}

// ── Orphan watchdog (DESIGN section 4 "Orphan Turn Watchdog", B.9.1) ─────

pub use orphan_watchdog::{chat_owner, finalize_orphan, orphan_candidates};

/// The orphan watchdog's scan, its finalization CAS and the chat owner read
/// ([`crate::domain::ports::OrphanTurnRepository`]).
///
/// The scan is a system read across tenants (unconstrained scope, `running`
/// rows only, at most `limit`); the CAS and the owner read are confined to
/// the candidate's tenant. Every timestamp is bound through the database's
/// [`TimestampForm`].
mod orphan_watchdog {
    use async_trait::async_trait;
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
    use time::OffsetDateTime;
    use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, SecureUpdateExt};
    use toolkit_security::AccessScope;
    use uuid::Uuid;

    use super::{DbRepository, child_scope, to_finalized};
    use crate::domain::billing::ORPHAN_TIMEOUT;
    use crate::domain::error::DomainError;
    use crate::domain::ports::{
        FinalizedTurnRecord, OrphanCandidate, OrphanFinalization, OrphanTurnRepository, TurnState,
    };
    use crate::infra::db::entities::{TimestampForm, chat_turns, chats};

    /// `last_progress_at <= cutoff OR (last_progress_at IS NULL AND
    /// started_at <= cutoff)`.
    fn stale_progress(ts: TimestampForm, cutoff: OffsetDateTime) -> Condition {
        Condition::any()
            .add(chat_turns::Column::LastProgressAt.lte(ts.stamp(cutoff)))
            .add(
                Condition::all()
                    .add(chat_turns::Column::LastProgressAt.is_null())
                    .add(chat_turns::Column::StartedAt.lte(ts.stamp(cutoff))),
            )
    }

    /// `state = 'running' AND deleted_at IS NULL` plus the stale-progress
    /// predicate: the orphan rule, shared by the scan and the CAS.
    fn orphan_rule(ts: TimestampForm, cutoff: OffsetDateTime) -> Condition {
        Condition::all()
            .add(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .add(chat_turns::Column::DeletedAt.is_null())
            .add(stale_progress(ts, cutoff))
    }

    /// At most `limit` orphan candidates (all tenants), oldest
    /// `(started_at, id)` first.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure.
    pub async fn orphan_candidates(
        runner: &impl DBRunner,
        ts: TimestampForm,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        let rows = chat_turns::Entity::find()
            .filter(orphan_rule(ts, cutoff))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .order_by(chat_turns::Column::StartedAt, Order::Asc)
            .order_by(chat_turns::Column::Id, Order::Asc)
            .limit(limit)
            .all(runner)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| OrphanCandidate {
                id: row.id,
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
            })
            .collect())
    }

    /// The orphan finalization CAS (timestamps bound as `ts`): `failed` /
    /// `orphan_timeout`, `completed_at = updated_at = at`, re-checking the
    /// whole orphan rule; `None` when the turn is no longer
    /// orphan-finalizable.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure.
    pub async fn finalize_orphan(
        runner: &impl DBRunner,
        ts: TimestampForm,
        finalization: OrphanFinalization,
    ) -> Result<Option<FinalizedTurnRecord>, DomainError> {
        let updated = chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::State,
                Expr::value(TurnState::Failed.as_str()),
            )
            .col_expr(
                chat_turns::Column::ErrorCode,
                Expr::value(Some(ORPHAN_TIMEOUT.to_owned())),
            )
            .col_expr(
                chat_turns::Column::CompletedAt,
                Expr::value(Some(ts.stamp(finalization.at))),
            )
            .col_expr(
                chat_turns::Column::UpdatedAt,
                Expr::value(ts.stamp(finalization.at)),
            )
            .filter(
                orphan_rule(ts, finalization.cutoff)
                    .add(chat_turns::Column::Id.eq(finalization.turn_id)),
            )
            .secure()
            .scope_with(&child_scope(&AccessScope::for_tenant(
                finalization.tenant_id,
            )))
            .exec_with_returning(runner)
            .await?;
        Ok(updated.into_iter().next().map(to_finalized))
    }

    /// Owner of the chat `chat_id` of `tenant_id`, soft-deleted or not.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure.
    pub async fn chat_owner(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<Uuid>, DomainError> {
        let chat = chats::Entity::find()
            .filter(chats::Column::Id.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?;
        Ok(chat.map(|chat| chat.user_id))
    }

    #[async_trait]
    impl OrphanTurnRepository for DbRepository {
        async fn orphan_candidates(
            &self,
            cutoff: OffsetDateTime,
            limit: u64,
        ) -> Result<Vec<OrphanCandidate>, DomainError> {
            let conn = self.db().conn()?;
            orphan_candidates(&conn, self.timestamps(), cutoff, limit).await
        }

        async fn finalize_orphan_in_tx(
            &self,
            tx: &DbTx<'_>,
            finalization: OrphanFinalization,
        ) -> Result<Option<FinalizedTurnRecord>, DomainError> {
            finalize_orphan(tx, self.timestamps(), finalization).await
        }

        async fn chat_owner_in_tx(
            &self,
            tx: &DbTx<'_>,
            tenant_id: Uuid,
            chat_id: Uuid,
        ) -> Result<Option<Uuid>, DomainError> {
            chat_owner(tx, tenant_id, chat_id).await
        }
    }
}
