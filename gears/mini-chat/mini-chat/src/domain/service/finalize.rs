//! CAS-guarded turn finalization: assistant message, turn state, quota
//! settlement, usage + audit outbox events and the thread-summary trigger,
//! all in one transaction (DESIGN §5.7–5.9).

use std::time::{Duration, Instant};

use mini_chat_sdk::{
    AuditEvent, LatencyMs, ModelTier, PolicyDecisions, QuotaDecisionAudit, ToolCalls, TurnAuditEvent, UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DbTx, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::stream::{Citation, DonePayload, LiveTurn, QuotaWarningView, SseEvent};
use super::{MiniChatService, now};
use crate::domain::billing::{Method, ReserveFields, Settlement, Terminal, settle};
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::quota::{BUCKET_PREMIUM, BUCKET_TOTAL, BucketDelta, apply_delta, load_usage, period_starts, quota_status};
use crate::infra::db::entities::{chat_turns, messages, thread_summaries};
use crate::infra::outbox::{OutboxEnqueuer, ThreadSummaryTask, fire};

/// Terminal outcome reported by the relay task.
#[derive(Debug, Clone)]
pub enum Outcome {
    Completed { text: String, usage: Option<UsageTokens>, response_id: Option<String>, citations: Vec<Citation> },
    Failed { code: String, message: String, usage: Option<UsageTokens> },
    Cancelled { text: String },
}

/// Per-turn tool counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounters {
    pub web_search_started: u32,
    pub web_search_done: u32,
    pub code_interpreter_started: u32,
    pub code_interpreter_done: u32,
    pub file_search_done: u32,
}

const CAS_LOST: &str = "__cas_lost__";

fn cas_lost() -> DomainError {
    DomainError::Internal(CAS_LOST.to_owned())
}

fn is_cas_lost(e: &DomainError) -> bool {
    matches!(e, DomainError::Internal(m) if m == CAS_LOST)
}

/// Inputs of the shared settlement + usage-event step.
#[derive(Debug, Clone)]
pub(crate) struct SettleCtx {
    pub tenant_id: Uuid,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub tier: Option<ModelTier>,
    pub reserve: Option<ReserveFields>,
    pub policy_version: u64,
    pub period_anchor: OffsetDateTime,
    pub in_mult: i64,
    pub out_mult: i64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    pub overshoot_tolerance: f64,
}

#[must_use]
pub(crate) fn dedupe_key(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!("{}/{}/{}", tenant.as_simple(), turn.as_simple(), request.as_simple())
}

