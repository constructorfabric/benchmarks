//! Chat turn repository (lifecycle + CAS finalization).

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{Db, now};
use super::super::entity::chat_turns;
use crate::error::DomainError;

/// Turn states (`chat_turns.state`).
pub mod state {
    /// Generation in progress.
    pub const RUNNING: &str = "running";
    /// Completed.
    pub const COMPLETED: &str = "completed";
    /// Failed.
    pub const FAILED: &str = "failed";
    /// Cancelled.
    pub const CANCELLED: &str = "cancelled";
}

/// New turn row (send path; preflight fields set).
pub struct NewTurn {
    /// Turn id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Correlation key.
    pub request_id: Uuid,
    /// Requesting user.
    pub requester_user_id: Uuid,
    /// Web search requested.
    pub web_search_enabled: bool,
    /// Preflight token reserve.
    pub reserve_tokens: i64,
    /// Applied max output tokens.
    pub max_output_tokens_applied: i32,
    /// Reserved credits.
    pub reserved_credits_micro: i64,
    /// Policy version.
    pub policy_version_applied: i64,
    /// Effective model.
    pub effective_model: String,
    /// Applied minimal generation floor.
    pub minimal_generation_floor_applied: i32,
}

/// Preflight fields filled later for retry/edit turns.
pub struct PreflightFields {
    /// Preflight token reserve.
    pub reserve_tokens: i64,
    /// Applied max output tokens.
    pub max_output_tokens_applied: i32,
    /// Reserved credits.
    pub reserved_credits_micro: i64,
    /// Policy version.
    pub policy_version_applied: i64,
    /// Effective model.
    pub effective_model: String,
    /// Applied minimal generation floor.
    pub minimal_generation_floor_applied: i32,
}

/// Turn repository.
#[derive(Clone)]
pub struct TurnRepo {
    #[allow(dead_code)]
    db: Db,
}

