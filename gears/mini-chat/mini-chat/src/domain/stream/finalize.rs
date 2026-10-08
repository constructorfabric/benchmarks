//! Turn finalization contract (DESIGN §5.7): one transaction with the CAS guard,
//! the assistant message, quota settlement and the usage / audit / thread-summary
//! outbox messages.

use std::sync::Arc;

use mini_chat_sdk::{
    LatencyMs, MiniChatAuditEvent, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_db::DbTx;
use uuid::Uuid;

use super::TurnRuntime;
use crate::domain::app::AppServices;
use crate::domain::credits::{self, BillingOutcome, SettlementMethod, TurnReserve};
use crate::domain::error::{DomainError, DomainResult, retry_contention};
use crate::domain::quota::{self, QuotaDecision, SettleInput};
use crate::domain::time::now;
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repo;
use crate::infra::outbox::{OutboxEnqueuer, ThreadSummaryPayload, Wakes};

const MESSAGE_INSERT_MARKER: &str = "assistant message insert failed";

/// Tool counters of a turn.
// The `_done` suffix marks completed-call counts (as opposed to requested/enabled tools).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounters {
    pub web_search_done: u32,
    pub code_interpreter_done: u32,
    pub file_search_done: u32,
    /// `search_knowledge` calls (counted before the retrieval runs).
    pub knowledge_calls: u32,
}

impl ToolCounters {
    /// `file_search_calls` of the usage and audit events: the in-memory knowledge
    /// call count when knowledge search ran, else the provider `file_search` calls.
    #[must_use]
    pub fn file_search_calls(&self) -> u32 {
        if self.knowledge_calls > 0 {
            self.knowledge_calls
        } else {
            self.file_search_done
        }
    }
}

/// Terminal outcome of the provider task.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `completed`, `failed` or `cancelled`.
    pub state: &'static str,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub text: String,
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
    pub counters: ToolCounters,
    pub ttft_ms: Option<u64>,
}

/// Result of the finalization.
#[derive(Debug)]
pub enum FinalizeResult {
    /// The CAS was won and committed with this state and error code.
    Committed {
        state: String,
        error_code: Option<String>,
    },
    /// Another finalizer already moved the turn out of `running`.
    Lost,
    /// The transaction failed; the turn stays `running`.
    Failed(DomainError),
}

fn usage_known(u: Option<&UsageTokens>) -> bool {
    u.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Dedupe key of a turn usage event.
#[must_use]
pub fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}

struct Plan {
    state: &'static str,
    error_code: Option<String>,
    error_detail: Option<String>,
    persist_message: bool,
    method: SettlementMethod,
    billing: BillingOutcome,
    settlement: credits::Settlement,
}

impl AppServices {
    fn plan(
        &self,
        rt: &TurnRuntime,
        o: &Outcome,
        mults: (i64, i64),
        persist: bool,
    ) -> DomainResult<Plan> {
        let (billing, method) = credits::derive_billing(
            o.state,
            o.error_code.as_deref(),
            usage_known(o.usage.as_ref()),
        );
        let d = &rt.decision;
        let reserve = TurnReserve {
            reserve_tokens: d.reserve_tokens,
            max_output_tokens_applied: d.max_output_tokens_applied,
            reserved_credits_micro: d.reserved_credits_micro,
            minimal_generation_floor_applied: d.minimal_generation_floor_applied,
        };
        let actual = o.usage.map(|u| (u.input_tokens, u.output_tokens));
        let settlement = credits::settle(
            method,
            &reserve,
            actual,
            mults.0,
            mults.1,
            self.cfg.quota.overshoot_tolerance_factor,
        )
        .map_err(|e| DomainError::internal(format!("settlement credit computation failed: {e}")))?;
        Ok(Plan {
            state: o.state,
            error_code: o.error_code.clone(),
            error_detail: o.error_detail.clone(),
            persist_message: persist,
            method,
            billing,
            settlement,
        })
    }