/// Settle the reserve and enqueue the usage event (shared by the stream
/// finalization and the orphan watchdog).
pub(crate) async fn settle_and_enqueue(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    s: &SettleCtx,
    terminal: &Terminal,
    usage: Option<UsageTokens>,
) -> DomainResult<(Option<Settlement>, Wake)> {
    let terminal_state = terminal.state().to_owned();
    let (settlement, ev) = if let (Some(r), Some(user_id)) = (s.reserve, s.user_id) {
        let st = settle(terminal, usage, r, s.in_mult, s.out_mult, s.overshoot_tolerance)
            .map_err(|e| DomainError::internal(format!("settlement credit computation failed: {e}")))?;
        let tool_add = |n: u32| if st.count_tool_calls { i64::from(n) } else { 0 };
        for (period, start) in period_starts(s.period_anchor) {
            apply_delta(tx, s.tenant_id, user_id, period, start, BUCKET_TOTAL, BucketDelta {
                reserved: -r.reserved_credits_micro,
                spent: st.committed_credits_micro,
                calls: 1,
                input_tokens: st.telemetry_input,
                output_tokens: st.telemetry_output,
                web_search_calls: tool_add(s.web_search_calls),
                code_interpreter_calls: tool_add(s.code_interpreter_calls),
            })
            .await?;
            if s.tier == Some(ModelTier::Premium) {
                apply_delta(tx, s.tenant_id, user_id, period, start, BUCKET_PREMIUM, BucketDelta {
                    reserved: -r.reserved_credits_micro,
                    spent: st.committed_credits_micro,
                    calls: 1,
                    ..Default::default()
                })
                .await?;
            }
        }
        let ev = UsageEvent {
            tenant_id: s.tenant_id,
            user_id: s.user_id,
            chat_id: s.chat_id,
            turn_id: Some(s.turn_id),
            request_id: s.request_id,
            effective_model: s.effective_model.clone(),
            selected_model: s.selected_model.clone(),
            terminal_state,
            billing_outcome: st.billing_outcome.to_owned(),
            usage: st.event_usage,
            actual_credits_micro: st.committed_credits_micro,
            settlement_method: st.method.as_str().to_owned(),
            policy_version_applied: s.policy_version,
            web_search_calls: s.web_search_calls,
            code_interpreter_calls: s.code_interpreter_calls,
            file_search_calls: s.file_search_calls,
            timestamp: OffsetDateTime::now_utc(),
            requester_type: "user".to_owned(),
            dedupe_key: dedupe_key(s.tenant_id, s.turn_id, s.request_id),
            system_task_type: None,
        };
        (Some(st), ev)
    } else {
        tracing::warn!(turn_id = %s.turn_id, "turn has no reserve fields; settlement skipped");
        let ev = UsageEvent {
            tenant_id: s.tenant_id,
            user_id: s.user_id,
            chat_id: s.chat_id,
            turn_id: Some(s.turn_id),
            request_id: s.request_id,
            effective_model: String::new(),
            selected_model: String::new(),
            terminal_state,
            billing_outcome: "aborted".to_owned(),
            usage: None,
            actual_credits_micro: 0,
            settlement_method: Method::Estimated.as_str().to_owned(),
            policy_version_applied: 0,
            web_search_calls: s.web_search_calls,
            code_interpreter_calls: s.code_interpreter_calls,
            file_search_calls: s.file_search_calls,
            timestamp: OffsetDateTime::now_utc(),
            requester_type: "user".to_owned(),
            dedupe_key: dedupe_key(s.tenant_id, s.turn_id, s.request_id),
            system_task_type: None,
        };
        (None, ev)
    };
    let wake = outbox.usage(tx, &ev).await?;
    Ok((settlement, wake))
}

/// Result of the summary trigger evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerResult {
    NotEvaluated,
    Scheduled,
    NotNeeded,
}

