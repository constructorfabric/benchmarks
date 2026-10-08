//! `chat_turns` queries: idempotency lookups, the running-turn index, the CAS
//! finalizers (stream and orphan watchdog), progress touches and soft delete
//! (DESIGN §3.7 `chat_turns`, §4 "Orphan Turn Watchdog", spec §8.3).
//!
//! Scoping: lookups by chat carry the tenant (`AccessScope::for_tenant`); the
//! methods keyed by a turn id alone (`touch_progress`, the CAS finalizers,
//! `fill_preflight`, `soft_delete`) and the cross-tenant watchdog scan
//! (`stale_running`) are internal paths whose ids come from rows the service
//! already loaded under the caller's authorization, so they run with
//! `AccessScope::allow_all()`.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Expr, Func, OnConflict, SimpleExpr};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DbErr, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder, QuerySelect,
};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{TurnState, error_codes};
use crate::infra::db::entities::chat_turn::{Column, Entity, Model};

/// `requester_type` of user-initiated turns (system tasks never create turns).
const REQUESTER_USER: &str = "user";

/// Per-turn tool-call counters (`*_completed_count` columns).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnCounters {
    pub web_search: i32,
    pub code_interpreter: i32,
    pub file_search: i32,
}

/// Quota-reserve columns of a turn, written once (never changed afterwards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightFields {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

/// A new `running` turn of a user.
#[derive(Debug, Clone)]
pub struct NewTurn {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Uuid,
    pub web_search_enabled: bool,
    /// `Some` on the send path; `None` for retry/edit, which fill the columns
    /// later in the reserve transaction (see [`TurnRepo::fill_preflight`]).
    pub preflight: Option<PreflightFields>,
    /// Application clock: `started_at`, `last_progress_at` and `updated_at`.
    pub now: DateTime<Utc>,
}

/// Terminal transition of a running turn.
#[derive(Debug, Clone)]
pub struct TerminalUpdate {
    /// `completed`, `failed` or `cancelled`.
    pub state: TurnState,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub counters: TurnCounters,
    /// `completed_at` and `updated_at`.
    pub now: DateTime<Utc>,
}

/// `(chat_id, request_id)` key.
fn request_key(chat_id: Uuid, request_id: Uuid) -> Condition {
    Condition::all()
        .add(Column::ChatId.eq(chat_id))
        .add(Column::RequestId.eq(request_id))
}

/// Orphan predicate of the watchdog scan and CAS (DESIGN §4): stale
/// `last_progress_at`, or `started_at` when it is NULL.
fn is_stale(cutoff: DateTime<Utc>) -> Condition {
    Condition::any()
        .add(Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(Column::LastProgressAt.is_null())
                .add(Column::StartedAt.lte(cutoff)),
        )
}

/// `COALESCE(col, value)`: assigns `value` only while the column is NULL.
fn if_null(col: Column, value: impl Into<sea_orm::Value>) -> SimpleExpr {
    Func::coalesce([Expr::col(col), Expr::val(value)]).into()
}

/// Untargeted `ON CONFLICT DO NOTHING` (covers every unique key of the table,
/// including the partial running index). The primary key is only the polyfill
/// column the query builder needs on backends without `DO NOTHING` (`MySQL`,
/// unsupported by this gear).
fn absorb_any_conflict() -> OnConflict {
    OnConflict::new().do_nothing_on([Column::Id]).to_owned()
}

/// Queries over `chat_turns`.
pub struct TurnRepo;

