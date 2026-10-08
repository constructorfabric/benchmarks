//! Turn finalization (DESIGN §5.7): CAS guard + quota settlement + usage /
//! audit / thread-summary outbox enqueue, all in one transaction.

use std::time::Instant;

use mini_chat_sdk::audit::{
    AuditLatency, AuditUsage, PolicyDecisions, QuotaDecisionAudit, ToolCalls, TurnAuditEvent, TurnAuditEventType,
};
use mini_chat_sdk::usage::{UsageTokens, requester_type, turn_dedupe_key};
use mini_chat_sdk::{AuditEvent, UsageEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use serde::{Deserialize, Serialize};
use time::Date;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use super::billing::{BillingDecision, Settlement, TurnState, codes, committed_credits, derive};
use super::clock::{self, Timestamp};
use super::credits::credits_micro;
use super::error::DomainError;
use super::quota::{QuotaService, SettleParams};
use super::sanitize::sanitize_provider_message;
use super::service::MiniChat;
use super::stream::TurnRuntime;
use super::stream_types::{Citation, QuotaWarning};
use crate::infra::llm::types::ProviderUsage;
use crate::infra::outbox::{OutboxKind, OutboxSlot, enqueue_json};
use crate::infra::storage::entity::{chat_turn, message, thread_summary};

/// `system_task_type` of thread summary work.
pub const THREAD_SUMMARY_TASK: &str = "thread_summary_update";

/// What ended the provider task.
#[derive(Debug, Clone)]
pub enum TerminalOutcome {
    Completed {
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
        text: String,
        citations: Vec<Citation>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
    },
    Cancelled {
        partial_text: String,
    },
}

/// Per-turn tool counters and latency.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunStats {
    pub web_search: u32,
    pub code_interpreter: u32,
    /// `chat_turns.file_search_completed_count`.
    pub file_search: u32,
    /// `file_search_calls` of the usage/audit events: the knowledge-search
    /// call count on the stream path, the persisted counter on the orphan path.
    pub reported_file_search: u32,
    pub ttft_ms: Option<u64>,
}

/// Result of the stream finalization.
#[derive(Debug, Clone)]
pub enum FinalizeResult {
    /// CAS won and the transaction committed with the intended state.
    Won { quota_warnings: Vec<QuotaWarning> },
    /// Completed stream downgraded to `failed` (`message_persistence_failed`).
    PersistenceFailed,
    /// The finalization transaction failed; the turn stays `running`.
    TxFailed,
    /// Another finalizer won the CAS.
    Lost,
}

/// Thread-summary outbox payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub system_task_type: String,
    #[serde(default)]
    pub base_frontier_created_at: Option<Timestamp>,
    #[serde(default)]
    pub base_frontier_message_id: Option<Uuid>,
    pub frozen_target_created_at: Timestamp,
    pub frozen_target_message_id: Uuid,
}

/// Inputs of the shared settlement + usage/audit emission step.
#[derive(Debug, Clone)]
pub struct SettleInputs {
    pub tenant_id: Uuid,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub state: TurnState,
    pub error_code: Option<String>,
    pub usage: Option<ProviderUsage>,
    /// `(reserve_tokens, max_output_tokens_applied, reserved_credits, floor)`;
    /// `None` when the reserve fields are NULL (settlement is skipped).
    pub reserve: Option<(i64, i64, i64, i64)>,
    pub multipliers: Option<(i64, i64)>,
    pub premium: bool,
    pub daily_start: Date,
    pub monthly_start: Date,
    pub policy_version: u64,
    pub stats: RunStats,
    pub total_ms: u64,
    pub quota_decision: QuotaDecisionAudit,
}

/// Committed credits of a settlement.
#[must_use]
pub fn settlement_credits(
    decision: BillingDecision,
    usage: Option<ProviderUsage>,
    reserve: (i64, i64, i64, i64),
    multipliers: (i64, i64),
    tolerance: f64,
) -> (i64, bool) {
    let (reserve_tokens, mot, reserved_credits, floor) = reserve;
    let (im, om) = multipliers;
    match decision.settlement {
        Settlement::Released => (0, false),
        Settlement::Actual => {
            let u = usage.unwrap_or_default();
            let input = u.input_tokens.clamp(0, super::credits::MAX_TOKENS);
            let output = u.output_tokens.clamp(0, super::credits::MAX_TOKENS);
            let actual = credits_micro(input, output, im, om).unwrap_or(reserved_credits);
            committed_credits(u.total(), reserve_tokens, actual, reserved_credits, tolerance)
        }
        Settlement::Estimated => {
            let est_input = (reserve_tokens - mot).max(0);
            let floor = floor.clamp(0, mot.max(0));
            let c = credits_micro(est_input.min(super::credits::MAX_TOKENS), floor, im, om).unwrap_or(reserved_credits);
            (c.min(reserved_credits.max(0)), false)
        }
    }
}

