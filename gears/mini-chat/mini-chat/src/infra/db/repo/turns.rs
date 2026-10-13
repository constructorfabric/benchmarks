//! `TurnRepository` over the Secure ORM.

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::{TurnLedgerRepository, TurnLifecycleRepository, TurnRepository};
use crate::domain::turns::{
    NewTurn, NewUnstartedTurn, OrphanCandidate, ToolCounts, ToolKind, TurnFinalization,
    TurnPreflightColumns, TurnRow, TurnState,
};
use crate::infra::db::entity::chat_turns;
use crate::infra::db::timestamp::{from_db, to_db};

fn to_row(model: chat_turns::Model) -> TurnRow {
    TurnRow {
        id: model.id,
        request_id: model.request_id,
        state: model.state,
        assistant_message_id: model.assistant_message_id,
        error_code: model.error_code,
        effective_model: model.effective_model,
        requester_user_id: model.requester_user_id,
        web_search_enabled: model.web_search_enabled,
        deleted_at: model.deleted_at.map(from_db),
        started_at: from_db(model.started_at),
        updated_at: model.updated_at.map(from_db),
    }
}

#[async_trait]
impl<R: DBRunner> TurnRepository for R {
    async fn find_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        let model = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::RequestId.eq(request_id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn find_live_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        let model = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::RequestId.eq(request_id))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn find_running_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        let model = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn insert_turn(&self, scope: &AccessScope, turn: NewTurn) -> Result<(), DomainError> {
        secure_insert::<chat_turns::Entity>(
            chat_turns::ActiveModel {
                id: Set(turn.id),
                tenant_id: Set(turn.tenant_id),
                chat_id: Set(turn.chat_id),
                request_id: Set(turn.request_id),
                requester_type: Set("user".to_owned()),
                requester_user_id: Set(Some(turn.requester_user_id)),
                state: Set(TurnState::Running.as_str().to_owned()),
                provider_name: Set(None),
                provider_response_id: Set(None),
                assistant_message_id: Set(None),
                error_code: Set(None),
                reserve_tokens: Set(Some(i64::try_from(turn.reserve_tokens).unwrap_or(i64::MAX))),
                max_output_tokens_applied: Set(Some(
                    i32::try_from(turn.max_output_tokens_applied).unwrap_or(i32::MAX),
                )),
                reserved_credits_micro: Set(Some(turn.reserved_credits_micro)),
                policy_version_applied: Set(Some(
                    i64::try_from(turn.policy_version_applied).unwrap_or(i64::MAX),
                )),
                effective_model: Set(Some(turn.effective_model)),
                minimal_generation_floor_applied: Set(Some(
                    i32::try_from(turn.minimal_generation_floor_applied).unwrap_or(i32::MAX),
                )),
                error_detail: Set(None),
                deleted_at: Set(None),
                replaced_by_request_id: Set(None),
                started_at: Set(to_db(turn.now)),
                last_progress_at: Set(Some(to_db(turn.now))),
                web_search_enabled: Set(turn.web_search_enabled),
                web_search_completed_count: Set(0),
                code_interpreter_completed_count: Set(0),
                file_search_completed_count: Set(0),
                completed_at: Set(None),
                updated_at: Set(Some(to_db(turn.now))),
            },
            scope,
            self,
        )
        .await?;
        Ok(())
    }

    async fn touch_turn_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(to_db(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(())
    }

    async fn count_tool_done(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        tool: ToolKind,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let counter = match tool {
            ToolKind::FileSearch => chat_turns::Column::FileSearchCompletedCount,
            ToolKind::WebSearch => chat_turns::Column::WebSearchCompletedCount,
            ToolKind::CodeInterpreter => chat_turns::Column::CodeInterpreterCompletedCount,
        };
        chat_turns::Entity::update_many()
            .col_expr(counter, Expr::col(counter).add(1))
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(to_db(now)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(())
    }

    async fn finalize_turn(
        &self,
        scope: &AccessScope,
        finalization: &TurnFinalization,
    ) -> Result<bool, DomainError> {
        let now = to_db(finalization.now);
        let updated = chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::State,
                Expr::value(finalization.state.as_str()),
            )
            .col_expr(
                chat_turns::Column::ErrorCode,
                Expr::value(finalization.error_code.clone()),
            )
            .col_expr(
                chat_turns::Column::AssistantMessageId,
                Expr::value(finalization.assistant_message_id),
            )
            .col_expr(
                chat_turns::Column::ProviderResponseId,
                Expr::value(finalization.provider_response_id.clone()),
            )
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(now))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(chat_turns::Column::Id.eq(finalization.turn_id))
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }
}

fn count(value: i32) -> u32 {
    u32::try_from(value).unwrap_or_default()
}

#[async_trait]
impl<R: DBRunner> TurnLedgerRepository for R {
    async fn tool_counts(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
    ) -> Result<ToolCounts, DomainError> {
        let model = chat_turns::Entity::find()
            .filter(chat_turns::Column::Id.eq(turn_id))
            .secure()
            .scope_with(scope)
            .one(self)
            .await?
            .ok_or_else(|| DomainError::internal(format!("turn {turn_id} is not visible")))?;
        Ok(ToolCounts {
            web_search: count(model.web_search_completed_count),
            code_interpreter: count(model.code_interpreter_completed_count),
            file_search: count(model.file_search_completed_count),
        })
    }
}

/// `orphan_timeout`: the error code of a turn finalized by the watchdog.
const ORPHAN_TIMEOUT: &str = "orphan_timeout";

