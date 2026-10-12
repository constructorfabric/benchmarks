//! `SeaORM` implementation of the turn repository over Secure ORM: the reserve transaction, the
//! finalization transaction and the turn reads.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, Func};
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::entity::{attachments, chat_turns, message_attachments, messages, thread_summaries};
use super::map_scope_err;
use super::mutation_repo::lock_chat;
use super::quota_repo::{self, RowKey, SettleDelta};
use super::rows;
use crate::domain::error::DomainError;
use crate::domain::metrics::Metrics;
use crate::domain::orphan::{OrphanCandidate, OrphanRepository, OrphanReserve};
use crate::domain::quota::Bucket;
use crate::domain::stream::{
    FinalizeCommand, FinalizeOutcome, ReserveCommand, StoredAssistantMessage, SummaryRequest,
    TurnRepository,
};
use crate::domain::summary::{Frontier, SummaryWorkItem, frozen_target};
use crate::domain::turn::{TurnRecord, TurnState};
use crate::infra::outbox::OutboxEnqueuer;

const RUNNING: &str = "running";
const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";
const FILE_SEARCH: &str = "file_search";

/// Turn repository over the gear's database.
pub struct TurnRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<dyn Metrics>,
}

impl TurnRepositoryImpl {
    /// Creates the repository; `metrics` records the summary trigger after a finalization commit.
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        outbox: Arc<OutboxEnqueuer>,
        metrics: Arc<dyn Metrics>,
    ) -> Self {
        Self {
            db,
            outbox,
            metrics,
        }
    }
}

fn to_record(model: chat_turns::Model) -> Result<TurnRecord, DomainError> {
    let state = TurnState::parse(&model.state)
        .ok_or_else(|| DomainError::internal("a chat turn has an unknown state"))?;
    Ok(TurnRecord {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        state,
        error_code: model.error_code,
        assistant_message_id: model.assistant_message_id,
        effective_model: model.effective_model,
        deleted: model.deleted_at.is_some(),
        requester_user_id: model.requester_user_id,
        web_search_enabled: model.web_search_enabled,
        started_at: model.started_at,
        updated_at: model.updated_at,
    })
}

fn user_message(command: &ReserveCommand) -> messages::ActiveModel {
    rows::user_message(
        command.user_message_id,
        command.tenant_id,
        command.chat_id,
        command.request_id,
        command.content.clone(),
        command.now,
    )
}

fn running_turn(command: &ReserveCommand) -> chat_turns::ActiveModel {
    chat_turns::ActiveModel {
        id: ActiveValue::Set(command.turn_id),
        tenant_id: ActiveValue::Set(command.tenant_id),
        chat_id: ActiveValue::Set(command.chat_id),
        request_id: ActiveValue::Set(command.request_id),
        requester_type: ActiveValue::Set("user".to_owned()),
        requester_user_id: ActiveValue::Set(Some(command.user_id)),
        state: ActiveValue::Set(RUNNING.to_owned()),
        provider_name: ActiveValue::Set(None),
        provider_response_id: ActiveValue::Set(None),
        assistant_message_id: ActiveValue::Set(None),
        error_code: ActiveValue::Set(None),
        reserve_tokens: ActiveValue::Set(Some(command.reserve_tokens)),
        max_output_tokens_applied: ActiveValue::Set(Some(command.max_output_tokens_applied)),
        reserved_credits_micro: ActiveValue::Set(Some(command.plan.reserved_credits_micro)),
        policy_version_applied: ActiveValue::Set(Some(command.policy_version)),
        effective_model: ActiveValue::Set(Some(command.effective_model.clone())),
        minimal_generation_floor_applied: ActiveValue::Set(Some(
            command.minimal_generation_floor_applied,
        )),
        error_detail: ActiveValue::Set(None),
        deleted_at: ActiveValue::Set(None),
        replaced_by_request_id: ActiveValue::Set(None),
        started_at: ActiveValue::Set(command.now),
        last_progress_at: ActiveValue::Set(Some(command.now)),
        web_search_enabled: ActiveValue::Set(command.web_search_enabled),
        web_search_completed_count: ActiveValue::Set(0),
        code_interpreter_completed_count: ActiveValue::Set(0),
        file_search_completed_count: ActiveValue::Set(0),
        completed_at: ActiveValue::Set(None),
        updated_at: ActiveValue::Set(command.now),
    }
}

