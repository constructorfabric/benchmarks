//! `chat_turn` repository (chat child: tenant-only scope).

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, Order, QueryFilter, Value};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::chat_turn;

/// Terminal columns written by the finalization CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTerminal {
    /// `completed` | `failed` | `cancelled`.
    pub state: &'static str,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub now: OffsetDateTime,
}

/// Preflight columns of a turn (written once: on insert by a send, by the
/// reserve transaction of a retry / edit turn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPreflight {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

/// Per-turn completed tool counter (`chat_turns.*_completed_count`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCounter {
    WebSearch,
    CodeInterpreter,
    FileSearch,
}

impl ToolCounter {
    const fn column(self) -> chat_turn::Column {
        match self {
            Self::WebSearch => chat_turn::Column::WebSearchCompletedCount,
            Self::CodeInterpreter => chat_turn::Column::CodeInterpreterCompletedCount,
            Self::FileSearch => chat_turn::Column::FileSearchCompletedCount,
        }
    }
}

/// `id = :id AND state = 'running'` (the finalization CAS guard).
fn running(id: Uuid) -> Condition {
    Condition::all()
        .add(chat_turn::Column::Id.eq(id))
        .add(chat_turn::Column::State.eq("running"))
}

/// The orphan predicate (D "Orphan Turn Watchdog"): `state = 'running' AND
/// deleted_at IS NULL AND (last_progress_at <= :cutoff OR (last_progress_at
/// IS NULL AND started_at <= :cutoff))`. `cutoff` must be bound in the
/// column's comparable form (`timestamps::comparable`).
fn orphan(cutoff: &Value) -> Condition {
    Condition::all()
        .add(chat_turn::Column::State.eq("running"))
        .add(chat_turn::Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(chat_turn::Column::LastProgressAt.lte(cutoff.clone()))
                .add(
                    Condition::all()
                        .add(chat_turn::Column::LastProgressAt.is_null())
                        .add(chat_turn::Column::StartedAt.lte(cutoff.clone())),
                ),
        )
}

/// Repository for `chat_turn` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnRepo;

impl TurnRepo {
    /// Insert a complete row.
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error (unique/CHECK
    /// violations included).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: chat_turn::Model,
    ) -> Result<chat_turn::Model, ScopeError> {
        insert_model::<chat_turn::Entity>(runner, &scope.tenant_only(), row).await
    }

