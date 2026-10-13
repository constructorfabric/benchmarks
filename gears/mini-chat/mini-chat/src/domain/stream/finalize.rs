//! Turn finalization: one transaction with the CAS on `state = 'running'`,
//! the assistant message, quota settlement and the usage / audit outbox
//! events (DESIGN §5.7).

use mini_chat_sdk::{
    AuditUsage, MiniChatAuditEvent, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::task::{ToolCounters, TurnRun};
use super::{DoneData, QuotaWarningOut};
use crate::domain::billing::{Terminal, classify, settle};
use crate::domain::context::summary_trigger;
use crate::domain::error::DomainError;
use crate::domain::quota::{ToolCounts, apply_settlement};
use crate::domain::sanitize::sanitize_provider_message;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{chat_turns, messages, thread_summaries};
use crate::infra::db::now;
use crate::infra::outbox::{OutboxEnqueuer, Queue, ThreadSummaryTask};

/// Terminal outcome reported by the provider task.
#[derive(Debug, Clone)]
pub enum Outcome {
    Completed {
        text: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
    Cancelled {
        text: String,
    },
}

/// Result of finalization as seen by the SSE relay.
#[derive(Debug)]
pub enum FinalizeOutcome {
    Completed(Box<DoneData>),
    Failed { code: String, message: String },
    /// Another finalizer won the CAS.
    Lost,
    /// The finalization transaction failed.
    Error(DomainError),
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// `{tenant}/{turn}/{request}` in simple UUID form.
#[must_use]
pub fn dedupe_key(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!("{}/{}/{}", tenant.as_simple(), turn.as_simple(), request.as_simple())
}

/// What the finalization transaction writes.
#[derive(Debug, Clone)]
struct Plan {
    state: &'static str,
    error_code: Option<String>,
    terminal: Terminal,
    message: Option<(String, Option<UsageTokens>)>,
    response_id: Option<String>,
    usage: Option<UsageTokens>,
}

/// Inputs of the thread-summary trigger.
#[derive(Debug, Clone, Copy)]
pub struct SummaryTrigger {
    pub enabled: bool,
    pub fire: bool,
}

impl MiniChat {
    /// Finalize a turn; the caller emits the terminal SSE event from the result.
    // reason: one arm per stream outcome, each mapping to a distinct settlement plan
    #[allow(clippy::cognitive_complexity)]
    pub async fn finalize_turn(&self, run: &TurnRun, outcome: Outcome, counters: ToolCounters) -> FinalizeOutcome {
        match outcome {
            Outcome::Completed {
                text,
                usage,
                response_id,
                incomplete_reason,
            } => {
                if let Some(r) = &incomplete_reason {
                    tracing::warn!(turn_id = %run.turn_id, reason = %r, "stream incomplete");
                }
                let plan = Plan {
                    state: "completed",
                    error_code: None,
                    terminal: Terminal::Completed,
                    message: Some((text, usage)),
                    response_id,
                    usage,
                };
                match self.finalize_tx(run, plan, counters).await {
                    Ok(true) => FinalizeOutcome::Completed(Box::new(self.done_data(run, usage, true).await)),
                    Ok(false) => FinalizeOutcome::Lost,
                    Err(DomainError::MessagePersistence(e)) => {
                        tracing::error!(error = %e, "assistant message persistence failed");
                        let plan = Plan {
                            state: "failed",
                            error_code: Some("message_persistence_failed".into()),
                            terminal: Terminal::Failed("message_persistence_failed".into()),
                            message: None,
                            response_id: None,
                            usage,
                        };
                        match self.finalize_tx(run, plan, counters).await {
                            Ok(true) => FinalizeOutcome::Failed {
                                code: "message_persistence_failed".into(),
                                message: "The response could not be saved".into(),
                            },
                            Ok(false) => FinalizeOutcome::Lost,
                            Err(e) => FinalizeOutcome::Error(e),
                        }
                    }
                    Err(e) => FinalizeOutcome::Error(e),
                }
            }
            Outcome::Failed { code, message, usage } => {
                let plan = Plan {
                    state: "failed",
                    error_code: Some(code.clone()),
                    terminal: Terminal::Failed(code.clone()),
                    message: None,
                    response_id: None,
                    usage,
                };
                match self.finalize_tx(run, plan, counters).await {
                    Ok(true) => FinalizeOutcome::Failed {
                        code,
                        message: sanitize_provider_message(&message),
                    },
                    Ok(false) => FinalizeOutcome::Lost,
                    Err(e) => FinalizeOutcome::Error(e),
                }
            }
            Outcome::Cancelled { text } => {
                let message = (!text.is_empty()).then_some((text, None));
                let with_msg = message.is_some();
                let plan = Plan {
                    state: "cancelled",
                    error_code: None,
                    terminal: Terminal::Cancelled,
                    message,
                    response_id: None,
                    usage: None,
                };
                match self.finalize_tx(run, plan.clone(), counters).await {
                    Ok(true) => FinalizeOutcome::Failed {
                        code: "cancelled".into(),
                        message: String::new(),
                    },
                    Ok(false) => FinalizeOutcome::Lost,
                    Err(e) if with_msg => {
                        tracing::warn!(error = %e, "partial message persistence failed; finalizing without it");
                        let plan = Plan { message: None, ..plan };
                        match self.finalize_tx(run, plan, counters).await {
                            Ok(true) => FinalizeOutcome::Failed {
                                code: "cancelled".into(),
                                message: String::new(),
                            },
                            Ok(false) => FinalizeOutcome::Lost,
                            Err(e) => FinalizeOutcome::Error(e),
                        }
                    }
                    Err(e) => FinalizeOutcome::Error(e),
                }
            }
        }
    }

    async fn done_data(&self, run: &TurnRun, usage: Option<UsageTokens>, warnings: bool) -> DoneData {
        let u = usage.unwrap_or_default();
        let quota_warnings = if warnings {
            match self.quota_entries(run.tenant_id, run.user_id).await {
                Ok(entries) => Some(
                    entries
                        .into_iter()
                        .map(|e| QuotaWarningOut {
                            tier: e.tier,
                            period: e.period,
                            remaining_percentage: e.remaining_percentage,
                            warning: e.warning,
                            exhausted: e.exhausted,
                            next_reset: (e.warning || e.exhausted).then_some(e.next_reset),
                        })
                        .collect(),
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "quota warnings unavailable");
                    None
                }
            }
        } else {
            None
        };
        DoneData {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            effective_model: run.effective_model.clone(),
            selected_model: run.selected_model.clone(),
            downgraded: run.downgraded,
            downgrade_from: run.downgraded.then(|| run.selected_model.clone()),
            downgrade_reason: if run.downgraded { run.downgrade_reason.clone() } else { None },
            quota_warnings,
        }
    }

    /// One finalization transaction. `Ok(false)` when the CAS was lost.
    async fn finalize_tx(&self, run: &TurnRun, plan: Plan, counters: ToolCounters) -> Result<bool, DomainError> {
        let run = run.clone();
        let outbox = self.outbox.clone();
        let tolerance = self.cfg.quota.overshoot_tolerance_factor;
        let trigger_enabled = self.cfg.thread_summary_worker.enabled;
        let threshold = self.cfg.thread_summary_worker.compression_threshold_pct;
        let res = crate::infra::db::tx_retry(&self.db, move |tx| {
                let outbox = outbox.clone();
                let plan = plan.clone();
                let run = run.clone();
                Box::pin(async move {
                    let ts = now();
                    let scope = AccessScope::for_tenant(run.tenant_id);
                    let msg_id = plan.message.as_ref().map(|_| run.assistant_message_id);
                    let cas = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::State, Expr::value(plan.state))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value(plan.error_code.clone()))
                        .col_expr(chat_turns::Column::AssistantMessageId, Expr::value(msg_id))
                        .col_expr(chat_turns::Column::ProviderResponseId, Expr::value(plan.response_id.clone()))
                        .col_expr(
                            chat_turns::Column::WebSearchCompletedCount,
                            Expr::value(i32::try_from(counters.web_search_completed).unwrap_or(i32::MAX)),
                        )
                        .col_expr(
                            chat_turns::Column::CodeInterpreterCompletedCount,
                            Expr::value(i32::try_from(counters.code_interpreter_completed).unwrap_or(i32::MAX)),
                        )
                        .col_expr(
                            chat_turns::Column::FileSearchCompletedCount,
                            Expr::value(i32::try_from(counters.file_search_completed).unwrap_or(i32::MAX)),
                        )
                        .filter(
                            Condition::all()
                                .add(chat_turns::Column::Id.eq(run.turn_id))
                                .add(chat_turns::Column::State.eq("running")),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if cas.rows_affected == 0 {
                        return Ok(None);
                    }
                    if let Some((text, usage)) = &plan.message {
                        insert_assistant_message(tx, &run, text, *usage, plan.response_id.clone(), ts)
                            .await
                            .map_err(|e| DomainError::MessagePersistence(e.to_string()))?;
                    }
                    let mut wake = settle_and_publish(tx, &outbox, &run, &plan, counters, tolerance, ts).await?;
                    if plan.state == "completed" && trigger_enabled {
                        wake += maybe_schedule_summary(tx, &outbox, &run, threshold).await?;
                    }
                    Ok(Some(wake))
                })
            })
            .await?;
        match res {
            Some(w) => {
                w.fire();
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

async fn insert_assistant_message(
    tx: &impl DBRunner,
    run: &TurnRun,
    text: &str,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
    ts: time::OffsetDateTime,
) -> Result<(), DomainError> {
    let u = usage.unwrap_or_default();
    let am = messages::ActiveModel {
        id: Set(run.assistant_message_id),
        tenant_id: Set(run.tenant_id),
        chat_id: Set(run.chat_id),
        request_id: Set(Some(run.request_id)),
        role: Set("assistant".into()),
        content: Set(text.to_owned()),
        content_type: Set("text".into()),
        token_estimate: Set(0),
        provider_response_id: Set(response_id),
        request_kind: Set("chat".into()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(u.input_tokens),
        output_tokens: Set(u.output_tokens),
        cache_read_input_tokens: Set(u.cache_read_input_tokens),
        cache_write_input_tokens: Set(u.cache_write_input_tokens),
        reasoning_tokens: Set(u.reasoning_tokens),
        model: Set(Some(run.effective_model.clone())),
        is_compressed: Set(false),
        created_at: Set(ts),
        deleted_at: Set(None),
    };
    messages::Entity::insert(am)
        .secure()
        .scope_unchecked(&AccessScope::for_tenant(run.tenant_id))?
        .exec(tx)
        .await?;
    Ok(())
}

async fn settle_and_publish(
    tx: &impl DBRunner,
    outbox: &OutboxEnqueuer,
    run: &TurnRun,
    plan: &Plan,
    counters: ToolCounters,
    tolerance: f64,
    ts: time::OffsetDateTime,
) -> Result<Wake, DomainError> {
    let class = classify(&plan.terminal, plan.usage.as_ref());
    let s = settle(class.method, run.reserve, plan.usage.as_ref(), run.in_mult, run.out_mult, tolerance)
        .map_err(|e| DomainError::Internal(format!("settlement: {e}")))?;
    apply_settlement(
        tx,
        run.tenant_id,
        run.user_id,
        run.periods,
        run.tier == ModelTier::Premium,
        run.reserve.reserved_credits_micro,
        &s,
        ToolCounts {
            web_search: i32::try_from(counters.web_search_completed).unwrap_or(i32::MAX),
            code_interpreter: i32::try_from(counters.code_interpreter_completed).unwrap_or(i32::MAX),
        },
    )
    .await?;
    let usage_out = if class.method == "actual" { plan.usage } else { None };
    let event = UsageEvent {
        tenant_id: run.tenant_id,
        user_id: Some(run.user_id),
        chat_id: run.chat_id,
        turn_id: Some(run.turn_id),
        request_id: run.request_id,
        effective_model: run.effective_model.clone(),
        selected_model: run.selected_model.clone(),
        terminal_state: plan.state.to_owned(),
        billing_outcome: class.billing_outcome.to_owned(),
        usage: usage_out,
        actual_credits_micro: s.committed_credits_micro,
        settlement_method: class.method.to_owned(),
        policy_version_applied: run.policy_version,
        web_search_calls: counters.web_search_completed,
        code_interpreter_calls: counters.code_interpreter_completed,
        file_search_calls: counters.file_search_calls(),
        timestamp: rfc3339(ts),
        requester_type: "user".into(),
        dedupe_key: dedupe_key(run.tenant_id, run.turn_id, run.request_id),
        system_task_type: None,
    };
    let mut wake = outbox.enqueue(tx, Queue::Usage, run.tenant_id, &event).await?;
    let u = plan.usage.unwrap_or_default();
    let audit = TurnAuditEvent {
        event_type: if plan.state == "completed" { "turn_completed" } else { "turn_failed" }.into(),
        tenant_id: run.tenant_id,
        user_id: Some(run.user_id),
        requester_type: "user".into(),
        chat_id: run.chat_id,
        turn_id: run.turn_id,
        request_id: run.request_id,
        selected_model: run.selected_model.clone(),
        effective_model: run.effective_model.clone(),
        terminal_state: plan.state.to_owned(),
        error_code: plan.error_code.clone(),
        usage: AuditUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_write_input_tokens: u.cache_write_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
        },
        latency_ms: u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        tool_calls: ToolCalls {
            web_search_calls: counters.web_search_completed,
            file_search_calls: counters.file_search_calls(),
        },
        policy_decisions: PolicyDecisions {
            quota: QuotaPolicyDecision {
                decision: if run.downgraded { "downgrade" } else { "allow" }.into(),
                downgrade_from: run.downgraded.then(|| run.selected_model.clone()),
                downgrade_reason: if run.downgraded { run.downgrade_reason.clone() } else { None },
            },
            license: String::new(),
            quota_scope: String::new(),
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        trace_id: None,
        timestamp: rfc3339(ts),
    };
    wake += outbox
        .enqueue(tx, Queue::Audit, run.tenant_id, &MiniChatAuditEvent::Turn(Box::new(audit)))
        .await?;
    Ok(wake)
}

async fn maybe_schedule_summary(
    tx: &impl DBRunner,
    outbox: &OutboxEnqueuer,
    run: &TurnRun,
    threshold: u32,
) -> Result<Wake, DomainError> {
    let plan = crate::domain::context::ContextPlan {
        history: Vec::new(),
        summary_applied: None,
        assembled_tokens: run.assembled_tokens,
        messages_truncated: run.messages_truncated,
        effective_budget: run.effective_budget,
    };
    if !summary_trigger(&plan, run.has_summary, threshold) {
        return Ok(Wake::empty());
    }
    let scope = AccessScope::for_tenant(run.tenant_id);
    let target = messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(run.chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(messages::Column::RequestId.ne(run.request_id)),
        )
        .order_by_desc(messages::Column::CreatedAt)
        .order_by_desc(messages::Column::Id)
        .limit(1)
        .secure()
        .scope_with(&scope)
        .one(tx)
        .await?;
    let Some(target) = target else {
        tracing::debug!(chat_id = %run.chat_id, "thread summary not needed: nothing to summarize");
        return Ok(Wake::empty());
    };
    let summary = thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(run.chat_id))
        .secure()
        .scope_with(&scope)
        .one(tx)
        .await?;
    if let Some(s) = &summary
        && s.summarized_up_to_message_id == target.id
    {
        return Ok(Wake::empty());
    }
    let task = ThreadSummaryTask {
        tenant_id: run.tenant_id,
        chat_id: run.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: summary.as_ref().map(|s| rfc3339(s.summarized_up_to_created_at)),
        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: rfc3339(target.created_at),
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".into(),
    };
    outbox.enqueue(tx, Queue::ThreadSummary, run.chat_id, &task).await
}