#[async_trait]
impl TurnRepository for TurnRepositoryImpl {
    async fn find_by_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        let conn = self.db.conn()?;
        chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::RequestId.eq(request_id))
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(to_record)
            .transpose()
    }

    async fn find_running(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        let conn = self.db.conn()?;
        chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::State.eq(RUNNING))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(to_record)
            .transpose()
    }

    async fn assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError> {
        let conn = self.db.conn()?;
        Ok(messages::Entity::find()
            .filter(messages::Column::Id.eq(message_id))
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::Role.eq("assistant"))
            .filter(messages::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(|model| StoredAssistantMessage {
                id: model.id,
                content: model.content,
                input_tokens: model.input_tokens,
                output_tokens: model.output_tokens,
            }))
    }

    async fn reserve(
        &self,
        scope: &AccessScope,
        command: ReserveCommand,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let tenant_scope = scope.tenant_only();
                quota_repo::reserve(
                    tx,
                    &scope,
                    command.tenant_id,
                    command.user_id,
                    &command.plan,
                    command.now,
                )
                .await?;
                // The chat row is written before the attachment links are validated and inserted:
                // an attachment delete takes the same lock before it counts the references, so
                // of the two exactly one sees the other's committed result. The lock order is
                // quota rows, chat row, turn insert.
                lock_chat(tx, &scope, command.chat_id, Some(command.now)).await?;
                let message = user_message(&command);
                secure_insert::<messages::Entity>(message, &tenant_scope, tx)
                    .await
                    .map_err(map_scope_err)?;
                if !command.attachment_ids.is_empty() {
                    link_attachments(tx, &tenant_scope, &command).await?;
                }
                secure_insert::<chat_turns::Entity>(running_turn(&command), &tenant_scope, tx)
                    .await
                    .map_err(map_scope_err)?;
                Ok(((), Wake::empty()))
            })
        })
        .await
    }

    async fn finalize(
        &self,
        scope: &AccessScope,
        command: FinalizeCommand,
    ) -> Result<FinalizeOutcome, DomainError> {
        let scope = scope.clone();
        let outbox = Arc::clone(&self.outbox);
        let ((outcome, announced), trigger) = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let tenant_scope = scope.tenant_only();
                if !cas_terminal(tx, &tenant_scope, &command).await? {
                    return Ok::<_, DomainError>((
                        ((FinalizeOutcome::Lost, Vec::new()), None),
                        Wake::empty(),
                    ));
                }
                if let Some(message) = &command.assistant_message {
                    insert_assistant_message(tx, &tenant_scope, &command, message).await?;
                }
                if command.book_quota {
                    settle_quota(tx, &scope, &command).await?;
                }
                // Reads first, enqueues last: an error after an enqueue would drop its wake.
                let summary = match &command.summary {
                    Some(request) => {
                        Some(plan_summary(tx, &tenant_scope, &command, request).await?)
                    }
                    None => None,
                };
                let (usage_wake, usage_message) =
                    outbox.enqueue_usage(tx, &command.usage_event).await?;
                let (audit_wake, audit_message) =
                    outbox.enqueue_audit(tx, &command.audit_event).await?;
                let mut wake = usage_wake + audit_wake;
                let mut announced = vec![usage_message, audit_message];
                let trigger = match summary {
                    Some(Some(item)) => {
                        let (summary_wake, message) = outbox.enqueue_summary(tx, &item).await?;
                        wake += summary_wake;
                        announced.push(message);
                        Some("scheduled")
                    }
                    Some(None) => Some("not_needed"),
                    None => None,
                };
                Ok((((FinalizeOutcome::Committed, announced), trigger), wake))
            })
        })
        .await?;
        self.outbox.announce(&announced);
        if let Some(result) = trigger {
            self.metrics.thread_summary_trigger(result);
        }
        Ok(outcome)
    }

    async fn touch_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(now))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(RUNNING))
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(())
    }

    async fn record_tool_completed(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        tool: &str,
    ) -> Result<(), DomainError> {
        let column = match tool {
            WEB_SEARCH => chat_turns::Column::WebSearchCompletedCount,
            CODE_INTERPRETER => chat_turns::Column::CodeInterpreterCompletedCount,
            FILE_SEARCH => chat_turns::Column::FileSearchCompletedCount,
            _ => return Ok(()),
        };
        let conn = self.db.conn()?;
        chat_turns::Entity::update_many()
            .col_expr(column, Expr::col(column).add(1))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(RUNNING))
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(())
    }
}

