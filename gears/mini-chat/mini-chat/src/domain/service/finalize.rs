//! Turn finalization (DESIGN §5.7): one transaction with the CAS on the turn
//! state, the assistant message, the quota settlement and the usage / audit
//! (and thread-summary) outbox events. The terminal SSE event is produced
//! only after the commit.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::sync::Arc;

use mini_chat_sdk::{
    AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage,
    TurnAuditEvent, TurnAuditEventType, UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_db::DbTx;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::Service;
use super::context::OrderKey;
use super::quota::Periods;
use super::stream::{
    DoneInfo, NewMessage, QuotaWarning, StreamEvent, ToolCounts, TurnRun, insert_message,
};
use crate::domain::billing::{self, BillingOutcome, PersistedReserve, SettlementMethod};
use crate::domain::clock;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::events::{
    PAYLOAD_THREAD_SUMMARY, PAYLOAD_TURN_AUDIT, PAYLOAD_USAGE, THREAD_SUMMARY_TASK,
    ThreadSummaryTask,
};
use crate::infra::llm::types::Usage;
use crate::infra::outbox::{OutboxEnqueuer, fire};
use crate::infra::storage::entity::{chat_turn, message, thread_summary};

/// Terminal outcome of a provider task.
#[derive(Debug, Clone)]
pub enum FinalOutcome {
    Completed {
        usage: Option<Usage>,
        response_id: Option<String>,
        incomplete: bool,
    },
    Failed {
        code: String,
        /// Sanitized client message.
        message: String,
        /// Internal diagnostic (stored in `error_detail`).
        detail: Option<String>,
        usage: Option<Usage>,
        response_id: Option<String>,
    },
    Cancelled,
}

/// Accumulated stream state.
#[derive(Debug, Clone, Default)]
pub struct TurnFinal {
    pub text: String,
    pub counts: ToolCounts,
    pub ttft_ms: Option<u64>,
}

/// What a finalization transaction writes.
#[derive(Debug, Clone)]
struct Terminal {
    state: &'static str,
    error_code: Option<String>,
    error_detail: Option<String>,
    usage: Option<Usage>,
    response_id: Option<String>,
    persist_message: bool,
}

/// Inputs of the shared settlement + outbox step (stream and orphan paths).
pub(crate) struct SettlementInput<'a> {
    pub tenant_id: Uuid,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: &'a str,
    pub effective_model: &'a str,
    pub premium: bool,
    pub state: &'a str,
    pub error_code: Option<&'a str>,
    pub usage: Option<Usage>,
    pub reserve: Option<PersistedReserve>,
    pub multipliers: (i64, i64),
    pub policy_version: u64,
    pub periods: Periods,
    pub counts: (u32, u32, u32),
    pub quota_decision: AuditQuotaDecision,
    pub latency: AuditLatency,
    pub tolerance: f64,
}

