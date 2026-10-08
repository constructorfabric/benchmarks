//! `chat_turns` repository. All terminal transitions are CAS-guarded on
//! `state = 'running'`.

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_turn;

pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETED: &str = "completed";
pub const STATE_FAILED: &str = "failed";
pub const STATE_CANCELLED: &str = "cancelled";

/// Preflight (reserve) fields of a turn; written once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightFields {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: i32,
}

pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    m: &chat_turn::Model,
) -> Result<(), DomainError> {
    let am = chat_turn::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        request_id: Set(m.request_id),
        requester_type: Set(m.requester_type.clone()),
        requester_user_id: Set(m.requester_user_id),
        state: Set(m.state.clone()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(m.reserve_tokens),
        max_output_tokens_applied: Set(m.max_output_tokens_applied),
        reserved_credits_micro: Set(m.reserved_credits_micro),
        policy_version_applied: Set(m.policy_version_applied),
        effective_model: Set(m.effective_model.clone()),
        minimal_generation_floor_applied: Set(m.minimal_generation_floor_applied),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(m.started_at),
        last_progress_at: Set(m.last_progress_at),
        web_search_enabled: Set(m.web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(m.updated_at),
    };
    chat_turn::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(())
}

/// Turn by `(chat_id, request_id)` including soft-deleted rows.
pub async fn find_by_request(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// The running, non-deleted turn of a chat, if any.
pub async fn find_running(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Latest non-deleted turn: greatest `(started_at, id)`.
pub async fn latest(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .order_by(chat_turn::Column::StartedAt, Order::Desc)
        .order_by(chat_turn::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Write the preflight fields of a retry/edit turn (only while NULL).
pub async fn fill_preflight(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    f: &PreflightFields,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chat_turn::Entity::update_many()
        .secure()
        .col_expr(
            chat_turn::Column::ReserveTokens,
            Expr::value(f.reserve_tokens),
        )
        .col_expr(
            chat_turn::Column::MaxOutputTokensApplied,
            Expr::value(f.max_output_tokens_applied),
        )
        .col_expr(
            chat_turn::Column::ReservedCreditsMicro,
            Expr::value(f.reserved_credits_micro),
        )
        .col_expr(
            chat_turn::Column::PolicyVersionApplied,
            Expr::value(f.policy_version_applied),
        )
        .col_expr(
            chat_turn::Column::EffectiveModel,
            Expr::value(f.effective_model.clone()),
        )
        .col_expr(
            chat_turn::Column::MinimalGenerationFloorApplied,
            Expr::value(f.minimal_generation_floor_applied),
        )
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING))
                .add(chat_turn::Column::ReserveTokens.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Refresh `last_progress_at` of a running turn.
pub async fn touch_progress(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chat_turn::Entity::update_many()
        .secure()
        .col_expr(chat_turn::Column::LastProgressAt, Expr::value(now))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Increment `file_search_completed_count` of a running turn.
pub async fn inc_file_search(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
) -> Result<(), DomainError> {
    chat_turn::Entity::update_many()
        .secure()
        .col_expr(
            chat_turn::Column::FileSearchCompletedCount,
            Expr::col(chat_turn::Column::FileSearchCompletedCount).add(1),
        )
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Terminal transition parameters.
#[derive(Debug, Clone, Default)]
pub struct Terminal {
    pub state: &'static str,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub web_search_completed_count: Option<i32>,
    pub code_interpreter_completed_count: Option<i32>,
    pub file_search_completed_count: Option<i32>,
}

/// CAS `running` → terminal. `true` when this caller won.
pub async fn cas_finalize(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    t: &Terminal,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let mut upd = chat_turn::Entity::update_many()
        .secure()
        .col_expr(chat_turn::Column::State, Expr::value(t.state))
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
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
        );
    if let Some(p) = &t.provider_response_id {
        upd = upd.col_expr(
            chat_turn::Column::ProviderResponseId,
            Expr::value(p.clone()),
        );
    }
    if let Some(n) = t.web_search_completed_count {
        upd = upd.col_expr(chat_turn::Column::WebSearchCompletedCount, Expr::value(n));
    }
    if let Some(n) = t.code_interpreter_completed_count {
        upd = upd.col_expr(
            chat_turn::Column::CodeInterpreterCompletedCount,
            Expr::value(n),
        );
    }
    if let Some(n) = t.file_search_completed_count {
        upd = upd.col_expr(chat_turn::Column::FileSearchCompletedCount, Expr::value(n));
    }
    let res = upd
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Soft-delete a turn (retry/edit set `replaced_by_request_id`).
pub async fn soft_delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    replaced_by: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chat_turn::Entity::update_many()
        .secure()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(now))
        .col_expr(
            chat_turn::Column::ReplacedByRequestId,
            Expr::value(replaced_by),
        )
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(chat_turn::Column::State.ne(STATE_RUNNING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

fn stale(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turn::Column::LastProgressAt.is_null())
                .add(chat_turn::Column::StartedAt.lte(cutoff)),
        )
}

/// Orphan candidates (at most `limit`).
pub async fn orphan_candidates(
    runner: &impl DBRunner,
    scope: &AccessScope,
    cutoff: OffsetDateTime,
    limit: u64,
) -> Result<Vec<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::State.eq(STATE_RUNNING))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(stale(cutoff)),
        )
        .secure()
        .scope_with(scope)
        .order_by(chat_turn::Column::StartedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// Orphan CAS: re-checks state, deletion and the stale-progress predicate.
pub async fn orphan_cas(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chat_turn::Entity::update_many()
        .secure()
        .col_expr(chat_turn::Column::State, Expr::value(STATE_FAILED))
        .col_expr(chat_turn::Column::ErrorCode, Expr::value("orphan_timeout"))
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(stale(cutoff)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}