/// Evaluates the summary trigger in the finalization transaction of a completed turn: reads the
/// stored frontier and the latest non-deleted message outside the causing turn, and returns the
/// work item when the trigger fires and there is something to summarize.
async fn plan_summary(
    tx: &toolkit_db::DbTx<'_>,
    tenant_scope: &AccessScope,
    command: &FinalizeCommand,
    request: &SummaryRequest,
) -> Result<Option<SummaryWorkItem>, DomainError> {
    let base = thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(command.chat_id))
        .secure()
        .scope_with(tenant_scope)
        .one(tx)
        .await
        .map_err(map_scope_err)?
        .map(|row| Frontier {
            created_at: row.summarized_up_to_created_at,
            message_id: row.summarized_up_to_message_id,
        });
    let latest = messages::Entity::find()
        .filter(messages::Column::ChatId.eq(command.chat_id))
        .filter(messages::Column::DeletedAt.is_null())
        .filter(messages::Column::RequestId.is_not_null())
        .filter(messages::Column::RequestId.ne(command.request_id))
        .secure()
        .scope_with(tenant_scope)
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(messages::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .all(tx)
        .await
        .map_err(map_scope_err)?
        .into_iter()
        .next()
        .map(|row| Frontier {
            created_at: row.created_at,
            message_id: row.id,
        });
    Ok(frozen_target(request.trigger, base, latest).map(|target| {
        SummaryWorkItem::new(
            command.tenant_id,
            command.chat_id,
            request.system_request_id,
            base,
            target,
        )
    }))
}

/// Validates the attachment ids against the chat, the uploader and the `ready` status inside the
/// reserve transaction and links them to the user message.
async fn link_attachments(
    tx: &toolkit_db::DbTx<'_>,
    tenant_scope: &AccessScope,
    command: &ReserveCommand,
) -> Result<(), DomainError> {
    let ready = attachments::Entity::find()
        .filter(attachments::Column::Id.is_in(command.attachment_ids.clone()))
        .filter(attachments::Column::ChatId.eq(command.chat_id))
        .filter(attachments::Column::UploadedByUserId.eq(command.user_id))
        .filter(attachments::Column::Status.eq("ready"))
        .filter(attachments::Column::DeletedAt.is_null())
        .secure()
        .scope_with(tenant_scope)
        .count(tx)
        .await
        .map_err(map_scope_err)?;
    if usize::try_from(ready).ok() != Some(command.attachment_ids.len()) {
        return Err(DomainError::InvalidAttachment);
    }
    for (position, attachment_id) in command.attachment_ids.iter().enumerate() {
        let link = message_attachments::ActiveModel {
            tenant_id: ActiveValue::Set(command.tenant_id),
            chat_id: ActiveValue::Set(command.chat_id),
            message_id: ActiveValue::Set(command.user_message_id),
            attachment_id: ActiveValue::Set(*attachment_id),
            created_at: ActiveValue::Set(rows::link_time(command.now, position)),
        };
        secure_insert::<message_attachments::Entity>(link, tenant_scope, tx)
            .await
            .map_err(map_scope_err)?;
    }
    Ok(())
}