/// Settle the reserve and enqueue the usage and audit events (shared by the
/// stream finalization and the orphan watchdog).
pub(crate) async fn settle_and_emit(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    metrics: &crate::infra::metrics::Metrics,
    s: SettlementInput<'_>,
) -> DomainResult<Vec<toolkit_db::outbox::Wake>> {
    let usage_known = match s.state {
        "completed" => true,
        _ => s.usage.is_some_and(|u| u.is_nonzero()),
    };
    let (outcome, method) = billing::derive_billing(s.state, s.error_code, usage_known);
    let (ws, ci, fs) = s.counts;
    let mut charged = 0i64;
    if let (Some(reserve), Some(user_id)) = (s.reserve, s.user_id) {
        let (in_mult, out_mult) = s.multipliers;
        charged = match method {
            SettlementMethod::Actual => {
                let u = s.usage.unwrap_or_default();
                let actual = u.input_tokens.saturating_add(u.output_tokens);
                for period in ["daily", "monthly"] {
                    let l = crate::infra::metrics::labels(&[("period", period)]);
                    metrics.quota_commit.add(1, &l);
                    if actual > reserve.reserve_tokens {
                        metrics.quota_overshoot.add(1, &l);
                    }
                }
                #[allow(clippy::cast_precision_loss)]
                metrics.quota_actual_tokens.record(actual as f64, &[]);
                if ci > 0 {
                    metrics.code_interpreter_calls.add(
                        u64::from(ci),
                        &crate::infra::metrics::labels(&[("model", s.effective_model)]),
                    );
                }
                billing::settle_actual(
                    &reserve,
                    u.input_tokens,
                    u.output_tokens,
                    in_mult,
                    out_mult,
                    s.tolerance,
                )
                .map_err(|e| DomainError::internal(format!("settlement: {e}")))?
                .credits_micro
            }
            SettlementMethod::Estimated => {
                billing::settle_estimated(&reserve, in_mult, out_mult)
                    .map_err(|e| DomainError::internal(format!("settlement: {e}")))?
                    .credits_micro
            }
            SettlementMethod::Released => 0,
        };
        let tokens = (method == SettlementMethod::Actual).then(|| {
            let u = s.usage.unwrap_or_default();
            (u.input_tokens, u.output_tokens)
        });
        let tool_calls = (method != SettlementMethod::Released).then(|| {
            (
                i32::try_from(ws).unwrap_or(i32::MAX),
                i32::try_from(ci).unwrap_or(i32::MAX),
            )
        });
        Service::settle_buckets(
            tx,
            s.tenant_id,
            user_id,
            s.periods,
            s.premium,
            reserve.reserved_credits_micro,
            charged,
            tokens,
            tool_calls,
        )
        .await?;
    } else {
        tracing::warn!(
            turn_id = %s.turn_id,
            "mini-chat: turn has no reserve or requester; quota settlement skipped"
        );
    }

    let usage_payload = if usage_known && method == SettlementMethod::Actual {
        s.usage.map(|u| UsageTokens {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_write_input_tokens: u.cache_write_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
        })
    } else {
        None
    };
    let now = clock::now();
    let event = UsageEvent {
        tenant_id: s.tenant_id,
        user_id: s.user_id,
        chat_id: s.chat_id,
        turn_id: Some(s.turn_id),
        request_id: s.request_id,
        effective_model: s.effective_model.to_owned(),
        selected_model: s.selected_model.to_owned(),
        terminal_state: s.state.to_owned(),
        billing_outcome: outcome.as_str().to_owned(),
        usage: usage_payload,
        actual_credits_micro: charged,
        settlement_method: method.as_str().to_owned(),
        policy_version_applied: s.policy_version,
        web_search_calls: ws,
        code_interpreter_calls: ci,
        file_search_calls: fs,
        timestamp: now,
        requester_type: "user".to_owned(),
        dedupe_key: UsageEvent::turn_dedupe_key(s.tenant_id, s.turn_id, s.request_id),
        system_task_type: None,
    };
    let mut wakes = vec![
        outbox
            .enqueue_json(
                tx,
                &outbox.queues.queue_name,
                s.tenant_id,
                PAYLOAD_USAGE,
                &event,
            )
            .await?,
    ];
    let audit = TurnAuditEvent {
        event_type: if s.state == "completed" {
            TurnAuditEventType::TurnCompleted
        } else {
            TurnAuditEventType::TurnFailed
        },
        timestamp: now,
        tenant_id: s.tenant_id,
        requester_type: "user".to_owned(),
        user_id: s.user_id,
        chat_id: s.chat_id,
        turn_id: s.turn_id,
        request_id: s.request_id,
        selected_model: s.selected_model.to_owned(),
        effective_model: s.effective_model.to_owned(),
        terminal_state: s.state.to_owned(),
        error_code: s.error_code.map(str::to_owned),
        usage: s.usage.map(|u| AuditUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_write_input_tokens: u.cache_write_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
        }),
        latency_ms: s.latency,
        tool_calls: AuditToolCalls {
            web_search_calls: ws,
            file_search_calls: fs,
        },
        policy_decisions: AuditPolicyDecisions {
            license: String::new(),
            quota: s.quota_decision,
        },
        trace_id: None,
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
    };
    wakes.push(
        outbox
            .enqueue_json(
                tx,
                &outbox.queues.audit_queue_name,
                s.tenant_id,
                PAYLOAD_TURN_AUDIT,
                &audit,
            )
            .await?,
    );
    let _ = BillingOutcome::Completed;
    Ok(wakes)
}

