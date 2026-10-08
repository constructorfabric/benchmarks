//! `chat_turns` repository.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_turns::{ActiveModel, Column, Entity, Model};

fn tscope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Turn states.
pub mod state {
    pub const RUNNING: &str = "running";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
}

/// Inserts a turn.
///
/// # Errors
/// Database errors (unique violations on request id / running turn).
pub async fn insert(runner: &impl DBRunner, tenant_id: Uuid, am: ActiveModel) -> Result<(), DomainError> {
    Entity::insert(am)
        .secure()
        .scope_unchecked(&tscope(tenant_id))?
        .exec(runner)
        .await?;
    Ok(())
}

/// Turn by `(chat_id, request_id)`, including soft-deleted rows.
///
/// # Errors
/// Database errors.
pub async fn find_by_request(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::RequestId.eq(request_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .one(runner)
        .await?)
}

/// Turn by id (system access).
///
/// # Errors
/// Database errors.
pub async fn find_by_id(runner: &impl DBRunner, turn_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Id.eq(turn_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(runner)
        .await?)
}

/// Running, non-deleted turn of a chat.
///
/// # Errors
/// Database errors.
pub async fn find_running(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::State.eq(state::RUNNING))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .one(runner)
        .await?)
}

/// Latest non-deleted turn by `(started_at, id)`.
///
/// # Errors
/// Database errors.
pub async fn latest(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::StartedAt, Order::Desc)
        .order_by(Column::Id, Order::Desc)
        .one(runner)
        .await?)
}

/// Fields written by a terminal CAS.
#[derive(Debug, Clone, Default)]
pub struct TerminalUpdate {
    pub state: &'static str,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub web_search_completed: Option<i32>,
    pub code_interpreter_completed: Option<i32>,
    pub file_search_completed: Option<i32>,
}

/// CAS `running -> terminal`; returns rows affected (0 = lost the race).
///
/// # Errors
/// Database errors.
pub async fn cas_finalize(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    upd: &TerminalUpdate,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    let mut q = Entity::update_many()
        .col_expr(Column::State, Expr::value(upd.state))
        .col_expr(Column::CompletedAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .col_expr(Column::ErrorCode, Expr::value(upd.error_code.clone()))
        .col_expr(Column::ErrorDetail, Expr::value(upd.error_detail.clone()))
        .col_expr(Column::AssistantMessageId, Expr::value(upd.assistant_message_id));
    if let Some(rid) = &upd.provider_response_id {
        q = q.col_expr(Column::ProviderResponseId, Expr::value(rid.clone()));
    }
    if let Some(n) = upd.web_search_completed {
        q = q.col_expr(Column::WebSearchCompletedCount, Expr::value(n));
    }
    if let Some(n) = upd.code_interpreter_completed {
        q = q.col_expr(Column::CodeInterpreterCompletedCount, Expr::value(n));
    }
    if let Some(n) = upd.file_search_completed {
        q = q.col_expr(Column::FileSearchCompletedCount, Expr::value(n));
    }
    Ok(q.filter(Column::Id.eq(turn_id))
        .filter(Column::State.eq(state::RUNNING))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Preflight fields persisted once (send insert or retry/edit reserve).
#[derive(Debug, Clone)]
pub struct PreflightFields {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

/// Writes the preflight fields of a retry/edit turn (only while NULL).
///
/// # Errors
/// Database errors.
pub async fn set_preflight_fields(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    f: &PreflightFields,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(Column::ReserveTokens, Expr::value(f.reserve_tokens))
        .col_expr(Column::MaxOutputTokensApplied, Expr::value(f.max_output_tokens_applied))
        .col_expr(Column::ReservedCreditsMicro, Expr::value(f.reserved_credits_micro))
        .col_expr(Column::PolicyVersionApplied, Expr::value(f.policy_version_applied))
        .col_expr(Column::EffectiveModel, Expr::value(f.effective_model.clone()))
        .col_expr(
            Column::MinimalGenerationFloorApplied,
            Expr::value(f.minimal_generation_floor_applied),
        )
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::Id.eq(turn_id))
        .filter(Column::ReserveTokens.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Refreshes `last_progress_at` (and tool counters) of a running turn.
///
/// # Errors
/// Database errors.
pub async fn touch_progress(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    counters: Option<(i32, i32, i32)>,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    let mut q = Entity::update_many()
        .col_expr(Column::LastProgressAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now));
    if let Some((ws, ci, fs)) = counters {
        q = q
            .col_expr(Column::WebSearchCompletedCount, Expr::value(ws))
            .col_expr(Column::CodeInterpreterCompletedCount, Expr::value(ci))
            .col_expr(Column::FileSearchCompletedCount, Expr::value(fs));
    }
    q.filter(Column::Id.eq(turn_id))
        .filter(Column::State.eq(state::RUNNING))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Soft-deletes a turn (retry/edit/delete).
///
/// # Errors
/// Database errors.
pub async fn soft_delete(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    replaced_by: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(Column::DeletedAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .col_expr(Column::ReplacedByRequestId, Expr::value(replaced_by))
        .filter(Column::Id.eq(turn_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Orphan candidates: running, not deleted, stale progress (oldest first).
///
/// # Errors
/// Database errors.
pub async fn orphan_candidates(
    runner: &impl DBRunner,
    cutoff: DateTime<Utc>,
    limit: u64,
) -> Result<Vec<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::State.eq(state::RUNNING))
        .filter(Column::DeletedAt.is_null())
        .filter(stale_predicate(cutoff))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(Column::StartedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

fn stale_predicate(cutoff: DateTime<Utc>) -> Condition {
    Condition::any()
        .add(Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(Column::LastProgressAt.is_null())
                .add(Column::StartedAt.lte(cutoff)),
        )
}

/// Orphan finalization CAS (re-checks the stale predicate).
///
/// # Errors
/// Database errors.
pub async fn orphan_cas(
    runner: &impl DBRunner,
    turn_id: Uuid,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    Ok(Entity::update_many()
        .col_expr(Column::State, Expr::value(state::FAILED))
        .col_expr(Column::ErrorCode, Expr::value("orphan_timeout"))
        .col_expr(Column::CompletedAt, Expr::value(now))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::Id.eq(turn_id))
        .filter(Column::State.eq(state::RUNNING))
        .filter(Column::DeletedAt.is_null())
        .filter(stale_predicate(cutoff))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(runner)
        .await?
        .rows_affected)
}