impl TurnRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Insert a running turn with preflight fields (send path).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        new: NewTurn,
    ) -> Result<chat_turns::Model, DomainError> {
        let now = now();
        let am = chat_turns::ActiveModel {
            id: ActiveValue::Set(new.id),
            tenant_id: ActiveValue::Set(new.tenant_id),
            chat_id: ActiveValue::Set(new.chat_id),
            request_id: ActiveValue::Set(new.request_id),
            requester_type: ActiveValue::Set("user".to_owned()),
            requester_user_id: ActiveValue::Set(Some(new.requester_user_id)),
            state: ActiveValue::Set(state::RUNNING.to_owned()),
            provider_name: ActiveValue::Set(None),
            provider_response_id: ActiveValue::Set(None),
            assistant_message_id: ActiveValue::Set(None),
            error_code: ActiveValue::Set(None),
            reserve_tokens: ActiveValue::Set(Some(new.reserve_tokens)),
            max_output_tokens_applied: ActiveValue::Set(Some(new.max_output_tokens_applied)),
            reserved_credits_micro: ActiveValue::Set(Some(new.reserved_credits_micro)),
            policy_version_applied: ActiveValue::Set(Some(new.policy_version_applied)),
            effective_model: ActiveValue::Set(Some(new.effective_model)),
            minimal_generation_floor_applied: ActiveValue::Set(Some(
                new.minimal_generation_floor_applied,
            )),
            error_detail: ActiveValue::Set(None),
            deleted_at: ActiveValue::Set(None),
            replaced_by_request_id: ActiveValue::Set(None),
            started_at: ActiveValue::Set(now),
            last_progress_at: ActiveValue::Set(Some(now)),
            web_search_enabled: ActiveValue::Set(new.web_search_enabled),
            web_search_completed_count: ActiveValue::Set(0),
            code_interpreter_completed_count: ActiveValue::Set(0),
            file_search_completed_count: ActiveValue::Set(0),
            completed_at: ActiveValue::Set(None),
            updated_at: ActiveValue::Set(now),
        };
        let model = toolkit_db::secure::secure_insert::<chat_turns::Entity>(am, &super::tenant_scope(scope), runner).await?;
        Ok(model)
    }

    /// Insert a running turn without preflight fields (mutation path; filled
    /// by the reserve transaction).
    pub async fn insert_unstarted(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
        requester_user_id: Uuid,
        web_search_enabled: bool,
    ) -> Result<chat_turns::Model, DomainError> {
        let now = now();
        let am = chat_turns::ActiveModel {
            id: ActiveValue::Set(id),
            tenant_id: ActiveValue::Set(tenant_id),
            chat_id: ActiveValue::Set(chat_id),
            request_id: ActiveValue::Set(request_id),
            requester_type: ActiveValue::Set("user".to_owned()),
            requester_user_id: ActiveValue::Set(Some(requester_user_id)),
            state: ActiveValue::Set(state::RUNNING.to_owned()),
            provider_name: ActiveValue::Set(None),
            provider_response_id: ActiveValue::Set(None),
            assistant_message_id: ActiveValue::Set(None),
            error_code: ActiveValue::Set(None),
            reserve_tokens: ActiveValue::Set(None),
            max_output_tokens_applied: ActiveValue::Set(None),
            reserved_credits_micro: ActiveValue::Set(None),
            policy_version_applied: ActiveValue::Set(None),
            effective_model: ActiveValue::Set(None),
            minimal_generation_floor_applied: ActiveValue::Set(None),
            error_detail: ActiveValue::Set(None),
            deleted_at: ActiveValue::Set(None),
            replaced_by_request_id: ActiveValue::Set(None),
            started_at: ActiveValue::Set(now),
            last_progress_at: ActiveValue::Set(Some(now)),
            web_search_enabled: ActiveValue::Set(web_search_enabled),
            web_search_completed_count: ActiveValue::Set(0),
            code_interpreter_completed_count: ActiveValue::Set(0),
            file_search_completed_count: ActiveValue::Set(0),
            completed_at: ActiveValue::Set(None),
            updated_at: ActiveValue::Set(now),
        };
        let model = toolkit_db::secure::secure_insert::<chat_turns::Entity>(am, &super::tenant_scope(scope), runner).await?;
        Ok(model)
    }

    /// Fill the preflight fields of an unstarted turn (reserve transaction).
    pub async fn fill_preflight(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        fields: &PreflightFields,
    ) -> Result<(), DomainError> {
        let now = now();
        chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::ReserveTokens,
                Expr::value(Some(fields.reserve_tokens)),
            )
            .col_expr(
                chat_turns::Column::MaxOutputTokensApplied,
                Expr::value(Some(fields.max_output_tokens_applied)),
            )
            .col_expr(
                chat_turns::Column::ReservedCreditsMicro,
                Expr::value(Some(fields.reserved_credits_micro)),
            )
            .col_expr(
                chat_turns::Column::PolicyVersionApplied,
                Expr::value(Some(fields.policy_version_applied)),
            )
            .col_expr(
                chat_turns::Column::EffectiveModel,
                Expr::value(Some(fields.effective_model.clone())),
            )
            .col_expr(
                chat_turns::Column::MinimalGenerationFloorApplied,
                Expr::value(Some(fields.minimal_generation_floor_applied)),
            )
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Load a turn by `(chat_id, request_id)`, including soft-deleted rows.
    pub async fn get_any(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<chat_turns::Model>, DomainError> {
        let row = chat_turns::Entity::find()
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .filter(Condition::all().add(chat_turns::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chat_turns::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(chat_turns::Column::RequestId.eq(request_id)))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Load a turn by id (tenant-filtered).
    pub async fn get_by_id(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<chat_turns::Model>, DomainError> {
        let row = chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_turns::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Any running turn in the chat (any request id).
    pub async fn any_running(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<chat_turns::Model>, DomainError> {
        let row = chat_turns::Entity::find()
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .filter(Condition::all().add(chat_turns::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chat_turns::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .filter(Condition::all().add(chat_turns::Column::DeletedAt.is_null()))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// The latest non-deleted turn of a chat (greatest `(started_at, id)`).
    pub async fn latest(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<chat_turns::Model>, DomainError> {
        let row = chat_turns::Entity::find()
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .filter(Condition::all().add(chat_turns::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(chat_turns::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(chat_turns::Column::DeletedAt.is_null()))
            .order_by(chat_turns::Column::StartedAt, sea_orm::Order::Desc)
            .order_by(chat_turns::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .one(runner)
            .await?;
        Ok(row)
    }

    /// CAS-finalize a running turn as `completed`.
    ///
    /// Returns `true` when this finalizer won the CAS.
    pub async fn finalize_completed(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        assistant_message_id: Uuid,
        provider_response_id: Option<String>,
    ) -> Result<bool, DomainError> {
        let now = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value(state::COMPLETED))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                chat_turns::Column::AssistantMessageId,
                Expr::value(Some(assistant_message_id)),
            )
            .col_expr(
                chat_turns::Column::ProviderResponseId,
                Expr::value(provider_response_id),
            )
            .col_expr(chat_turns::Column::ErrorCode, Expr::value(Option::<String>::None))
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// CAS-finalize a running turn as `failed`.
    pub async fn finalize_failed(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        error_code: &str,
        error_detail: Option<&str>,
    ) -> Result<bool, DomainError> {
        let now = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value(state::FAILED))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                chat_turns::Column::ErrorCode,
                Expr::value(Some(error_code.to_owned())),
            )
            .col_expr(
                chat_turns::Column::ErrorDetail,
                Expr::value(error_detail.map(str::to_owned)),
            )
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// CAS-finalize a running turn as `cancelled`.
    pub async fn finalize_cancelled(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        assistant_message_id: Option<Uuid>,
    ) -> Result<bool, DomainError> {
        let now = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value(state::CANCELLED))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                chat_turns::Column::AssistantMessageId,
                Expr::value(assistant_message_id),
            )
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Orphan watchdog CAS: finalize a stale running turn as
    /// `failed`/`orphan_timeout`.
    pub async fn orphan_finalize(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let stale = Condition::any()
            .add(chat_turns::Column::LastProgressAt.lte(cutoff))
            .add(
                Condition::all()
                    .add(chat_turns::Column::LastProgressAt.is_null())
                    .add(chat_turns::Column::StartedAt.lte(cutoff)),
            );
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value(state::FAILED))
            .col_expr(
                chat_turns::Column::ErrorCode,
                Expr::value(Some("orphan_timeout".to_owned())),
            )
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .filter(Condition::all().add(chat_turns::Column::DeletedAt.is_null()))
            .filter(stale)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Orphan watchdog scan: stale running candidates (at most `limit`).
    pub async fn orphan_scan(
        &self,
        runner: &impl DBRunner,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<chat_turns::Model>, DomainError> {
        let stale = Condition::any()
            .add(chat_turns::Column::LastProgressAt.lte(cutoff))
            .add(
                Condition::all()
                    .add(chat_turns::Column::LastProgressAt.is_null())
                    .add(chat_turns::Column::StartedAt.lte(cutoff)),
            );
        let rows = chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .filter(Condition::all().add(chat_turns::Column::DeletedAt.is_null()))
            .filter(stale)
            .order_by(chat_turns::Column::StartedAt, sea_orm::Order::Asc)
            .limit(limit)
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Refresh `last_progress_at` on a running turn (throttled by caller).
    pub async fn refresh_progress(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(Some(now)))
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Set the completed tool-call counters on a running turn.
    pub async fn set_tool_counts(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        web_search: i32,
        code_interpreter: i32,
        file_search: i32,
    ) -> Result<(), DomainError> {
        chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::WebSearchCompletedCount,
                Expr::value(web_search),
            )
            .col_expr(
                chat_turns::Column::CodeInterpreterCompletedCount,
                Expr::value(code_interpreter),
            )
            .col_expr(
                chat_turns::Column::FileSearchCompletedCount,
                Expr::value(file_search),
            )
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::State.eq(state::RUNNING)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Soft-delete a turn, optionally marking the replacement request id.
    pub async fn soft_delete(
        &self,
        runner: &impl DBRunner,
        turn_id: Uuid,
        replaced_by: Option<Uuid>,
    ) -> Result<bool, DomainError> {
        let now = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                chat_turns::Column::ReplacedByRequestId,
                Expr::value(replaced_by),
            )
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .filter(Condition::all().add(chat_turns::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }
}