    /// Finalizes a turn; exactly one finalizer wins the CAS on `state = 'running'`.
    // Single linear CAS + settlement + event sequence; splitting it would obscure the ordering.
    #[allow(clippy::cognitive_complexity)]
    pub async fn finalize_turn(self: &Arc<Self>, rt: &TurnRuntime, o: Outcome) -> FinalizeResult {
        let started = std::time::Instant::now();
        // Multipliers and tier from the snapshot of the applied policy version.
        let eff = &rt.decision.effective;
        let (mults, premium) = match self
            .policy
            .snapshot(rt.user_id, rt.decision.policy_version)
            .await
        {
            Ok(s) => match s.find(&eff.id) {
                Some(m) => (
                    (
                        m.input_tokens_credit_multiplier_micro,
                        m.output_tokens_credit_multiplier_micro,
                    ),
                    m.tier == ModelTier::Premium,
                ),
                None => (
                    (
                        eff.input_tokens_credit_multiplier_micro,
                        eff.output_tokens_credit_multiplier_micro,
                    ),
                    rt.decision.premium,
                ),
            },
            Err(e) => {
                tracing::warn!(error = %e, "policy snapshot unavailable at settlement; using preflight entry");
                (
                    (
                        eff.input_tokens_credit_multiplier_micro,
                        eff.output_tokens_credit_multiplier_micro,
                    ),
                    rt.decision.premium,
                )
            }
        };
        let persist = o.state == "completed" || (o.state == "cancelled" && !o.text.is_empty());
        let plan = match self.plan(rt, &o, mults, persist) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, turn_id = %rt.turn_id, "settlement failed; turn left running");
                return FinalizeResult::Failed(e);
            }
        };
        let res = self.finalize_tx(rt, &o, &plan, premium).await;
        let res = match res {
            Err(e) if e.to_string().contains(MESSAGE_INSERT_MARKER) => {
                tracing::error!(error = %e, turn_id = %rt.turn_id, "assistant message persistence failed");
                let mut o2 = o.clone();
                if o.state == "completed" {
                    o2.state = "failed";
                    o2.error_code = Some("message_persistence_failed".to_owned());
                }
                match self.plan(rt, &o2, mults, false) {
                    Ok(p2) => self.finalize_tx(rt, &o2, &p2, premium).await,
                    Err(e) => Err(e),
                }
            }
            other => other,
        };
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "finalization_latency_ms",
            started.elapsed().as_secs_f64() * 1000.0,
            &[],
        );
        match res {
            Ok(Some((state, code))) => FinalizeResult::Committed {
                state,
                error_code: code,
            },
            Ok(None) => FinalizeResult::Lost,
            Err(e) => {
                tracing::error!(error = %e, turn_id = %rt.turn_id, "turn finalization failed");
                FinalizeResult::Failed(e)
            }
        }
    }

    // One finalization transaction with all its outbox side effects.
    #[allow(clippy::too_many_lines)]
    async fn finalize_tx(
        self: &Arc<Self>,
        rt: &TurnRuntime,
        o: &Outcome,
        plan: &Plan,
        premium: bool,
    ) -> DomainResult<Option<(String, Option<String>)>> {
        let svc = Arc::clone(self);
        let rt = rt.clone();
        let o = o.clone();
        let state = plan.state;
        let error_code = plan.error_code.clone();
        let error_detail = plan.error_detail.clone();
        let persist = plan.persist_message;
        let method = plan.method;
        let billing = plan.billing;
        let settlement = plan.settlement;
        let res = retry_contention(|| {
            let svc = Arc::clone(&svc);
            let rt = rt.clone();
            let o = o.clone();
            let error_code = error_code.clone();
            let error_detail = error_detail.clone();
            async move {
                let outbox = Arc::clone(&svc.outbox);
                let cfg = Arc::clone(&svc.cfg);
                svc.db
                    .transaction(move |tx| {
                        Box::pin(async move {
                            let ts = now();
                            let mut cols = vec![
                                (chat_turn::Column::State, Expr::value(state)),
                                (chat_turn::Column::CompletedAt, Expr::value(ts)),
                                (chat_turn::Column::UpdatedAt, Expr::value(ts)),
                                (
                                    chat_turn::Column::WebSearchCompletedCount,
                                    Expr::value(
                                        i32::try_from(o.counters.web_search_done)
                                            .unwrap_or(i32::MAX),
                                    ),
                                ),
                                (
                                    chat_turn::Column::CodeInterpreterCompletedCount,
                                    Expr::value(
                                        i32::try_from(o.counters.code_interpreter_done)
                                            .unwrap_or(i32::MAX),
                                    ),
                                ),
                                (
                                    chat_turn::Column::FileSearchCompletedCount,
                                    Expr::value(
                                        i32::try_from(o.counters.file_search_done)
                                            .unwrap_or(i32::MAX),
                                    ),
                                ),
                            ];
                            if let Some(c) = &error_code
                                && state == "failed"
                            {
                                cols.push((chat_turn::Column::ErrorCode, Expr::value(c.clone())));
                            }
                            if let Some(d) = &error_detail {
                                cols.push((
                                    chat_turn::Column::ErrorDetail,
                                    Expr::value(d.chars().take(1000).collect::<String>()),
                                ));
                            }
                            if let Some(r) = &o.response_id {
                                cols.push((
                                    chat_turn::Column::ProviderResponseId,
                                    Expr::value(r.clone()),
                                ));
                            }
                            if persist {
                                cols.push((
                                    chat_turn::Column::AssistantMessageId,
                                    Expr::value(rt.message_id),
                                ));
                            }
                            let won = repo::update_turn_where(
                                tx,
                                rt.tenant_id,
                                rt.turn_id,
                                Condition::all().add(chat_turn::Column::State.eq("running")),
                                cols,
                            )
                            .await?;
                            if won == 0 {
                                return Ok(None);
                            }
                            if persist {
                                let mut m = repo::message_model(
                                    rt.message_id,
                                    rt.tenant_id,
                                    rt.chat_id,
                                    rt.request_id,
                                    "assistant",
                                    o.text.clone(),
                                    ts,
                                );
                                let u = o.usage.unwrap_or_default();
                                m.input_tokens = u.input_tokens.max(0);
                                m.output_tokens = u.output_tokens.max(0);
                                m.cache_read_input_tokens = u.cache_read_input_tokens.max(0);
                                m.cache_write_input_tokens = u.cache_write_input_tokens.max(0);
                                m.reasoning_tokens = u.reasoning_tokens.max(0);
                                m.model = Some(rt.decision.effective.id.clone());
                                m.provider_response_id.clone_from(&o.response_id);
                                repo::insert_message(tx, m).await.map_err(|e| {
                                    DomainError::internal(format!("{MESSAGE_INSERT_MARKER}: {e}"))
                                })?;
                            }
                            quota::apply_settlement(
                                tx,
                                &SettleInput {
                                    tenant_id: rt.tenant_id,
                                    user_id: rt.user_id,
                                    periods: rt.decision.periods,
                                    premium,
                                    reserved_credits_micro: rt.decision.reserved_credits_micro,
                                    settlement,
                                    web_search_calls: i64::from(o.counters.web_search_done),
                                    code_interpreter_calls: i64::from(
                                        o.counters.code_interpreter_done,
                                    ),
                                },
                                ts,
                            )
                            .await?;
                            let mut wakes = Wakes::default();
                            let usage_payload = match method {
                                SettlementMethod::Actual => {
                                    o.usage.or(Some(UsageTokens::default()))
                                }
                                SettlementMethod::Released => Some(UsageTokens::default()),
                                SettlementMethod::Estimated => None,
                            };
                            let ev = UsageEvent {
                                tenant_id: rt.tenant_id,
                                user_id: Some(rt.user_id),
                                chat_id: rt.chat_id,
                                turn_id: Some(rt.turn_id),
                                request_id: rt.request_id,
                                effective_model: rt.decision.effective.id.clone(),
                                selected_model: rt.selected_model.clone(),
                                terminal_state: state.to_owned(),
                                billing_outcome: billing.as_str().to_owned(),
                                usage: usage_payload,
                                actual_credits_micro: settlement.committed_credits_micro,
                                settlement_method: method.as_str().to_owned(),
                                policy_version_applied: rt.decision.policy_version,
                                web_search_calls: o.counters.web_search_done,
                                code_interpreter_calls: o.counters.code_interpreter_done,
                                file_search_calls: o.counters.file_search_calls(),
                                timestamp: ts,
                                requester_type: "user".to_owned(),
                                dedupe_key: dedupe_key(rt.tenant_id, rt.turn_id, rt.request_id),
                                system_task_type: None,
                            };
                            wakes.push(
                                outbox
                                    .usage(tx, &ev)
                                    .await
                                    .map_err(crate::domain::turns::internal_payload)?,
                            );
                            let audit = turn_audit_event(&rt, &o, state, error_code.as_deref(), ts);
                            wakes.push(
                                outbox
                                    .audit(tx, &audit)
                                    .await
                                    .map_err(crate::domain::turns::internal_payload)?,
                            );
                            let summary_label = if state == "completed" {
                                match schedule_summary(tx, &outbox, &cfg, &rt).await? {
                                    Some((w, label)) => {
                                        wakes.push(w);
                                        Some(label)
                                    }
                                    None => None,
                                }
                            } else {
                                None
                            };
                            Ok(Some((
                                wakes,
                                state.to_owned(),
                                error_code.clone(),
                                summary_label,
                            )))
                        })
                    })
                    .await
            }
        })
        .await;
        match res {
            Ok(Some((wakes, state, code, summary_label))) => {
                wakes.fire();
                if let Some(label) = summary_label {
                    self.metrics
                        .inc("thread_summary_trigger", &[("result", label)]);
                } else if state == "completed" && rt_trigger_evaluated(&rt) {
                    self.metrics
                        .inc("thread_summary_trigger", &[("result", "not_needed")]);
                }
                if method == SettlementMethod::Actual {
                    for p in ["daily", "monthly"] {
                        self.metrics.inc("quota_commit", &[("period", p)]);
                        if settlement.overshoot {
                            self.metrics.inc("quota_overshoot", &[("period", p)]);
                        }
                    }
                }
                Ok(Some((state, code)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn rt_trigger_evaluated(rt: &TurnRuntime) -> bool {
    let t = rt.summary_trigger;
    t.evaluate && (t.truncated || !t.has_summary)
}

/// Builds the turn audit event.
fn turn_audit_event(
    rt: &TurnRuntime,
    o: &Outcome,
    state: &str,
    error_code: Option<&str>,
    ts: time::OffsetDateTime,
) -> MiniChatAuditEvent {
    let d = &rt.decision;
    let downgrade = d.decision == QuotaDecision::Downgrade;
    MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: if state == "completed" {
            "turn_completed"
        } else {
            "turn_failed"
        }
        .to_owned(),
        timestamp: ts,
        tenant_id: rt.tenant_id,
        requester_type: "user".to_owned(),
        requester_user_id: Some(rt.user_id),
        chat_id: rt.chat_id,
        turn_id: rt.turn_id,
        request_id: rt.request_id,
        selected_model: rt.selected_model.clone(),
        effective_model: d.effective.id.clone(),
        terminal_state: state.to_owned(),
        error_code: if state == "failed" {
            error_code.map(str::to_owned)
        } else {
            None
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        usage: o.usage.unwrap_or_default(),
        latency_ms: LatencyMs {
            ttft_ms: o.ttft_ms,
            total_ms: u64::try_from(rt.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
        tool_calls: ToolCalls {
            web_search_calls: o.counters.web_search_done,
            file_search_calls: o.counters.file_search_calls(),
        },
        policy_decisions: PolicyDecisions {
            license: None,
            quota: QuotaPolicyDecision {
                decision: if downgrade { "downgrade" } else { "allow" }.to_owned(),
                quota_scope: None,
                downgrade_from: downgrade.then(|| rt.selected_model.clone()),
                downgrade_reason: if downgrade {
                    d.downgrade_reason.clone()
                } else {
                    None
                },
            },
        },
        trace_id: None,
    })
}

/// Evaluates the thread-summary trigger and enqueues the work item (DESIGN §3.6).
async fn schedule_summary(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    cfg: &crate::config::MiniChatConfig,
    rt: &TurnRuntime,
) -> DomainResult<Option<(toolkit_db::outbox::Wake, &'static str)>> {
    let t = rt.summary_trigger;
    if !t.evaluate {
        return Ok(None);
    }
    let pct = i64::from(cfg.thread_summary_worker.compression_threshold_pct);
    let proactive = !t.has_summary
        && t.effective_budget > 0
        && t.assembled_tokens * 100 >= t.effective_budget * pct;
    if !(proactive || t.truncated) {
        return Ok(None);
    }
    let Some(target) =
        repo::latest_message(tx, rt.tenant_id, rt.chat_id, Some(rt.request_id)).await?
    else {
        return Ok(None);
    };
    let summary = repo::find_summary(tx, rt.tenant_id, rt.chat_id).await?;
    let base = summary
        .as_ref()
        .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
    if base == Some((target.created_at, target.id)) {
        return Ok(None);
    }
    let payload = ThreadSummaryPayload {
        tenant_id: rt.tenant_id,
        chat_id: rt.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.map(|b| b.0),
        base_frontier_message_id: base.map(|b| b.1),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".to_owned(),
    };
    let w = outbox
        .thread_summary(tx, &payload)
        .await
        .map_err(crate::domain::turns::internal_payload)?;
    Ok(Some((w, "scheduled")))
}