/// Shared step: quota settlement + usage event + turn audit event. Must run
/// inside the CAS-winning transaction. Returns the wakes to fire after commit.
///
/// # Errors
/// DB / outbox errors (abort the transaction).
#[allow(clippy::cognitive_complexity, reason = "settlement decision table")]
pub async fn settle_and_emit(
    tx: &(impl DBRunner + Sync),
    slot: &OutboxSlot,
    quota: &QuotaService,
    tolerance: f64,
    user_scope: &AccessScope,
    s: &SettleInputs,
) -> Result<(Vec<Wake>, i64, bool), DomainError> {
    let decision = derive(s.state, s.error_code.as_deref(), s.usage.is_some_and(|u| u.is_known()));
    if decision.unknown_error_code {
        tracing::warn!(error_code = ?s.error_code, turn_id = %s.turn_id, "unknown error code; settling estimated");
    }
    let mut committed = 0;
    let mut overshoot = false;
    match (s.reserve, s.user_id) {
        (Some(reserve), Some(user_id)) => {
            (committed, overshoot) = match s.multipliers {
                Some(mults) => settlement_credits(decision, s.usage, reserve, mults, tolerance),
                // Model no longer resolvable: charge the persisted reserve.
                None if decision.settlement == Settlement::Released => (0, false),
                None => (reserve.2, false),
            };
            let actual_tokens = (decision.settlement == Settlement::Actual)
                .then(|| s.usage.map_or((0, 0), |u| (u.input_tokens, u.output_tokens)));
            quota
                .settle(
                    tx,
                    user_scope,
                    &SettleParams {
                        tenant_id: s.tenant_id,
                        user_id,
                        premium: s.premium,
                        daily_start: s.daily_start,
                        monthly_start: s.monthly_start,
                        reserved_credits: reserve.2,
                        committed_credits: if decision.settlement == Settlement::Released { 0 } else { committed },
                        actual_tokens,
                        web_search_calls: i64::from(s.stats.web_search),
                        code_interpreter_calls: i64::from(s.stats.code_interpreter),
                    },
                )
                .await?;
        }
        _ => {
            tracing::warn!(turn_id = %s.turn_id, "turn has no reserve fields; quota settlement skipped");
        }
    }
    let now = clock::now();
    let usage_known = s.usage.is_some_and(|u| u.is_known());
    let usage_payload = match s.state {
        TurnState::Completed => Some(s.usage.unwrap_or_default()),
        TurnState::Failed if usage_known => s.usage,
        _ => None,
    }
    .map(|u| UsageTokens {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_write_input_tokens: u.cache_write_input_tokens,
        reasoning_tokens: u.reasoning_tokens,
    });
    let ev = UsageEvent {
        tenant_id: s.tenant_id,
        user_id: s.user_id,
        chat_id: s.chat_id,
        turn_id: Some(s.turn_id),
        request_id: s.request_id,
        effective_model: s.effective_model.clone(),
        selected_model: s.selected_model.clone(),
        terminal_state: s.state.as_str().to_owned(),
        billing_outcome: decision.billing_outcome.to_owned(),
        usage: usage_payload,
        actual_credits_micro: committed,
        settlement_method: decision.settlement.as_str().to_owned(),
        policy_version_applied: s.policy_version,
        web_search_calls: s.stats.web_search,
        code_interpreter_calls: s.stats.code_interpreter,
        file_search_calls: s.stats.reported_file_search,
        timestamp: clock::to_time(now),
        requester_type: requester_type::USER.to_owned(),
        dedupe_key: turn_dedupe_key(s.tenant_id, s.turn_id, s.request_id),
        system_task_type: None,
    };
    let mut wakes = vec![enqueue_json(slot, tx, OutboxKind::Usage, s.chat_id, &ev).await?];
    let u = s.usage.unwrap_or_default();
    let audit = AuditEvent::Turn(TurnAuditEvent {
        event_type: if s.state == TurnState::Completed {
            TurnAuditEventType::TurnCompleted
        } else {
            TurnAuditEventType::TurnFailed
        },
        tenant_id: s.tenant_id,
        requester_type: requester_type::USER.to_owned(),
        requester_user_id: s.user_id,
        chat_id: s.chat_id,
        turn_id: s.turn_id,
        request_id: s.request_id,
        selected_model: s.selected_model.clone(),
        effective_model: s.effective_model.clone(),
        terminal_state: s.state.as_str().to_owned(),
        error_code: s.error_code.clone(),
        usage: AuditUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_write_input_tokens: u.cache_write_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
        },
        latency: AuditLatency { ttft_ms: s.stats.ttft_ms, total_ms: s.total_ms },
        tool_calls: ToolCalls { web_search_calls: s.stats.web_search, file_search_calls: s.stats.reported_file_search },
        policy_decisions: PolicyDecisions { quota: s.quota_decision.clone(), license: None, quota_scope: None },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        trace_id: current_trace_id(),
        timestamp: clock::to_time(now),
    });
    wakes.push(enqueue_json(slot, tx, OutboxKind::Audit, s.chat_id, &audit).await?);
    Ok((wakes, committed, overshoot))
}

