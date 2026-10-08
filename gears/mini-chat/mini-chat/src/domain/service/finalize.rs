//! Turn finalization contract (DESIGN §5.7): CAS on `chat_turns.state`, quota
//! settlement and outbox emission in one transaction.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mini_chat_sdk::{
    AuditEnvelope, AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, TurnAuditEvent,
    UsageEvent, UsageTokens, turn_dedupe_key,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use toolkit_db::DbError;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::quota::{self, PreflightDecision, Settlement, SettlementMethod, TurnReserve};
use super::stream::{NewMessage, insert_message};
use super::{AppServices, now};
use crate::domain::error::DomainError;
use crate::domain::estimate::PeriodType;
use crate::infra::db::entity::{chat_turns, messages, thread_summaries};
use crate::infra::outbox::{OutboxEnqueuer, Wakes};

/// Terminal outcome of a stream.
#[derive(Debug, Clone)]
pub enum Terminal {
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
    Cancelled,
}

impl Terminal {
    #[must_use]
    pub fn failed(code: &str, message: String, usage: Option<UsageTokens>) -> Self {
        Self::Failed {
            code: code.to_owned(),
            message,
            usage,
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Failed { message, .. } => message.clone(),
            _ => String::new(),
        }
    }
}

/// Finalization input.
pub struct FinalizeInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub selected_model: String,
    pub decision: PreflightDecision,
    pub terminal: Terminal,
    pub text: String,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    pub summary_trigger: bool,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Outcome of finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Completed,
    Failed,
    MessagePersistenceFailed,
    FinalizationFailed,
    Lost,
}

/// Thread-summary outbox payload (DESIGN §3.6 "Durable scheduling").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub base_frontier_created_at: Option<DateTime<Utc>>,
    pub base_frontier_message_id: Option<Uuid>,
    pub frozen_target_created_at: DateTime<Utc>,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

#[derive(Debug)]
enum TxError {
    Message(DomainError),
    Other(DomainError),
}

impl From<DbError> for TxError {
    fn from(e: DbError) -> Self {
        Self::Other(DomainError::from(e))
    }
}

impl From<DomainError> for TxError {
    fn from(e: DomainError) -> Self {
        Self::Other(e)
    }
}

/// Usage-event fields for a settlement.
#[must_use]
pub fn usage_for_event(method: SettlementMethod, usage: Option<&UsageTokens>) -> Option<UsageTokens> {
    match method {
        SettlementMethod::Actual => usage.copied(),
        SettlementMethod::Estimated => None,
        SettlementMethod::Released => Some(UsageTokens::default()),
    }
}

/// Builds the turn usage event.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn turn_usage_event(
    tenant_id: Uuid,
    user_id: Option<Uuid>,
    chat_id: Uuid,
    turn_id: Uuid,
    request_id: Uuid,
    effective_model: &str,
    selected_model: &str,
    terminal_state: &str,
    billing_outcome: &str,
    s: &Settlement,
    usage: Option<UsageTokens>,
    policy_version: u64,
    calls: (u32, u32, u32),
) -> UsageEvent {
    UsageEvent {
        tenant_id,
        user_id,
        chat_id,
        turn_id: Some(turn_id),
        request_id,
        effective_model: effective_model.to_owned(),
        selected_model: selected_model.to_owned(),
        terminal_state: terminal_state.to_owned(),
        billing_outcome: billing_outcome.to_owned(),
        usage,
        actual_credits_micro: s.committed_credits,
        settlement_method: s.method.as_str().to_owned(),
        policy_version_applied: policy_version,
        web_search_calls: calls.0,
        code_interpreter_calls: calls.1,
        file_search_calls: calls.2,
        timestamp: time::OffsetDateTime::now_utc(),
        requester_type: "user".to_owned(),
        dedupe_key: turn_dedupe_key(tenant_id, turn_id, request_id),
        system_task_type: None,
    }
}