    /// Load a row by id within the scope.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<chat_turn::Model>, ScopeError> {
        chat_turn::Entity::find_by_id(id)
            .secure()
            .scope_with(&scope.tenant_only())
            .one(runner)
            .await
    }

    /// The turn of `(chat_id, request_id)`, soft-deleted rows included.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_request(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<chat_turn::Model>, ScopeError> {
        chat_turn::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .one(runner)
            .await
    }

    /// The non-deleted `running` turn of the chat, if any.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_running(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<chat_turn::Model>, ScopeError> {
        chat_turn::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::State.eq("running"))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .one(runner)
            .await
    }

    /// Finalization CAS: write the terminal columns when the turn is still
    /// `running`. Returns the number of rows updated (0 = lost the race).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn finalize_cas(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        t: &TurnTerminal,
    ) -> Result<u64, ScopeError> {
        let res = chat_turn::Entity::update_many()
            .filter(running(id))
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(chat_turn::Column::State, Expr::value(t.state))
            .col_expr(
                chat_turn::Column::ErrorCode,
                Expr::value(t.error_code.clone()),
            )
            .col_expr(
                chat_turn::Column::ErrorDetail,
                Expr::value(t.error_detail.clone()),
            )
            .col_expr(
                chat_turn::Column::AssistantMessageId,
                Expr::value(t.assistant_message_id),
            )
            .col_expr(
                chat_turn::Column::ProviderResponseId,
                Expr::value(t.provider_response_id.clone()),
            )
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(t.now))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(t.now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Refresh `last_progress_at` of a running turn.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn touch_progress(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = chat_turn::Entity::update_many()
            .filter(running(id))
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Increment a completed tool counter of a running turn and refresh
    /// `last_progress_at`.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn increment_tool_count(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        counter: ToolCounter,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let col = counter.column();
        let res = chat_turn::Entity::update_many()
            .filter(running(id))
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(col, Expr::col(col).add(1))
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// The latest non-deleted turn of the chat by `(started_at, id)`
    /// (D§3.9 "last turn").
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_latest_active(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<chat_turn::Model>, ScopeError> {
        chat_turn::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .order_by(chat_turn::Column::StartedAt, Order::Desc)
            .order_by(chat_turn::Column::Id, Order::Desc)
            .limit(1)
            .one(runner)
            .await
    }

    /// Soft-delete a non-deleted terminal turn (`deleted_at`, optional
    /// `replaced_by_request_id`). Returns the number of rows updated (0 when
    /// the turn is already deleted or still running). On `PostgreSQL` the
    /// update takes the row lock, so concurrent mutations of the same turn
    /// serialize and the loser updates 0 rows.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn soft_delete_terminal(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        replaced_by: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = chat_turn::Entity::update_many()
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(id))
                    .add(chat_turn::Column::DeletedAt.is_null())
                    .add(chat_turn::Column::State.ne("running")),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(chat_turn::Column::DeletedAt, Expr::value(now))
            .col_expr(
                chat_turn::Column::ReplacedByRequestId,
                Expr::value(replaced_by),
            )
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Fill the preflight columns of a running retry / edit turn whose
    /// columns are still NULL. Returns the number of rows updated.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn fill_preflight(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        p: &TurnPreflight,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = chat_turn::Entity::update_many()
            .filter(
                running(id)
                    .add(chat_turn::Column::DeletedAt.is_null())
                    .add(chat_turn::Column::ReserveTokens.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(
                chat_turn::Column::ReserveTokens,
                Expr::value(p.reserve_tokens),
            )
            .col_expr(
                chat_turn::Column::MaxOutputTokensApplied,
                Expr::value(p.max_output_tokens_applied),
            )
            .col_expr(
                chat_turn::Column::ReservedCreditsMicro,
                Expr::value(p.reserved_credits_micro),
            )
            .col_expr(
                chat_turn::Column::PolicyVersionApplied,
                Expr::value(p.policy_version_applied),
            )
            .col_expr(
                chat_turn::Column::EffectiveModel,
                Expr::value(p.effective_model.clone()),
            )
            .col_expr(
                chat_turn::Column::MinimalGenerationFloorApplied,
                Expr::value(p.minimal_generation_floor_applied),
            )
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(now))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// At most `limit` orphan candidates within `scope` (the watchdog uses
    /// an unconstrained scope: every tenant), oldest `started_at` first.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_orphan_candidates(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        cutoff: &Value,
        limit: u64,
    ) -> Result<Vec<chat_turn::Model>, ScopeError> {
        chat_turn::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(orphan(cutoff))
            .order_by(chat_turn::Column::StartedAt, Order::Asc)
            .order_by(chat_turn::Column::Id, Order::Asc)
            .limit(limit)
            .all(runner)
            .await
    }

    /// Orphan finalization CAS: `failed` / `orphan_timeout` when the turn
    /// still matches the orphan predicate (re-checked here, not only at
    /// discovery). Returns the number of rows updated (0: finalized,
    /// deleted or refreshed meanwhile).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn orphan_cas(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        cutoff: &Value,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = chat_turn::Entity::update_many()
            .filter(orphan(cutoff).add(chat_turn::Column::Id.eq(id)))
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(chat_turn::Column::State, Expr::value("failed"))
            .col_expr(chat_turn::Column::ErrorCode, Expr::value("orphan_timeout"))
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Whether `user` has a non-deleted `running` turn other than `id`
    /// (any chat of the tenant in `scope`).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn has_other_running(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        user: Uuid,
        id: Uuid,
    ) -> Result<bool, ScopeError> {
        let other = chat_turn::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::RequesterUserId.eq(user))
                    .add(chat_turn::Column::State.eq("running"))
                    .add(chat_turn::Column::DeletedAt.is_null())
                    .add(chat_turn::Column::Id.ne(id)),
            )
            .limit(1)
            .one(runner)
            .await?;
        Ok(other.is_some())
    }
}