/// The CAS: `running` to the terminal state. `false` when another finalizer won.
async fn cas_terminal(
    tx: &toolkit_db::DbTx<'_>,
    tenant_scope: &AccessScope,
    command: &FinalizeCommand,
) -> Result<bool, DomainError> {
    let counts = command.tool_counts;
    let assistant_message_id = command.assistant_message.as_ref().map(|message| message.id);
    let affected = chat_turns::Entity::update_many()
        .col_expr(
            chat_turns::Column::State,
            Expr::value(command.state.as_str()),
        )
        .col_expr(
            chat_turns::Column::ErrorCode,
            Expr::value(command.error_code.clone()),
        )
        .col_expr(
            chat_turns::Column::ErrorDetail,
            Expr::value(command.error_detail.clone()),
        )
        .col_expr(
            chat_turns::Column::ProviderResponseId,
            Expr::value(command.provider_response_id.clone()),
        )
        .col_expr(
            chat_turns::Column::AssistantMessageId,
            Expr::value(assistant_message_id),
        )
        .col_expr(
            chat_turns::Column::WebSearchCompletedCount,
            Expr::value(i32::try_from(counts.web_search).unwrap_or(i32::MAX)),
        )
        .col_expr(
            chat_turns::Column::CodeInterpreterCompletedCount,
            Expr::value(i32::try_from(counts.code_interpreter).unwrap_or(i32::MAX)),
        )
        .col_expr(
            chat_turns::Column::FileSearchCompletedCount,
            Expr::value(i32::try_from(counts.file_search).unwrap_or(i32::MAX)),
        )
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(command.now))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(command.now))
        .filter(chat_turns::Column::Id.eq(command.turn_id))
        .filter(chat_turns::Column::State.eq(RUNNING))
        .filter(match command.orphan_cutoff {
            Some(cutoff) => Condition::all()
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(stale_progress(cutoff)),
            None => Condition::all(),
        })
        // The watchdog booked nothing because the scan read NULL reserve fields: a reserve filled
        // since then makes the CAS lose, so the next scan re-reads it and settles the reserve.
        .filter(if command.require_unreserved {
            Condition::all().add(chat_turns::Column::ReserveTokens.is_null())
        } else {
            Condition::all()
        })
        .secure()
        .scope_with(tenant_scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    Ok(affected == 1)
}

async fn insert_assistant_message(
    tx: &toolkit_db::DbTx<'_>,
    tenant_scope: &AccessScope,
    command: &FinalizeCommand,
    message: &crate::domain::stream::AssistantMessage,
) -> Result<(), DomainError> {
    let usage = message.usage.unwrap_or_default();
    let model = messages::ActiveModel {
        id: ActiveValue::Set(message.id),
        tenant_id: ActiveValue::Set(command.tenant_id),
        chat_id: ActiveValue::Set(command.chat_id),
        request_id: ActiveValue::Set(Some(command.request_id)),
        role: ActiveValue::Set("assistant".to_owned()),
        content: ActiveValue::Set(message.content.clone()),
        content_type: ActiveValue::Set("text".to_owned()),
        token_estimate: ActiveValue::Set(0),
        provider_response_id: ActiveValue::Set(message.provider_response_id.clone()),
        request_kind: ActiveValue::Set("chat".to_owned()),
        features_used: ActiveValue::Set(serde_json::json!([])),
        input_tokens: ActiveValue::Set(std::cmp::max(usage.input_tokens, 0)),
        output_tokens: ActiveValue::Set(std::cmp::max(usage.output_tokens, 0)),
        cache_read_input_tokens: ActiveValue::Set(std::cmp::max(usage.cache_read_input_tokens, 0)),
        cache_write_input_tokens: ActiveValue::Set(std::cmp::max(
            usage.cache_write_input_tokens,
            0,
        )),
        reasoning_tokens: ActiveValue::Set(std::cmp::max(usage.reasoning_tokens, 0)),
        model: ActiveValue::Set(Some(message.model.clone())),
        is_compressed: ActiveValue::Set(false),
        created_at: ActiveValue::Set(command.now),
        deleted_at: ActiveValue::Set(None),
    };
    secure_insert::<messages::Entity>(model, tenant_scope, tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

async fn settle_quota(
    tx: &toolkit_db::DbTx<'_>,
    scope: &AccessScope,
    command: &FinalizeCommand,
) -> Result<(), DomainError> {
    let settlement = &command.settlement;
    let delta = SettleDelta {
        reserved_credits_micro: command.reserved_credits_micro,
        committed_credits_micro: settlement.committed_credits_micro,
        input_tokens: settlement.telemetry_input_tokens,
        output_tokens: settlement.telemetry_output_tokens,
        web_search_calls: tool_calls(command.tool_counts.web_search, settlement),
        code_interpreter_calls: tool_calls(command.tool_counts.code_interpreter, settlement),
        // An orphan release must never take a bucket below zero (it would fail the CHECK and
        // keep the turn `running` for good).
        clamp_reserved_at_zero: command.orphan_cutoff.is_some(),
    };
    let premium = premium_bucket_settled(command);
    let buckets: &[Bucket] = if premium {
        &[Bucket::Total, Bucket::PremiumTier]
    } else {
        &[Bucket::Total]
    };
    for &bucket in buckets {
        for period in &command.periods {
            let key = RowKey {
                tenant_id: command.tenant_id,
                user_id: command.user_id,
                period: *period,
                bucket,
            };
            quota_repo::settle(tx, scope, key, &delta, command.now).await?;
        }
    }
    Ok(())
}

/// Whether the premium bucket is settled with the turn. A turn of unknown tier (an orphan whose
/// model cannot be resolved) releases only `total`: the premium bucket is shared by every premium
/// turn of the user, so no guess is made from its rows and the premium headroom stays held back
/// (fail closed).
fn premium_bucket_settled(command: &FinalizeCommand) -> bool {
    if let Some(premium) = command.premium {
        return premium;
    }
    if command.reserved_credits_micro > 0 {
        tracing::error!(
            turn_id = %command.turn_id,
            tenant_id = %command.tenant_id,
            user_id = %command.user_id,
            leaked_premium_reserve_credits_micro = command.reserved_credits_micro,
            "the tier of the orphan turn is unresolvable; the premium bucket reserve is not released"
        );
    }
    false
}

/// Tool calls are added on actual and estimated settlements, not on released ones.
fn tool_calls(count: u32, settlement: &crate::domain::billing::Settlement) -> i32 {
    use mini_chat_sdk::SettlementMethod;
    match settlement.method {
        SettlementMethod::Actual | SettlementMethod::Estimated => {
            i32::try_from(count).unwrap_or(i32::MAX)
        }
        SettlementMethod::Released | SettlementMethod::None => 0,
    }
}

/// `last_progress_at <= cutoff OR (last_progress_at IS NULL AND started_at <= cutoff)`: the
/// stale-progress predicate shared by the candidate scan and the orphan CAS.
fn stale_progress(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turns::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turns::Column::LastProgressAt.is_null())
                .add(chat_turns::Column::StartedAt.lte(cutoff)),
        )
}