/// Enqueues a thread-summary task when the frozen target is beyond the frontier.
///
/// # Errors
/// Database / outbox failure.
pub async fn schedule_summary(
    tx: &toolkit_db::DbTx<'_>,
    outbox: &OutboxEnqueuer,
    tenant_id: Uuid,
    chat_id: Uuid,
    exclude_request: Uuid,
) -> Result<Option<toolkit_db::outbox::Wake>, DomainError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let target = messages::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(messages::Column::RequestId.ne(exclude_request)),
        )
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(messages::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .all(tx)
        .await?;
    let Some(target) = target.into_iter().next() else {
        return Ok(None);
    };
    let base = thread_summaries::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .one(tx)
        .await?;
    if let Some(b) = &base
        && (b.summarized_up_to_created_at, b.summarized_up_to_message_id) >= (target.created_at, target.id)
    {
        return Ok(None);
    }
    let payload = ThreadSummaryPayload {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.as_ref().map(|b| b.summarized_up_to_created_at),
        base_frontier_message_id: base.as_ref().map(|b| b.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".to_owned(),
    };
    Ok(Some(outbox.thread_summary(tx, chat_id, &payload).await?))
}

impl AppServices {
    /// Finalizes a turn (stream terminal paths).
    pub async fn finalize_turn(&self, input: FinalizeInput) -> FinalizeOutcome {
        let persist = match &input.terminal {
            Terminal::Completed { .. } => true,
            Terminal::Cancelled => !input.text.is_empty(),
            Terminal::Failed { .. } => false,
        };
        let input = Arc::new(input);
        match self.finalize_tx(Arc::clone(&input), persist, None).await {
            Ok(true) => match input.terminal {
                Terminal::Completed { .. } => FinalizeOutcome::Completed,
                _ => FinalizeOutcome::Failed,
            },
            Ok(false) => FinalizeOutcome::Lost,
            Err(TxError::Message(e)) => {
                tracing::warn!(error = %e, request_id = %input.request_id, "assistant message persistence failed");
                if matches!(input.terminal, Terminal::Cancelled) {
                    return match self.finalize_tx(Arc::clone(&input), false, None).await {
                        Ok(true) => FinalizeOutcome::Failed,
                        Ok(false) => FinalizeOutcome::Lost,
                        Err(_) => FinalizeOutcome::FinalizationFailed,
                    };
                }
                match self
                    .finalize_tx(Arc::clone(&input), false, Some("message_persistence_failed"))
                    .await
                {
                    Ok(true) => FinalizeOutcome::MessagePersistenceFailed,
                    Ok(false) => FinalizeOutcome::Lost,
                    Err(_) => FinalizeOutcome::FinalizationFailed,
                }
            }
            Err(TxError::Other(e)) => {
                tracing::warn!(error = %e, request_id = %input.request_id, "turn finalization failed");
                FinalizeOutcome::FinalizationFailed
            }
        }
    }

    /// One finalization attempt; `Ok(false)` = CAS lost.
    async fn finalize_tx(
        &self,
        input: Arc<FinalizeInput>,
        persist: bool,
        override_failed: Option<&'static str>,
    ) -> Result<bool, TxError> {
        let outbox = Arc::clone(&self.outbox);
        let tolerance = self.cfg.quota.overshoot_tolerance_factor;
        let summary_enabled = self.cfg.thread_summary_worker.enabled;
        let res = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    let i = input.as_ref();
                    let ts = now();
                    let (state, error_code, usage, response_id) = match (&i.terminal, override_failed) {
                        (_, Some(code)) => {
                            let u = match &i.terminal {
                                Terminal::Completed { usage, .. } => *usage,
                                _ => None,
                            };
                            ("failed", Some(code.to_owned()), u, None)
                        }
                        (Terminal::Completed { usage, response_id }, None) => {
                            ("completed", None, *usage, response_id.clone())
                        }
                        (Terminal::Failed { code, usage, .. }, None) => ("failed", Some(code.clone()), *usage, None),
                        (Terminal::Cancelled, None) => ("cancelled", None, None, None),
                    };
                    let scope = AccessScope::for_tenant(i.tenant_id);
                    let rows = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::State, Expr::value(state))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value(error_code.clone()))
                        .col_expr(
                            chat_turns::Column::ErrorDetail,
                            Expr::value(match &i.terminal {
                                Terminal::Failed { message, .. } => Some(message.clone()),
                                _ => None,
                            }),
                        )
                        .col_expr(chat_turns::Column::ProviderResponseId, Expr::value(response_id.clone()))
                        .col_expr(
                            chat_turns::Column::AssistantMessageId,
                            Expr::value(persist.then_some(i.message_id)),
                        )
                        .col_expr(
                            chat_turns::Column::WebSearchCompletedCount,
                            Expr::value(i32::try_from(i.web_search_calls).unwrap_or(i32::MAX)),
                        )
                        .col_expr(
                            chat_turns::Column::CodeInterpreterCompletedCount,
                            Expr::value(i32::try_from(i.code_interpreter_calls).unwrap_or(i32::MAX)),
                        )
                        .col_expr(
                            chat_turns::Column::FileSearchCompletedCount,
                            Expr::value(i32::try_from(i.file_search_calls).unwrap_or(i32::MAX)),
                        )
                        .filter(
                            Condition::all()
                                .add(chat_turns::Column::Id.eq(i.turn_id))
                                .add(chat_turns::Column::State.eq("running")),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await
                        .map_err(DomainError::from)?
                        .rows_affected;
                    if rows == 0 {
                        return Ok((false, Wakes::default()));
                    }
                    if persist {
                        let msg_usage = if state == "completed" { usage } else { None };
                        insert_message(tx, NewMessage {
                            id: i.message_id,
                            tenant_id: i.tenant_id,
                            chat_id: i.chat_id,
                            request_id: i.request_id,
                            role: "assistant",
                            content: i.text.clone(),
                            model: Some(i.decision.effective.id.clone()),
                            usage: msg_usage,
                            provider_response_id: response_id.clone(),
                            created_at: ts,
                        })
                        .await
                        .map_err(TxError::Message)?;
                    }
                    let (billing, method) = quota::derive_billing(state, error_code.as_deref(), usage.as_ref());
                    let d = &i.decision;
                    let reserve = TurnReserve {
                        reserve_tokens: d.reserve_tokens,
                        max_output_tokens_applied: i64::from(d.max_output_tokens_applied),
                        reserved_credits_micro: d.reserved_credits_micro,
                        floor_applied: i64::from(d.minimal_generation_floor_applied),
                        in_mult: d.effective.input_tokens_credit_multiplier_micro,
                        out_mult: d.effective.output_tokens_credit_multiplier_micro,
                    };
                    let s = quota::compute_settlement(method, usage.as_ref(), &reserve, tolerance)?;
                    quota::settle(
                        tx,
                        i.tenant_id,
                        i.user_id,
                        &d.periods,
                        d.is_premium(),
                        d.reserved_credits_micro,
                        &s,
                        i32::try_from(i.web_search_calls).unwrap_or(i32::MAX),
                        i32::try_from(i.code_interpreter_calls).unwrap_or(i32::MAX),
                    )
                    .await?;
                    let mut wakes = Wakes::default();
                    let event = turn_usage_event(
                        i.tenant_id,
                        Some(i.user_id),
                        i.chat_id,
                        i.turn_id,
                        i.request_id,
                        &d.effective.id,
                        &i.selected_model,
                        state,
                        billing,
                        &s,
                        usage_for_event(method, usage.as_ref()),
                        d.policy_version,
                        (i.web_search_calls, i.code_interpreter_calls, i.file_search_calls),
                    );
                    wakes.push(outbox.usage(tx, i.tenant_id, &event).await?);
                    let downgrade = d.quota_decision() == "downgrade";
                    let audit = TurnAuditEvent {
                        event_type: if state == "completed" { "turn_completed" } else { "turn_failed" }.to_owned(),
                        timestamp: time::OffsetDateTime::now_utc(),
                        tenant_id: i.tenant_id,
                        requester_type: "user".to_owned(),
                        actor_user_id: Some(i.user_id),
                        chat_id: i.chat_id,
                        turn_id: i.turn_id,
                        request_id: i.request_id,
                        selected_model: i.selected_model.clone(),
                        effective_model: d.effective.id.clone(),
                        terminal_state: state.to_owned(),
                        error_code: error_code.clone(),
                        usage,
                        latency: AuditLatency {
                            ttft_ms: i.ttft_ms,
                            total_ms: Some(i.total_ms),
                        },
                        tool_calls: AuditToolCalls {
                            web_search_calls: i.web_search_calls,
                            file_search_calls: i.file_search_calls,
                        },
                        policy_decisions: AuditPolicyDecisions {
                            quota: AuditQuotaDecision {
                                decision: d.quota_decision().to_owned(),
                                downgrade_from: downgrade.then(|| i.selected_model.clone()),
                                downgrade_reason: if downgrade {
                                    d.downgrade_reason.map(str::to_owned)
                                } else {
                                    None
                                },
                            },
                            license: None,
                        },
                        prompt: String::new(),
                        response: String::new(),
                        attachments: Vec::new(),
                        quota_scope: None,
                        trace_id: None,
                    };
                    wakes.push(outbox.audit(tx, i.tenant_id, &AuditEnvelope::Turn(audit)).await?);
                    if state == "completed" && summary_enabled && i.summary_trigger {
                        match schedule_summary(tx, &outbox, i.tenant_id, i.chat_id, i.request_id).await? {
                            Some(w) => {
                                wakes.push(w);
                                tracing::debug!(chat_id = %i.chat_id, "thread summary scheduled");
                            }
                            None => tracing::debug!(chat_id = %i.chat_id, "thread summary not needed"),
                        }
                    }
                    Ok::<_, TxError>((true, wakes))
                })
            })
            .await?;
        let (won, wakes) = res;
        wakes.fire();
        Ok(won)
    }
}