impl MiniChatService {
    #[allow(clippy::too_many_arguments)]
    async fn finalize_tx(
        &self,
        live: &LiveTurn,
        terminal: Terminal,
        persist_text: Option<String>,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        error_detail: Option<String>,
        counters: ToolCounters,
        latency: LatencyMs,
    ) -> DomainResult<TriggerResult> {
        let outbox = self.outbox.clone();
        let cfg = self.cfg.clone();
        let live_ids = (live.turn_id, live.request_id, live.chat_id, live.tenant_id, live.user_id, live.assistant_message_id);
        let sctx = SettleCtx {
            tenant_id: live.tenant_id,
            user_id: Some(live.user_id),
            chat_id: live.chat_id,
            turn_id: live.turn_id,
            request_id: live.request_id,
            selected_model: live.selected_model.clone(),
            effective_model: live.effective_model.clone(),
            tier: Some(live.tier),
            reserve: Some(live.reserve),
            policy_version: live.policy_version,
            period_anchor: live.started_at,
            in_mult: live.in_mult,
            out_mult: live.out_mult,
            web_search_calls: counters.web_search_done,
            code_interpreter_calls: counters.code_interpreter_done,
            file_search_calls: counters.file_search_done,
            overshoot_tolerance: self.cfg.quota.overshoot_tolerance_factor,
        };
        let audit_base = (
            live.selected_model.clone(),
            live.effective_model.clone(),
            live.decision.as_str().to_owned(),
            live.downgrade_reason.clone(),
        );
        let trigger_inputs = (live.context_truncated, live.summary_exists, live.assembled_tokens, live.effective_budget);
        let (wakes, trigger) = self
            .tx(move |tx| {
                let outbox = outbox.clone();
                let cfg = cfg.clone();
                let terminal = terminal.clone();
                let persist_text = persist_text.clone();
                let response_id = response_id.clone();
                let error_detail = error_detail.clone();
                let sctx = sctx.clone();
                let audit_base = audit_base.clone();
                let latency = latency.clone();
                Box::pin(async move {
                    let (turn_id, request_id, chat_id, tenant_id, user_id, assistant_message_id) = live_ids;
                    let scope = AccessScope::for_tenant(tenant_id);
                    let ts = now();
                    let u = usage.unwrap_or_default();
                    let assistant_id = if let Some(text) = persist_text {
                        let am = messages::ActiveModel {
                            id: Set(assistant_message_id),
                            tenant_id: Set(tenant_id),
                            chat_id: Set(chat_id),
                            request_id: Set(Some(request_id)),
                            role: Set("assistant".into()),
                            content: Set(text),
                            content_type: Set("text".into()),
                            token_estimate: Set(0),
                            provider_response_id: Set(response_id.clone()),
                            request_kind: Set("chat".into()),
                            features_used: Set(json!([])),
                            input_tokens: Set(u.input_tokens.max(0)),
                            output_tokens: Set(u.output_tokens.max(0)),
                            cache_read_input_tokens: Set(u.cache_read_input_tokens.max(0)),
                            cache_write_input_tokens: Set(u.cache_write_input_tokens.max(0)),
                            reasoning_tokens: Set(u.reasoning_tokens.max(0)),
                            model: Set(Some(sctx.effective_model.clone())),
                            is_compressed: Set(false),
                            created_at: Set(ts),
                            deleted_at: Set(None),
                        };
                        secure_insert::<messages::Entity>(am, &scope, tx).await?;
                        Some(assistant_message_id)
                    } else {
                        None
                    };
                    let error_code = match &terminal {
                        Terminal::Failed { error_code } => Some(error_code.clone()),
                        _ => None,
                    };
                    let res = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::State, Expr::value(terminal.state()))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value(error_code.clone()))
                        .col_expr(chat_turns::Column::ErrorDetail, Expr::value(error_detail))
                        .col_expr(chat_turns::Column::AssistantMessageId, Expr::value(assistant_id))
                        .col_expr(chat_turns::Column::ProviderResponseId, Expr::value(response_id))
                        .col_expr(chat_turns::Column::WebSearchCompletedCount, Expr::value(i32::try_from(counters.web_search_done).unwrap_or(i32::MAX)))
                        .col_expr(chat_turns::Column::CodeInterpreterCompletedCount, Expr::value(i32::try_from(counters.code_interpreter_done).unwrap_or(i32::MAX)))
                        .col_expr(chat_turns::Column::FileSearchCompletedCount, Expr::value(i32::try_from(counters.file_search_done).unwrap_or(i32::MAX)))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(ts))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)).add(chat_turns::Column::State.eq("running")))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(cas_lost());
                    }
                    let (_settlement, wake) = settle_and_enqueue(tx, &outbox, &sctx, &terminal, usage).await?;
                    let mut wakes = vec![wake];
                    let (selected, effective, decision, reason) = audit_base;
                    let downgrade_from = (decision == "downgrade").then(|| selected.clone());
                    let audit = AuditEvent::Turn(TurnAuditEvent {
                        event_type: if terminal == Terminal::Completed {
                            mini_chat_sdk::audit::event_types::TURN_COMPLETED
                        } else {
                            mini_chat_sdk::audit::event_types::TURN_FAILED
                        }
                        .to_owned(),
                        timestamp: OffsetDateTime::now_utc(),
                        tenant_id,
                        requester_type: "user".to_owned(),
                        user_id: Some(user_id),
                        chat_id,
                        turn_id,
                        request_id,
                        selected_model: selected,
                        effective_model: effective,
                        usage: u,
                        latency_ms: latency,
                        tool_calls: ToolCalls {
                            web_search_calls: counters.web_search_done,
                            file_search_calls: counters.file_search_done,
                        },
                        policy_decisions: PolicyDecisions {
                            quota: QuotaDecisionAudit { decision, downgrade_from, downgrade_reason: reason },
                            license: None,
                            quota_scope: None,
                        },
                        error_code,
                        prompt: String::new(),
                        response: String::new(),
                        attachments: Vec::new(),
                        trace_id: None,
                    });
                    wakes.push(outbox.audit(tx, &audit).await?);

                    // Thread summary trigger (completed turns only).
                    let mut trigger = TriggerResult::NotEvaluated;
                    if terminal == Terminal::Completed && cfg.thread_summary_worker.enabled {
                        let (truncated, summary_exists, assembled, budget) = trigger_inputs;
                        let proactive = !summary_exists
                            && budget.is_some_and(|b| {
                                assembled * 100 >= b * i64::from(cfg.thread_summary_worker.compression_threshold_pct)
                            });
                        if truncated || proactive {
                            trigger = TriggerResult::NotNeeded;
                            let summary = thread_summaries::Entity::find()
                                .filter(thread_summaries::Column::ChatId.eq(chat_id))
                                .secure()
                                .scope_with(&scope)
                                .one(tx)
                                .await?;
                            let target = messages::Entity::find()
                                .filter(
                                    Condition::all()
                                        .add(messages::Column::ChatId.eq(chat_id))
                                        .add(messages::Column::DeletedAt.is_null())
                                        .add(messages::Column::RequestId.ne(request_id)),
                                )
                                .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
                                .order_by(messages::Column::Id, sea_orm::Order::Desc)
                                .limit(1)
                                .secure()
                                .scope_with(&scope)
                                .one(tx)
                                .await?;
                            if let Some(t) = target {
                                let beyond = summary.as_ref().is_none_or(|s| {
                                    (t.created_at, t.id) > (s.summarized_up_to_created_at, s.summarized_up_to_message_id)
                                });
                                if beyond {
                                    let task = ThreadSummaryTask {
                                        tenant_id,
                                        chat_id,
                                        system_request_id: Uuid::new_v4(),
                                        base_frontier_created_at: summary.as_ref().map(|s| s.summarized_up_to_created_at),
                                        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
                                        frozen_target_created_at: t.created_at,
                                        frozen_target_message_id: t.id,
                                        system_task_type: "thread_summary_update".to_owned(),
                                    };
                                    wakes.push(outbox.thread_summary(tx, &task).await?);
                                    trigger = TriggerResult::Scheduled;
                                }
                            }
                        }
                    }
                    Ok((wakes, trigger))
                })
            })
            .await?;
        fire(wakes);
        Ok(trigger)
    }

    async fn quota_warnings(&self, live: &LiveTurn) -> Option<Vec<QuotaWarningView>> {
        let conn = self.db.conn().ok()?;
        let usage = load_usage(&conn, live.tenant_id, live.user_id, live.started_at).await.ok()?;
        let st = quota_status(&usage, &live.limits, self.cfg.quota.warning_threshold_pct, OffsetDateTime::now_utc());
        Some(
            st.into_iter()
                .flat_map(|(_, periods)| periods)
                .map(|p| QuotaWarningView {
                    tier: p.tier,
                    period: p.period,
                    remaining_percentage: p.remaining_percentage,
                    warning: p.warning,
                    exhausted: p.exhausted,
                    next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
                })
                .collect(),
        )
    }

    /// Finalize the turn and emit the terminal SSE event (only after commit).
    #[allow(clippy::cognitive_complexity)] // one branch per terminal outcome, kept together for auditability
    pub(crate) async fn finish(
        &self,
        live: &LiveTurn,
        outcome: Outcome,
        counters: &ToolCounters,
        ttft: Option<Duration>,
        begun: Instant,
        tx: &mpsc::Sender<SseEvent>,
    ) {
        let latency = LatencyMs {
            ttft_ms: ttft.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            total_ms: u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        match outcome {
            Outcome::Completed { text, usage, response_id, citations } => {
                let res = self
                    .finalize_tx(live, Terminal::Completed, Some(text), usage, response_id.clone(), None, *counters, latency.clone())
                    .await;
                match res {
                    Ok(_) => {
                        if !citations.is_empty() {
                            drop(tx.send(SseEvent::Citations(citations)).await);
                        }
                        let u = usage.unwrap_or_default();
                        let downgraded = live.decision == crate::domain::quota::QuotaDecision::Downgrade;
                        let warnings = self.quota_warnings(live).await;
                        drop(
                            tx.send(SseEvent::Done(DonePayload {
                                input_tokens: u.input_tokens,
                                output_tokens: u.output_tokens,
                                effective_model: live.effective_model.clone(),
                                selected_model: live.selected_model.clone(),
                                quota_decision: live.decision.as_str(),
                                downgrade_from: downgraded.then(|| live.selected_model.clone()),
                                downgrade_reason: if downgraded { live.downgrade_reason.clone() } else { None },
                                quota_warnings: warnings,
                            }))
                            .await,
                        );
                    }
                    Err(e) if is_cas_lost(&e) => {
                        drop(
                            tx.send(SseEvent::Error { code: "stream_interrupted".into(), message: "The stream was interrupted".into() })
                            .await,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, request_id = %live.request_id, "completed finalization failed; marking message_persistence_failed");
                        let fallback = self
                            .finalize_tx(
                                live,
                                Terminal::Failed { error_code: "message_persistence_failed".into() },
                                None,
                                usage,
                                response_id,
                                Some(format!("{e}")),
                                *counters,
                                latency,
                            )
                            .await;
                        let (code, message) = match fallback {
                            Ok(_) => ("message_persistence_failed", "The answer could not be saved"),
                            Err(e2) if is_cas_lost(&e2) => ("stream_interrupted", "The stream was interrupted"),
                            Err(e2) => {
                                tracing::error!(error = %e2, request_id = %live.request_id, "finalization failed");
                                ("finalization_failed", "The turn could not be finalized")
                            }
                        };
                        drop(tx.send(SseEvent::Error { code: code.into(), message: message.into() }).await);
                    }
                }
            }
            Outcome::Failed { code, message, usage } => {
                let res = self
                    .finalize_tx(
                        live,
                        Terminal::Failed { error_code: code.clone() },
                        None,
                        usage,
                        None,
                        Some(message.clone()),
                        *counters,
                        latency,
                    )
                    .await;
                let (code, message) = match res {
                    Err(e) if is_cas_lost(&e) => ("stream_interrupted".to_owned(), "The stream was interrupted".to_owned()),
                    Err(e) => {
                        tracing::error!(error = %e, request_id = %live.request_id, "failed-turn finalization failed");
                        (code, message)
                    }
                    Ok(_) => (code, message),
                };
                drop(tx.send(SseEvent::Error { code, message }).await);
            }
            Outcome::Cancelled { text } => {
                let persist = (!text.is_empty()).then_some(text);
                let had_text = persist.is_some();
                let res = self
                    .finalize_tx(live, Terminal::Cancelled, persist, None, None, None, *counters, latency.clone())
                    .await;
                if let Err(e) = res
                    && had_text && !is_cas_lost(&e)
                {
                    tracing::warn!(error = %e, "persisting partial answer failed; finalizing cancelled without it");
                    drop(self.finalize_tx(live, Terminal::Cancelled, None, None, None, None, *counters, latency).await);
                }
            }
        }
    }

    /// Mark an unstarted retry/edit turn failed (no reserve, no events).
    pub(crate) async fn fail_unstarted_turn(&self, tenant_id: Uuid, turn_id: Uuid, error_code: &str) {
        let scope = AccessScope::for_tenant(tenant_id);
        let ts = now();
        if let Ok(conn) = self.db.conn() {
            drop(
                chat_turns::Entity::update_many()
                    .col_expr(chat_turns::Column::State, Expr::value("failed"))
                    .col_expr(chat_turns::Column::ErrorCode, Expr::value(error_code))
                    .col_expr(chat_turns::Column::CompletedAt, Expr::value(ts))
                    .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                    .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)).add(chat_turns::Column::State.eq("running")))
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await,
            );
        }
    }
}