fn current_trace_id() -> Option<String> {
    None
}

/// Errors inside the finalization transaction.
enum TxAbort {
    Lost,
    Persist(DomainError),
    Other(DomainError),
}

impl From<DomainError> for TxAbort {
    fn from(e: DomainError) -> Self {
        Self::Other(e)
    }
}

impl From<toolkit_db::DbError> for TxAbort {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Other(e.into())
    }
}

impl From<TxAbort> for DomainError {
    fn from(a: TxAbort) -> Self {
        match a {
            TxAbort::Lost => DomainError::Internal("cas lost".to_owned()),
            TxAbort::Persist(e) | TxAbort::Other(e) => e,
        }
    }
}

/// Outcome of the thread-summary trigger evaluation inside the finalization tx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryTrigger {
    NotEvaluated,
    Scheduled,
    NotNeeded,
}

impl MiniChat {
    #[allow(clippy::similar_names, reason = "`state` and `stats` are distinct domain terms")]
    fn turn_settle_inputs(rt: &TurnRuntime, state: TurnState, error_code: Option<String>, usage: Option<ProviderUsage>, stats: RunStats) -> SettleInputs {
        let d = &rt.decision;
        SettleInputs {
            tenant_id: rt.tenant_id,
            user_id: Some(rt.user_id),
            chat_id: rt.chat_id,
            turn_id: rt.turn_id,
            request_id: rt.request_id,
            selected_model: d.selected_model.clone(),
            effective_model: d.effective.id.clone(),
            state,
            error_code,
            usage,
            reserve: Some((d.reserve.reserve_tokens, d.reserve.max_output_tokens_applied, d.reserved_credits_micro, d.floor_applied)),
            multipliers: Some((
                d.effective.input_tokens_credit_multiplier_micro,
                d.effective.output_tokens_credit_multiplier_micro,
            )),
            premium: d.is_premium(),
            daily_start: d.daily_start,
            monthly_start: d.monthly_start,
            policy_version: d.policy_version,
            stats,
            total_ms: u64::try_from(rt.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            quota_decision: QuotaDecisionAudit {
                decision: d.quota_decision().to_owned(),
                downgrade_from: (d.quota_decision() == "downgrade").then(|| d.selected_model.clone()),
                downgrade_reason: d.downgrade_reason.map(str::to_owned),
            },
        }
    }

    /// Stream finalization (exactly once per turn through the CAS guard).
    #[allow(clippy::cognitive_complexity, reason = "maps every terminal outcome shape")]
    pub async fn finalize(&self, rt: &TurnRuntime, outcome: TerminalOutcome, stats: RunStats) -> FinalizeResult {
        let started = Instant::now();
        let res = match outcome {
            TerminalOutcome::Completed { usage, response_id, text, .. } => {
                match self.finalize_tx(rt, TurnState::Completed, None, None, usage, response_id.clone(), Some(text), stats).await {
                    Ok(trigger) => {
                        self.record_trigger(trigger);
                        FinalizeResult::Won { quota_warnings: self.quota_warnings(rt).await }
                    }
                    Err(TxAbort::Lost) => FinalizeResult::Lost,
                    Err(TxAbort::Persist(e)) => {
                        tracing::error!(error = %e, turn_id = %rt.turn_id, "assistant message persistence failed");
                        match self
                            .finalize_tx(
                                rt,
                                TurnState::Failed,
                                Some(codes::MESSAGE_PERSISTENCE_FAILED.to_owned()),
                                Some("assistant message could not be persisted".to_owned()),
                                usage,
                                response_id,
                                None,
                                stats,
                            )
                            .await
                        {
                            Ok(_) => FinalizeResult::PersistenceFailed,
                            Err(TxAbort::Lost) => FinalizeResult::Lost,
                            Err(_) => FinalizeResult::TxFailed,
                        }
                    }
                    Err(TxAbort::Other(e)) => {
                        tracing::error!(error = %e, turn_id = %rt.turn_id, "turn finalization failed");
                        FinalizeResult::TxFailed
                    }
                }
            }
            TerminalOutcome::Failed { code, message, usage, response_id } => {
                let detail = sanitize_provider_message(&message);
                match self.finalize_tx(rt, TurnState::Failed, Some(code), Some(detail), usage, response_id, None, stats).await {
                    Ok(_) => FinalizeResult::Won { quota_warnings: Vec::new() },
                    Err(TxAbort::Lost) => FinalizeResult::Lost,
                    Err(TxAbort::Persist(e) | TxAbort::Other(e)) => {
                        tracing::error!(error = %e, turn_id = %rt.turn_id, "turn finalization failed");
                        FinalizeResult::TxFailed
                    }
                }
            }
            TerminalOutcome::Cancelled { partial_text } => {
                let text = (!partial_text.is_empty()).then_some(partial_text);
                let has_text = text.is_some();
                let first = self.finalize_tx(rt, TurnState::Cancelled, None, None, None, None, text, stats).await;
                match first {
                    Ok(_) => FinalizeResult::Won { quota_warnings: Vec::new() },
                    Err(TxAbort::Lost) => FinalizeResult::Lost,
                    Err(TxAbort::Persist(e)) if has_text => {
                        tracing::warn!(error = %e, turn_id = %rt.turn_id, "partial assistant message not persisted");
                        match self.finalize_tx(rt, TurnState::Cancelled, None, None, None, None, None, stats).await {
                            Ok(_) => FinalizeResult::Won { quota_warnings: Vec::new() },
                            Err(TxAbort::Lost) => FinalizeResult::Lost,
                            Err(_) => FinalizeResult::TxFailed,
                        }
                    }
                    Err(TxAbort::Persist(e) | TxAbort::Other(e)) => {
                        tracing::error!(error = %e, turn_id = %rt.turn_id, "cancel finalization failed");
                        FinalizeResult::TxFailed
                    }
                }
            }
        };
        self.metrics.finalization_latency(started.elapsed().as_secs_f64() * 1000.0);
        res
    }

    fn record_trigger(&self, t: SummaryTrigger) {
        match t {
            SummaryTrigger::NotEvaluated => {}
            SummaryTrigger::Scheduled => self.metrics.thread_summary_trigger("scheduled"),
            SummaryTrigger::NotNeeded => self.metrics.thread_summary_trigger("not_needed"),
        }
    }

    /// `quota_warnings` for the `done` event (empty on failure).
    async fn quota_warnings(&self, rt: &TurnRuntime) -> Vec<QuotaWarning> {
        let Ok(conn) = self.db.conn() else {
            return Vec::new();
        };
        match self
            .quota()
            .status(&conn, &rt.chat_scope, rt.tenant_id, rt.user_id, &rt.decision.limits)
            .await
        {
            Ok(list) => list
                .into_iter()
                .map(|p| QuotaWarning {
                    tier: p.tier,
                    period: p.period.as_str(),
                    remaining_percentage: p.remaining_percentage,
                    warning: p.warning,
                    exhausted: p.exhausted,
                    next_reset: (p.warning || p.exhausted).then(|| clock::format_rfc3339(p.next_reset)),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "quota warnings unavailable");
                Vec::new()
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::similar_names,
        reason = "terminal shape; `state` and `stats` are distinct domain terms"
    )]
    async fn finalize_tx(
        &self,
        rt: &TurnRuntime,
        state: TurnState,
        error_code: Option<String>,
        error_detail: Option<String>,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
        assistant_text: Option<String>,
        stats: RunStats,
    ) -> Result<SummaryTrigger, TxAbort> {
        let mut delay = std::time::Duration::from_millis(5);
        let mut attempt = 1;
        loop {
            let r = self
                .finalize_tx_once(
                    rt,
                    state,
                    error_code.clone(),
                    error_detail.clone(),
                    usage,
                    response_id.clone(),
                    assistant_text.clone(),
                    stats,
                )
                .await;
            match r {
                Err(TxAbort::Other(e) | TxAbort::Persist(e)) if e.is_contention() && attempt < 10 => {
                    tracing::debug!(error = %e, attempt, "finalization contention; retrying");
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, std::time::Duration::from_millis(250));
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::similar_names,
        clippy::integer_division,
        reason = "one transaction for all terminal shapes; `state`/`stats` are distinct terms; deliberate floor percentage"
    )]
    async fn finalize_tx_once(
        &self,
        rt: &TurnRuntime,
        state: TurnState,
        error_code: Option<String>,
        error_detail: Option<String>,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
        assistant_text: Option<String>,
        stats: RunStats,
    ) -> Result<SummaryTrigger, TxAbort> {
        let settle = Self::turn_settle_inputs(rt, state, error_code.clone(), usage, stats);
        let slot = self.outbox.clone();
        let quota = self.quota();
        let tolerance = self.cfg.quota.overshoot_tolerance_factor;
        let child = rt.child_scope.clone();
        let user_scope = rt.chat_scope.clone();
        let turn_id = rt.turn_id;
        let chat_id = rt.chat_id;
        let tenant_id = rt.tenant_id;
        let request_id = rt.request_id;
        let msg_id = rt.assistant_message_id;
        let effective_model = rt.decision.effective.id.clone();
        let summary_check = (state == TurnState::Completed && self.cfg.thread_summary_worker.enabled).then(|| {
            let threshold = rt.effective_budget.saturating_mul(i64::from(self.cfg.thread_summary_worker.compression_threshold_pct)) / 100;
            rt.messages_truncated || (!rt.summary_exists && rt.assembled_tokens >= threshold)
        });
        let metrics = self.metrics.clone();
        let result = self
            .raw_db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let set_message = assistant_text.is_some();
                    let mut upd = chat_turn::Entity::update_many()
                        .secure()
                        .col_expr(chat_turn::Column::State, Expr::value(state.as_str()))
                        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
                        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                        .col_expr(chat_turn::Column::ProviderResponseId, Expr::value(response_id.clone()))
                        .col_expr(chat_turn::Column::ErrorCode, Expr::value(error_code.clone()))
                        .col_expr(chat_turn::Column::ErrorDetail, Expr::value(error_detail.clone()))
                        .col_expr(chat_turn::Column::WebSearchCompletedCount, Expr::value(i32::try_from(stats.web_search).unwrap_or(i32::MAX)))
                        .col_expr(
                            chat_turn::Column::CodeInterpreterCompletedCount,
                            Expr::value(i32::try_from(stats.code_interpreter).unwrap_or(i32::MAX)),
                        )
                        .col_expr(chat_turn::Column::FileSearchCompletedCount, Expr::value(i32::try_from(stats.file_search).unwrap_or(i32::MAX)));
                    if set_message {
                        upd = upd.col_expr(chat_turn::Column::AssistantMessageId, Expr::value(Some(msg_id)));
                    }
                    let rows = upd
                        .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)).add(chat_turn::Column::State.eq("running")))
                        .scope_with(&child)
                        .exec(tx)
                        .await
                        .map_err(|e| TxAbort::Other(e.into()))?
                        .rows_affected;
                    if rows == 0 {
                        return Err(TxAbort::Lost);
                    }
                    if let Some(text) = assistant_text {
                        let u = if state == TurnState::Completed { usage.unwrap_or_default() } else { ProviderUsage::default() };
                        let m = message::ActiveModel {
                            id: Set(msg_id),
                            tenant_id: Set(tenant_id),
                            chat_id: Set(chat_id),
                            request_id: Set(Some(request_id)),
                            role: Set("assistant".to_owned()),
                            content: Set(text),
                            content_type: Set("text".to_owned()),
                            token_estimate: Set(0),
                            provider_response_id: Set(response_id.clone()),
                            request_kind: Set("chat".to_owned()),
                            features_used: Set(serde_json::json!([])),
                            input_tokens: Set(u.input_tokens),
                            output_tokens: Set(u.output_tokens),
                            cache_read_input_tokens: Set(u.cache_read_input_tokens),
                            cache_write_input_tokens: Set(u.cache_write_input_tokens),
                            reasoning_tokens: Set(u.reasoning_tokens),
                            model: Set(Some(effective_model.clone())),
                            is_compressed: Set(false),
                            created_at: Set(now),
                            deleted_at: Set(None),
                        };
                        secure_insert::<message::Entity>(m, &child, tx)
                            .await
                            .map_err(|e| TxAbort::Persist(e.into()))?;
                    }
                    let (mut wakes, _, overshoot) =
                        settle_and_emit(tx, &slot, &quota, tolerance, &user_scope, &settle).await?;
                    metrics.quota_commit(overshoot);
                    if let Some(u) = settle.usage {
                        metrics.quota_actual_tokens(u.total());
                    }
                    let mut trigger = SummaryTrigger::NotEvaluated;
                    if let Some(fire) = summary_check {
                        trigger = SummaryTrigger::NotNeeded;
                        if fire
                            && let Some(w) = schedule_summary(tx, &slot, &child, tenant_id, chat_id, request_id).await?
                        {
                            wakes.push(w);
                            trigger = SummaryTrigger::Scheduled;
                        }
                    }
                    Ok((wakes, trigger))
                })
            })
            .await;
        match result {
            Ok((wakes, trigger)) => {
                for w in wakes {
                    w.fire();
                }
                Ok(trigger)
            }
            Err(e) => Err(e),
        }
    }
}