impl TurnRepo {
    /// The turn of `request_id` in the chat, soft-deleted or not (the unique key
    /// `(chat_id, request_id)` spans deleted rows).
    ///
    /// # Errors
    /// Database failures.
    pub async fn find_by_request(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<Option<Model>> {
        Ok(Entity::find()
            .filter(request_key(chat_id, request_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// The non-deleted `running` turn of the chat, if any (at most one by the
    /// partial unique index).
    ///
    /// # Errors
    /// Database failures.
    pub async fn running_in_chat(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<Option<Model>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::State.eq(TurnState::Running.as_str()))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// The latest turn for mutation eligibility: greatest `(started_at, id)`
    /// among the chat's non-deleted turns (DESIGN §3.7).
    ///
    /// # Errors
    /// Database failures.
    pub async fn latest_live(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<Option<Model>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .order_by_desc(Column::StartedAt)
            .order_by_desc(Column::Id)
            .limit(1)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// `SELECT ... FOR UPDATE` of the turn `turn_id` of the chat: the row lock that
    /// orders concurrent mutations of one turn on `PostgreSQL` (DESIGN §3.9 rule 7).
    /// Not used on `SQLite`, where the mutation transaction's first write takes
    /// the database write lock.
    ///
    /// # Errors
    /// Database failures.
    pub async fn lock_for_update(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
    ) -> DomainResult<Option<Model>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::Id.eq(turn_id)),
            )
            .lock_exclusive()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// Insert a `running` turn (`started_at = last_progress_at = updated_at =
    /// new.now`) and return the stored row.
    ///
    /// Unique violations are mapped by *re-reading*: the insert is
    /// `INSERT .. ON CONFLICT DO NOTHING` (no conflict target, so it absorbs
    /// both `uq_chat_turns_chat_request` and the partial
    /// `uq_chat_turns_one_running_per_chat` index), which writes nothing on a
    /// conflict instead of raising. A raised unique violation would abort the
    /// whole transaction on `PostgreSQL`, so the re-read could not run inside
    /// the caller's reserve transaction; with `DO NOTHING` the transaction stays
    /// usable on both backends and the re-read runs on the same runner. If the
    /// `(chat_id, request_id)` key now exists the request id was reused, otherwise
    /// the running index was hit.
    ///
    /// # Errors
    /// `RequestIdConflict` when `(chat_id, request_id)` already has a turn
    /// (whatever its state); `TurnAlreadyRunning` when another non-deleted turn of
    /// the chat is running; scope violations and database failures. After either
    /// conflict the runner is still usable; the caller decides whether to roll back.
    pub async fn insert_running(runner: &impl DBRunner, new: NewTurn) -> DomainResult<Model> {
        let pre = new.preflight.as_ref();
        // The stored row is fully determined by `new`, so it is built once and
        // returned on success without a read-back; `reset_all` marks every column
        // as set for the INSERT.
        let row = Model {
            id: new.id,
            tenant_id: new.tenant_id,
            chat_id: new.chat_id,
            request_id: new.request_id,
            requester_type: REQUESTER_USER.to_owned(),
            requester_user_id: Some(new.requester_user_id),
            state: TurnState::Running.as_str().to_owned(),
            provider_name: None,
            provider_response_id: None,
            assistant_message_id: None,
            error_code: None,
            reserve_tokens: pre.map(|p| p.reserve_tokens),
            max_output_tokens_applied: pre.map(|p| p.max_output_tokens_applied),
            reserved_credits_micro: pre.map(|p| p.reserved_credits_micro),
            policy_version_applied: pre.map(|p| p.policy_version_applied),
            effective_model: pre.map(|p| p.effective_model.clone()),
            minimal_generation_floor_applied: pre.map(|p| p.minimal_generation_floor_applied),
            error_detail: None,
            deleted_at: None,
            replaced_by_request_id: None,
            started_at: new.now,
            last_progress_at: Some(new.now),
            web_search_enabled: new.web_search_enabled,
            web_search_completed_count: 0,
            code_interpreter_completed_count: 0,
            file_search_completed_count: 0,
            completed_at: None,
            updated_at: Some(new.now),
        };
        let am = row.clone().into_active_model().reset_all();
        let scope = AccessScope::for_tenant(new.tenant_id);
        // `exec` (not `exec_with_returning`): the one execution shape that reports
        // a swallowed conflict as `RecordNotInserted` on every backend.
        match Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict_raw(absorb_any_conflict())
            .exec(runner)
            .await
        {
            Ok(_) => return Ok(row),
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => {}
            Err(e) => return Err(e.into()),
        }

        // Conflict: re-read to tell the two unique keys apart.
        match Self::find_by_request(runner, new.tenant_id, new.chat_id, new.request_id).await? {
            Some(_) => Err(DomainError::RequestIdConflict),
            None => Err(DomainError::TurnAlreadyRunning),
        }
    }

    /// Write the preflight columns of a turn inserted without them (retry/edit).
    /// Only NULL columns are written (`COALESCE`), so a value set once never
    /// changes.
    ///
    /// # Errors
    /// `TurnNotFound`; database failures.
    pub async fn fill_preflight(
        runner: &impl DBRunner,
        turn_id: Uuid,
        f: &PreflightFields,
    ) -> DomainResult<()> {
        let res = Entity::update_many()
            .col_expr(
                Column::ReserveTokens,
                if_null(Column::ReserveTokens, f.reserve_tokens),
            )
            .col_expr(
                Column::MaxOutputTokensApplied,
                if_null(Column::MaxOutputTokensApplied, f.max_output_tokens_applied),
            )
            .col_expr(
                Column::ReservedCreditsMicro,
                if_null(Column::ReservedCreditsMicro, f.reserved_credits_micro),
            )
            .col_expr(
                Column::PolicyVersionApplied,
                if_null(Column::PolicyVersionApplied, f.policy_version_applied),
            )
            .col_expr(
                Column::EffectiveModel,
                if_null(Column::EffectiveModel, f.effective_model.clone()),
            )
            .col_expr(
                Column::MinimalGenerationFloorApplied,
                if_null(
                    Column::MinimalGenerationFloorApplied,
                    f.minimal_generation_floor_applied,
                ),
            )
            .filter(Column::Id.eq(turn_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        if res.rows_affected == 0 {
            return Err(DomainError::TurnNotFound);
        }
        Ok(())
    }

    /// Refresh `last_progress_at` / `updated_at` (application clock) and store the
    /// current tool-call counters of a still-`running`, non-deleted turn. A turn
    /// that already finished is left untouched.
    ///
    /// # Errors
    /// Database failures.
    pub async fn touch_progress(
        runner: &impl DBRunner,
        turn_id: Uuid,
        counters: TurnCounters,
    ) -> DomainResult<()> {
        let now = now_utc();
        Entity::update_many()
            .col_expr(Column::LastProgressAt, Expr::value(now))
            .col_expr(Column::UpdatedAt, Expr::value(now))
            .col_expr(
                Column::WebSearchCompletedCount,
                Expr::value(counters.web_search),
            )
            .col_expr(
                Column::CodeInterpreterCompletedCount,
                Expr::value(counters.code_interpreter),
            )
            .col_expr(
                Column::FileSearchCompletedCount,
                Expr::value(counters.file_search),
            )
            .filter(
                Condition::all()
                    .add(Column::Id.eq(turn_id))
                    .add(Column::State.eq(TurnState::Running.as_str()))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Finalization CAS: move the turn from `running` to the terminal state of
    /// `f` (`WHERE id = ? AND state = 'running'`). Returns `false` when the turn
    /// is no longer running (the loser rolls back and emits nothing).
    ///
    /// # Errors
    /// `Internal` when `f.state` is not terminal; database failures.
    pub async fn cas_finalize(
        runner: &impl DBRunner,
        turn_id: Uuid,
        f: &TerminalUpdate,
    ) -> DomainResult<bool> {
        if !f.state.is_terminal() {
            return Err(DomainError::internal(format!(
                "cannot finalize a turn into the non-terminal state `{}`",
                f.state
            )));
        }
        let res = Entity::update_many()
            .col_expr(Column::State, Expr::value(f.state.as_str()))
            .col_expr(Column::ErrorCode, Expr::value(f.error_code.clone()))
            .col_expr(Column::ErrorDetail, Expr::value(f.error_detail.clone()))
            .col_expr(
                Column::AssistantMessageId,
                Expr::value(f.assistant_message_id),
            )
            .col_expr(
                Column::ProviderResponseId,
                Expr::value(f.provider_response_id.clone()),
            )
            .col_expr(
                Column::WebSearchCompletedCount,
                Expr::value(f.counters.web_search),
            )
            .col_expr(
                Column::CodeInterpreterCompletedCount,
                Expr::value(f.counters.code_interpreter),
            )
            .col_expr(
                Column::FileSearchCompletedCount,
                Expr::value(f.counters.file_search),
            )
            .col_expr(Column::CompletedAt, Expr::value(f.now))
            .col_expr(Column::UpdatedAt, Expr::value(f.now))
            .filter(
                Condition::all()
                    .add(Column::Id.eq(turn_id))
                    .add(Column::State.eq(TurnState::Running.as_str())),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Orphan CAS (DESIGN §4 "Orphan Turn Watchdog"): fail the turn with
    /// `orphan_timeout` only while it is still `running`, not deleted, and its
    /// progress (or `started_at` when NULL) is at or before `cutoff`. `false`
    /// means the turn finished, was deleted or made progress meanwhile.
    ///
    /// # Errors
    /// Database failures.
    pub async fn cas_orphan(
        runner: &impl DBRunner,
        turn_id: Uuid,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = Entity::update_many()
            .col_expr(Column::State, Expr::value(TurnState::Failed.as_str()))
            .col_expr(Column::ErrorCode, Expr::value(error_codes::ORPHAN_TIMEOUT))
            .col_expr(Column::CompletedAt, Expr::value(now))
            .col_expr(Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(Column::Id.eq(turn_id))
                    .add(Column::State.eq(TurnState::Running.as_str()))
                    .add(Column::DeletedAt.is_null())
                    .add(is_stale(cutoff)),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Up to `limit` orphan candidates across tenants (oldest `started_at`
    /// first, then `id`), after the `(started_at, id)` key `after` when given
    /// (the last candidate of the previous page). Advisory only: finalization
    /// must still win [`Self::cas_orphan`].
    ///
    /// # Errors
    /// Database failures.
    pub async fn stale_running(
        runner: &impl DBRunner,
        cutoff: DateTime<Utc>,
        after: Option<(DateTime<Utc>, Uuid)>,
        limit: u64,
    ) -> DomainResult<Vec<Model>> {
        let mut filter = Condition::all()
            .add(Column::State.eq(TurnState::Running.as_str()))
            .add(Column::DeletedAt.is_null())
            .add(is_stale(cutoff));
        if let Some((started_at, id)) = after {
            filter = filter.add(
                Condition::any().add(Column::StartedAt.gt(started_at)).add(
                    Condition::all()
                        .add(Column::StartedAt.eq(started_at))
                        .add(Column::Id.gt(id)),
                ),
            );
        }
        Ok(Entity::find()
            .filter(filter)
            .order_by_asc(Column::StartedAt)
            .order_by_asc(Column::Id)
            .limit(limit)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(runner)
            .await?)
    }

    /// Soft-delete a turn (`deleted_at = updated_at = now`, optionally recording
    /// the `request_id` of the turn that replaced it). `false` when the turn is
    /// missing or already deleted.
    ///
    /// # Errors
    /// Database failures.
    pub async fn soft_delete(
        runner: &impl DBRunner,
        turn_id: Uuid,
        replaced_by: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let res = Entity::update_many()
            .col_expr(Column::DeletedAt, Expr::value(now))
            .col_expr(Column::UpdatedAt, Expr::value(now))
            .col_expr(Column::ReplacedByRequestId, Expr::value(replaced_by))
            .filter(
                Condition::all()
                    .add(Column::Id.eq(turn_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod turn_tests;