fn to_candidate(model: chat_turns::Model) -> OrphanCandidate {
    let reserve = match (
        model.reserve_tokens,
        model.max_output_tokens_applied,
        model.reserved_credits_micro,
        model.policy_version_applied,
        model.effective_model,
        model.minimal_generation_floor_applied,
    ) {
        (
            Some(reserve_tokens),
            Some(max_output_tokens_applied),
            Some(reserved_credits_micro),
            Some(policy_version),
            Some(effective_model),
            Some(minimal_generation_floor_applied),
        ) => Some(OrphanReserve {
            reserve_tokens,
            max_output_tokens_applied: i64::from(max_output_tokens_applied),
            reserved_credits_micro,
            policy_version,
            effective_model,
            minimal_generation_floor_applied: i64::from(minimal_generation_floor_applied),
        }),
        _ => None,
    };
    OrphanCandidate {
        turn_id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        user_id: model.requester_user_id,
        started_at: model.started_at,
        last_progress_at: model.last_progress_at,
        web_search_calls: u32::try_from(model.web_search_completed_count).unwrap_or(0),
        code_interpreter_calls: u32::try_from(model.code_interpreter_completed_count).unwrap_or(0),
        file_search_calls: u32::try_from(model.file_search_completed_count).unwrap_or(0),
        reserve,
    }
}

#[async_trait]
impl OrphanRepository for TurnRepositoryImpl {
    async fn stale_turns(
        &self,
        cutoff: OffsetDateTime,
        limit: u64,
        attempted: &[Uuid],
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        let conn = self.db.conn()?;
        // The scan is a system task without a tenant: candidate discovery is advisory only, every
        // finalization re-checks the predicate under the scope of the candidate's own tenant.
        let mut select = chat_turns::Entity::find()
            .filter(chat_turns::Column::State.eq(RUNNING))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .filter(stale_progress(cutoff));
        if !attempted.is_empty() {
            select = select.filter(chat_turns::Column::Id.is_not_in(attempted.iter().copied()));
        }
        let progress = Func::coalesce([
            Expr::col(chat_turns::Column::LastProgressAt),
            Expr::col(chat_turns::Column::StartedAt),
        ]);
        let rows = select
            .secure()
            .scope_with(&AccessScope::allow_all())
            .order_by(progress, sea_orm::Order::Asc)
            .order_by(chat_turns::Column::Id, sea_orm::Order::Asc)
            .limit(limit)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(rows.into_iter().map(to_candidate).collect())
    }
}