fn to_candidate(model: chat_turns::Model) -> OrphanCandidate {
    OrphanCandidate {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        requester_user_id: model.requester_user_id,
        started_at: from_db(model.started_at),
        effective_model: model.effective_model,
        policy_version_applied: model
            .policy_version_applied
            .and_then(|v| u64::try_from(v).ok()),
        reserve_tokens: model.reserve_tokens.and_then(|v| u64::try_from(v).ok()),
        max_output_tokens_applied: model
            .max_output_tokens_applied
            .and_then(|v| u32::try_from(v).ok()),
        reserved_credits_micro: model.reserved_credits_micro,
        minimal_generation_floor_applied: model
            .minimal_generation_floor_applied
            .and_then(|v| u32::try_from(v).ok()),
    }
}

/// `last_progress_at <= cutoff`, or `last_progress_at IS NULL AND started_at <=
/// cutoff` (DESIGN "Orphan Turn Watchdog"): the stale-progress predicate of the
/// scan and of the orphan CAS.
fn stale_progress(cutoff: OffsetDateTime) -> Condition {
    let cutoff = to_db(cutoff);
    Condition::any()
        .add(chat_turns::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turns::Column::LastProgressAt.is_null())
                .add(chat_turns::Column::StartedAt.lte(cutoff)),
        )
}

#[async_trait]
impl<R: DBRunner> TurnLifecycleRepository for R {
    async fn find_latest_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        let model = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .order_by(chat_turns::Column::StartedAt, Order::Desc)
            .order_by(chat_turns::Column::Id, Order::Desc)
            .one(self)
            .await?;
        Ok(model.map(to_row))
    }

    async fn has_newer_live_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        than: &TurnRow,
    ) -> Result<bool, DomainError> {
        let started = to_db(than.started_at);
        let model = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .filter(
                Condition::any()
                    .add(chat_turns::Column::StartedAt.gt(started))
                    .add(
                        Condition::all()
                            .add(chat_turns::Column::StartedAt.eq(started))
                            .add(chat_turns::Column::Id.gt(than.id)),
                    ),
            )
            .secure()
            .scope_with(scope)
            .one(self)
            .await?;
        Ok(model.is_some())
    }

    async fn soft_delete_turn(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        replaced_by: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let updated = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::DeletedAt, Expr::value(to_db(now)))
            .col_expr(
                chat_turns::Column::ReplacedByRequestId,
                Expr::value(replaced_by),
            )
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(to_db(now)))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .filter(chat_turns::Column::State.ne(TurnState::Running.as_str()))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn insert_unstarted_turn(
        &self,
        scope: &AccessScope,
        turn: NewUnstartedTurn,
    ) -> Result<(), DomainError> {
        secure_insert::<chat_turns::Entity>(
            chat_turns::ActiveModel {
                id: Set(turn.id),
                tenant_id: Set(turn.tenant_id),
                chat_id: Set(turn.chat_id),
                request_id: Set(turn.request_id),
                requester_type: Set("user".to_owned()),
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
                started_at: Set(to_db(turn.now)),
                last_progress_at: Set(Some(to_db(turn.now))),
                web_search_enabled: Set(turn.web_search_enabled),
                web_search_completed_count: Set(0),
                code_interpreter_completed_count: Set(0),
                file_search_completed_count: Set(0),
                completed_at: Set(None),
                updated_at: Set(Some(to_db(turn.now))),
            },
            scope,
            self,
        )
        .await?;
        Ok(())
    }

    async fn record_turn_reserve(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        columns: &TurnPreflightColumns,
    ) -> Result<bool, DomainError> {
        let updated = chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::ReserveTokens,
                Expr::value(i64::try_from(columns.reserve_tokens).unwrap_or(i64::MAX)),
            )
            .col_expr(
                chat_turns::Column::MaxOutputTokensApplied,
                Expr::value(i32::try_from(columns.max_output_tokens_applied).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turns::Column::ReservedCreditsMicro,
                Expr::value(columns.reserved_credits_micro),
            )
            .col_expr(
                chat_turns::Column::PolicyVersionApplied,
                Expr::value(i64::try_from(columns.policy_version_applied).unwrap_or(i64::MAX)),
            )
            .col_expr(
                chat_turns::Column::EffectiveModel,
                Expr::value(columns.effective_model.clone()),
            )
            .col_expr(
                chat_turns::Column::MinimalGenerationFloorApplied,
                Expr::value(
                    i32::try_from(columns.minimal_generation_floor_applied).unwrap_or(i32::MAX),
                ),
            )
            .col_expr(
                chat_turns::Column::UpdatedAt,
                Expr::value(to_db(columns.now)),
            )
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    async fn stale_running_turns(
        &self,
        scope: &AccessScope,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        let models = chat_turns::Entity::find()
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .filter(stale_progress(cutoff))
            .secure()
            .scope_with(scope)
            .order_by(chat_turns::Column::StartedAt, Order::Asc)
            .order_by(chat_turns::Column::Id, Order::Asc)
            .limit(limit)
            .all(self)
            .await?;
        Ok(models.into_iter().map(to_candidate).collect())
    }

    async fn finalize_orphan_turn(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let now = to_db(now);
        let updated = chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::State,
                Expr::value(TurnState::Failed.as_str()),
            )
            .col_expr(chat_turns::Column::ErrorCode, Expr::value(ORPHAN_TIMEOUT))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(now))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(TurnState::Running.as_str()))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .filter(stale_progress(cutoff))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(updated.rows_affected == 1)
    }
}