/// Enqueue thread-summary work for the chat (frozen target = latest
/// non-deleted message outside the causing request). Returns `None` when
/// nothing needs to be summarized.
///
/// # Errors
/// DB / outbox errors.
pub async fn schedule_summary(
    tx: &(impl DBRunner + Sync),
    slot: &OutboxSlot,
    child: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    causing_request: Uuid,
) -> Result<Option<Wake>, DomainError> {
    let target = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::DeletedAt.is_null())
        .filter(
            Condition::any()
                .add(message::Column::RequestId.is_null())
                .add(message::Column::RequestId.ne(causing_request)),
        )
        .order_by_desc(message::Column::CreatedAt)
        .order_by_desc(message::Column::Id)
        .limit(1)
        .secure()
        .scope_with(child)
        .one(tx)
        .await?;
    let Some(target) = target else {
        return Ok(None);
    };
    let summary = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(child)
        .one(tx)
        .await?;
    if let Some(s) = &summary {
        let base = (s.summarized_up_to_created_at, s.summarized_up_to_message_id);
        if (target.created_at, target.id) <= base {
            return Ok(None);
        }
    }
    let payload = ThreadSummaryPayload {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        system_task_type: THREAD_SUMMARY_TASK.to_owned(),
        base_frontier_created_at: summary.as_ref().map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
    };
    Ok(Some(enqueue_json(slot, tx, OutboxKind::ThreadSummary, chat_id, &payload).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::billing::derive;

    #[test]
    fn estimated_settlement_uses_floor() {
        let d = derive(TurnState::Cancelled, None, false);
        // reserve 1100 = 1000 input + 100 mot; floor 50; 1:1 multipliers in micro (1e6 = 1 credit/token)
        let (c, cap) = settlement_credits(d, None, (1100, 100, 1_100, 50), (1_000_000, 1_000_000), 1.1);
        assert_eq!((c, cap), (1_050, false));
    }

    #[test]
    fn actual_settlement_caps_overshoot() {
        let d = derive(TurnState::Completed, None, true);
        let usage = ProviderUsage { input_tokens: 1000, output_tokens: 300, ..Default::default() };
        let (c, cap) = settlement_credits(d, Some(usage), (1100, 100, 1_100, 50), (1_000_000, 1_000_000), 1.1);
        assert_eq!((c, cap), (1_100, true));
        let usage = ProviderUsage { input_tokens: 100, output_tokens: 20, ..Default::default() };
        let (c, cap) = settlement_credits(d, Some(usage), (1100, 100, 1_100, 50), (1_000_000, 1_000_000), 1.1);
        assert_eq!((c, cap), (120, false));
    }

    #[test]
    fn released_is_zero() {
        let d = derive(TurnState::Failed, Some("turn_setup_failed"), false);
        assert_eq!(settlement_credits(d, None, (1, 1, 5, 1), (1, 1), 1.1), (0, false));
    }

    #[test]
    fn payload_roundtrip_keeps_nanos() {
        let t = clock::now();
        let p = ThreadSummaryPayload {
            tenant_id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            system_request_id: Uuid::new_v4(),
            system_task_type: THREAD_SUMMARY_TASK.into(),
            base_frontier_created_at: None,
            base_frontier_message_id: None,
            frozen_target_created_at: t,
            frozen_target_message_id: Uuid::new_v4(),
        };
        let back: ThreadSummaryPayload = serde_json::from_slice(&serde_json::to_vec(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