impl Service {
    /// Finalize a turn and produce the terminal SSE event (`None`: nothing
    /// to send — cancellation or a lost CAS).
    #[allow(clippy::cognitive_complexity)]
    pub(crate) async fn finalize_turn(
        self: &Arc<Self>,
        run: &TurnRun,
        outcome: FinalOutcome,
        fin: TurnFinal,
    ) -> Option<StreamEvent> {
        match outcome {
            FinalOutcome::Completed {
                usage,
                response_id,
                incomplete: _,
            } => {
                let term = Terminal {
                    state: "completed",
                    error_code: None,
                    error_detail: None,
                    usage,
                    response_id: response_id.clone(),
                    persist_message: true,
                };
                match self.finalize_tx(run, term, &fin).await {
                    Ok(true) => Some(self.done_event(run, usage).await),
                    Ok(false) => None,
                    Err(DomainError::MessagePersistence { detail }) => {
                        tracing::error!(%detail, turn_id = %run.turn_id, "mini-chat: assistant message persistence failed");
                        let term = Terminal {
                            state: "failed",
                            error_code: Some("message_persistence_failed".to_owned()),
                            error_detail: Some(detail),
                            usage,
                            response_id,
                            persist_message: false,
                        };
                        match self.finalize_tx(run, term, &fin).await {
                            Ok(true) => Some(error_event(
                                "message_persistence_failed",
                                "The response could not be saved",
                            )),
                            Ok(false) => None,
                            Err(e) => {
                                tracing::error!(error = %e, "mini-chat: finalization failed");
                                Some(error_event(
                                    "finalization_failed",
                                    "The response could not be finalized",
                                ))
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, turn_id = %run.turn_id, "mini-chat: finalization failed");
                        Some(error_event(
                            "finalization_failed",
                            "The response could not be finalized",
                        ))
                    }
                }
            }
            FinalOutcome::Failed {
                code,
                message,
                detail,
                usage,
                response_id,
            } => {
                let term = Terminal {
                    state: "failed",
                    error_code: Some(code.clone()),
                    error_detail: detail,
                    usage,
                    response_id,
                    persist_message: false,
                };
                match self.finalize_tx(run, term, &fin).await {
                    Ok(true) => Some(error_event(&code, &message)),
                    Ok(false) => None,
                    Err(e) => {
                        tracing::error!(error = %e, turn_id = %run.turn_id, "mini-chat: finalization of a failed turn failed");
                        Some(error_event(&code, &message))
                    }
                }
            }
            FinalOutcome::Cancelled => {
                let persist = !fin.text.is_empty();
                let term = Terminal {
                    state: "cancelled",
                    error_code: None,
                    error_detail: None,
                    usage: None,
                    response_id: None,
                    persist_message: persist,
                };
                match self.finalize_tx(run, term.clone(), &fin).await {
                    Ok(_) => {}
                    Err(DomainError::MessagePersistence { detail }) => {
                        tracing::warn!(%detail, "mini-chat: partial message not persisted on cancel");
                        let mut t = term;
                        t.persist_message = false;
                        if let Err(e) = self.finalize_tx(run, t, &fin).await {
                            tracing::error!(error = %e, "mini-chat: cancel finalization failed");
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "mini-chat: cancel finalization failed"),
                }
                None
            }
        }
    }

    /// The finalization transaction. `Ok(true)`: CAS won and committed.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn finalize_tx(
        &self,
        run: &TurnRun,
        term: Terminal,
        fin: &TurnFinal,
    ) -> DomainResult<bool> {
        let outbox = Arc::clone(&self.outbox);
        let metrics = Arc::clone(&self.metrics);
        let cfg = Arc::clone(&self.cfg);
        let tenant_id = run.tenant_id;
        let user_id = run.user_id;
        let chat_id = run.chat_id;
        let turn_id = run.turn_id;
        let request_id = run.request_id;
        let message_id = run.assistant_message_id;
        let decision = run.decision.clone();
        let user_key = run.user_message_key;
        let trigger = cfg.thread_summary_worker.enabled
            && term.state == "completed"
            && (run.messages_truncated
                || (!run.has_summary
                    && run.plan_tokens
                        >= (run.effective_budget
                            * i64::from(cfg.thread_summary_worker.compression_threshold_pct))
                        .div_euclid(100)));
        let text = fin.text.clone();
        let counts = fin.counts;
        let latency = AuditLatency {
            ttft_ms: fin.ttft_ms,
            total_ms: u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let result = self
            .tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let metrics = Arc::clone(&metrics);
                let cfg = Arc::clone(&cfg);
                let term = term.clone();
                let decision = decision.clone();
                let text = text.clone();
                let latency = latency;
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(tenant_id);
                    let now = clock::now();
                    let cas = chat_turn::Entity::update_many()
                        .col_expr(chat_turn::Column::State, Expr::value(term.state))
                        .col_expr(
                            chat_turn::Column::ErrorCode,
                            Expr::value(term.error_code.clone()),
                        )
                        .col_expr(
                            chat_turn::Column::ErrorDetail,
                            Expr::value(term.error_detail.clone()),
                        )
                        .col_expr(
                            chat_turn::Column::ProviderResponseId,
                            Expr::value(term.response_id.clone()),
                        )
                        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
                        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(turn_id))
                                .add(chat_turn::Column::State.eq("running")),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if cas.rows_affected == 0 {
                        return Ok(None);
                    }
                    if term.persist_message {
                        let inserted = insert_message(
                            tx,
                            &scope,
                            NewMessage {
                                id: message_id,
                                tenant_id,
                                chat_id,
                                request_id,
                                role: "assistant",
                                content: &text,
                                model: Some(decision.effective.id.clone()),
                                usage: term.usage,
                                provider_response_id: term.response_id.clone(),
                                created_at: now,
                            },
                        )
                        .await;
                        if let Err(e) = inserted {
                            return Err(DomainError::MessagePersistence {
                                detail: e.to_string(),
                            });
                        }
                        chat_turn::Entity::update_many()
                            .col_expr(
                                chat_turn::Column::AssistantMessageId,
                                Expr::value(Some(message_id)),
                            )
                            .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
                            .secure()
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                    }
                    let quota_decision = AuditQuotaDecision {
                        decision: if decision.is_downgrade() {
                            "downgrade".to_owned()
                        } else {
                            "allow".to_owned()
                        },
                        downgrade_from: decision
                            .is_downgrade()
                            .then(|| decision.selected_model.clone()),
                        downgrade_reason: decision.downgrade_reason.clone(),
                        quota_scope: String::new(),
                    };
                    let mut wakes = settle_and_emit(
                        tx,
                        &outbox,
                        &metrics,
                        SettlementInput {
                            tenant_id,
                            user_id: Some(user_id),
                            chat_id,
                            turn_id,
                            request_id,
                            selected_model: &decision.selected_model,
                            effective_model: &decision.effective.id,
                            premium: decision.is_premium(),
                            state: term.state,
                            error_code: term.error_code.as_deref(),
                            usage: term.usage,
                            reserve: Some(PersistedReserve {
                                reserve_tokens: decision.reserve.reserve_tokens,
                                max_output_tokens_applied: decision
                                    .reserve
                                    .max_output_tokens_applied,
                                reserved_credits_micro: decision.reserve.reserved_credits_micro,
                                minimal_generation_floor_applied: decision
                                    .minimal_generation_floor_applied,
                            }),
                            multipliers: (
                                decision.effective.input_tokens_credit_multiplier_micro,
                                decision.effective.output_tokens_credit_multiplier_micro,
                            ),
                            policy_version: decision.policy_version,
                            periods: decision.periods,
                            counts: (
                                counts.web_search_done,
                                counts.code_interpreter_done,
                                counts.file_search_done + counts.knowledge_calls,
                            ),
                            quota_decision,
                            latency,
                            tolerance: cfg.quota.overshoot_tolerance_factor,
                        },
                    )
                    .await?;
                    if trigger {
                        let scheduled = match enqueue_summary_if_needed(
                            tx, &outbox, &scope, tenant_id, chat_id, user_key,
                        )
                        .await?
                        {
                            Some(w) => {
                                wakes.push(w);
                                "scheduled"
                            }
                            None => "not_needed",
                        };
                        metrics
                            .thread_summary_trigger
                            .add(1, &crate::infra::metrics::labels(&[("result", scheduled)]));
                    }
                    Ok(Some(wakes))
                })
            })
            .await?;
        match result {
            Some(wakes) => {
                fire(wakes);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn done_event(&self, run: &TurnRun, usage: Option<Usage>) -> StreamEvent {
        let d = &run.decision;
        let u = usage.unwrap_or_default();
        let warnings = match self.db.conn() {
            Ok(conn) => {
                let now = clock::now();
                match Self::usage_state(&conn, run.tenant_id, run.user_id, Periods::at(now)).await {
                    Ok(state) => Some(
                        self.quota_status_rows(&d.limits, &state, now)
                            .into_iter()
                            .flat_map(|t| {
                                t.periods.into_iter().map(move |p| QuotaWarning {
                                    tier: t.tier,
                                    period: p.period,
                                    remaining_percentage: p.remaining_percentage,
                                    warning: p.warning,
                                    exhausted: p.exhausted,
                                    next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
                                })
                            })
                            .collect(),
                    ),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        };
        StreamEvent::Done(DoneInfo {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            effective_model: d.effective.id.clone(),
            selected_model: d.selected_model.clone(),
            downgrade: d.is_downgrade(),
            downgrade_from: d.is_downgrade().then(|| d.selected_model.clone()),
            downgrade_reason: if d.is_downgrade() {
                d.downgrade_reason.clone()
            } else {
                None
            },
            quota_warnings: warnings,
        })
    }
}

/// `error` event.
#[must_use]
pub fn error_event(code: &str, message: &str) -> StreamEvent {
    StreamEvent::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// Enqueue a thread-summary task when the frozen target is beyond the
/// stored frontier. The target is the latest live message before the
/// finalized turn's user message.
pub(crate) async fn enqueue_summary_if_needed(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    user_key: OrderKey,
) -> DomainResult<Option<toolkit_db::outbox::Wake>> {
    let (ts, id) = user_key;
    let target = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any().add(message::Column::CreatedAt.lt(ts)).add(
                        Condition::all()
                            .add(message::Column::CreatedAt.eq(ts))
                            .add(message::Column::Id.lt(id)),
                    ),
                ),
        )
        .order_by(message::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(message::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .one(tx)
        .await?;
    let Some(target) = target else {
        return Ok(None);
    };
    let summary = thread_summary::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .one(tx)
        .await?;
    if let Some(s) = &summary
        && (s.summarized_up_to_created_at, s.summarized_up_to_message_id)
            >= (target.created_at, target.id)
    {
        return Ok(None);
    }
    let task = ThreadSummaryTask {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        system_task_type: THREAD_SUMMARY_TASK.to_owned(),
        base_frontier_created_at: summary.as_ref().map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
    };
    let wake = outbox
        .enqueue_json(
            tx,
            &outbox.queues.thread_summary_queue_name,
            chat_id,
            PAYLOAD_THREAD_SUMMARY,
            &task,
        )
        .await?;
    Ok(Some(wake))
}