/// Orphan-watchdog finalization (DESIGN §4 "Orphan Turn Watchdog").
///
/// # Errors
/// Database / outbox failure.
pub async fn finalize_orphan(
    svc: &AppServices,
    turn: chat_turns::Model,
    cutoff: DateTime<Utc>,
    multipliers: Option<(i64, i64, bool)>,
) -> Result<bool, DomainError> {
    let outbox = Arc::clone(&svc.outbox);
    let res = svc
        .db
        .transaction_ref_mapped(move |tx| {
            Box::pin(async move {
                crate::domain::service::lock_for_write(tx).await?;
                let ts = now();
                let scope = AccessScope::for_tenant(turn.tenant_id);
                let rows = chat_turns::Entity::update_many()
                    .col_expr(chat_turns::Column::State, Expr::value("failed"))
                    .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some("orphan_timeout")))
                    .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
                    .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                    .filter(
                        Condition::all()
                            .add(chat_turns::Column::Id.eq(turn.id))
                            .add(chat_turns::Column::State.eq("running"))
                            .add(chat_turns::Column::DeletedAt.is_null())
                            .add(
                                Condition::any()
                                    .add(chat_turns::Column::LastProgressAt.lte(cutoff))
                                    .add(
                                        Condition::all()
                                            .add(chat_turns::Column::LastProgressAt.is_null())
                                            .add(chat_turns::Column::StartedAt.lte(cutoff)),
                                    ),
                            ),
                    )
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await?
                    .rows_affected;
                if rows == 0 {
                    return Ok::<_, DomainError>((false, Wakes::default()));
                }
                let effective = turn.effective_model.clone().unwrap_or_default();
                let mut wakes = Wakes::default();
                let reserve_known = turn.reserve_tokens.is_some()
                    && turn.max_output_tokens_applied.is_some()
                    && turn.reserved_credits_micro.is_some()
                    && turn.minimal_generation_floor_applied.is_some()
                    && turn.requester_user_id.is_some();
                let (s, policy_version, eff) = match (reserve_known, multipliers) {
                    (true, Some((in_mult, out_mult, premium))) => {
                        let r = TurnReserve {
                            reserve_tokens: turn.reserve_tokens.unwrap_or(0),
                            max_output_tokens_applied: i64::from(turn.max_output_tokens_applied.unwrap_or(0)),
                            reserved_credits_micro: turn.reserved_credits_micro.unwrap_or(0),
                            floor_applied: i64::from(turn.minimal_generation_floor_applied.unwrap_or(0)),
                            in_mult,
                            out_mult,
                        };
                        let s = quota::compute_settlement(SettlementMethod::Estimated, None, &r, 1.0)?;
                        let started = turn.started_at;
                        let periods: Vec<(PeriodType, chrono::NaiveDate)> =
                            PeriodType::ALL.iter().map(|p| (*p, p.start_of(started))).collect();
                        quota::settle(
                            tx,
                            turn.tenant_id,
                            turn.requester_user_id.unwrap_or_default(),
                            &periods,
                            premium,
                            r.reserved_credits_micro,
                            &s,
                            turn.web_search_completed_count,
                            turn.code_interpreter_completed_count,
                        )
                        .await?;
                        (
                            s,
                            u64::try_from(turn.policy_version_applied.unwrap_or(0)).unwrap_or(0),
                            effective.clone(),
                        )
                    }
                    _ => {
                        tracing::warn!(turn_id = %turn.id, "orphan turn without reserve fields: settlement skipped");
                        (
                            Settlement {
                                method: SettlementMethod::Estimated,
                                committed_credits: 0,
                                telemetry_input: 0,
                                telemetry_output: 0,
                                count_tools: false,
                                overshoot: false,
                            },
                            0,
                            String::new(),
                        )
                    }
                };
                let event = turn_usage_event(
                    turn.tenant_id,
                    turn.requester_user_id,
                    turn.chat_id,
                    turn.id,
                    turn.request_id,
                    &eff,
                    &eff,
                    "failed",
                    "aborted",
                    &s,
                    None,
                    policy_version,
                    (
                        u32::try_from(turn.web_search_completed_count).unwrap_or(0),
                        u32::try_from(turn.code_interpreter_completed_count).unwrap_or(0),
                        u32::try_from(turn.file_search_completed_count).unwrap_or(0),
                    ),
                );
                wakes.push(outbox.usage(tx, turn.tenant_id, &event).await?);
                let audit = TurnAuditEvent {
                    event_type: "turn_failed".to_owned(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    tenant_id: turn.tenant_id,
                    requester_type: turn.requester_type.clone(),
                    actor_user_id: turn.requester_user_id,
                    chat_id: turn.chat_id,
                    turn_id: turn.id,
                    request_id: turn.request_id,
                    selected_model: effective.clone(),
                    effective_model: effective.clone(),
                    terminal_state: "failed".to_owned(),
                    error_code: Some("orphan_timeout".to_owned()),
                    usage: None,
                    latency: AuditLatency::default(),
                    tool_calls: AuditToolCalls {
                        web_search_calls: u32::try_from(turn.web_search_completed_count).unwrap_or(0),
                        file_search_calls: u32::try_from(turn.file_search_completed_count).unwrap_or(0),
                    },
                    policy_decisions: AuditPolicyDecisions {
                        quota: AuditQuotaDecision {
                            decision: "unknown".to_owned(),
                            downgrade_from: None,
                            downgrade_reason: None,
                        },
                        license: None,
                    },
                    prompt: String::new(),
                    response: String::new(),
                    attachments: Vec::new(),
                    quota_scope: None,
                    trace_id: None,
                };
                wakes.push(outbox.audit(tx, turn.tenant_id, &AuditEnvelope::Turn(audit)).await?);
                Ok((true, wakes))
            })
        })
        .await?;
    let (won, wakes) = res;
    wakes.fire();
    Ok(won)
}
