//! Turn finalization (DESIGN §5.7–5.9): CAS on `state = 'running'`, quota
//! settlement and outbox enqueue (usage, audit, thread summary) in one
//! transaction. Terminal SSE events are sent only after the commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mini_chat_sdk::{
    AuditEvent, AuditLatency, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, ModelTier,
    TurnAuditEvent, UsageEvent, UsageTokens,
};
use sea_orm::ActiveValue::Set;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use super::stream::{TurnSetup, new_message};
use crate::domain::clock;
use crate::domain::context::{ContextPlan, summary_trigger};
use crate::domain::error::DomainError;
use crate::domain::models::{DoneData, DoneUsage};
use crate::domain::quota::{SettlementMethod, TurnReserve, warnings_from_status};
use crate::infra::db::repo;
use crate::infra::db::repo::turns::{TerminalUpdate, state};
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// Converts a chrono timestamp to `time::OffsetDateTime`.
#[must_use]
pub fn to_time(ts: chrono::DateTime<chrono::Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ts.timestamp_nanos_opt().unwrap_or(0)))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

/// `{tenant}/{turn}/{request}` in simple UUID form.
#[must_use]
pub fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!("{}/{}/{}", tenant_id.simple(), turn_id.simple(), request_id.simple())
}

/// Identity and reserve of a turn being finalized.
#[derive(Debug, Clone)]
pub(crate) struct TurnContext {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub downgrade: bool,
    pub downgrade_reason: Option<&'static str>,
    pub policy_version: u64,
    pub reserve: Option<TurnReserve>,
    pub requester_type: &'static str,
}

impl TurnContext {
    pub(crate) fn from_setup(ctx: &SecurityContext, s: &TurnSetup, message_id: Uuid) -> Self {
        let d = &s.decision;
        Self {
            tenant_id: s.chat.tenant_id,
            user_id: ctx.subject_id(),
            chat_id: s.chat.id,
            turn_id: s.turn_id,
            request_id: s.request_id,
            message_id,
            selected_model: s.chat.model.clone(),
            effective_model: d.effective.id.clone(),
            downgrade: d.downgrade,
            downgrade_reason: d.downgrade_reason,
            policy_version: d.policy_version,
            reserve: Some(TurnReserve {
                tenant_id: s.chat.tenant_id,
                user_id: ctx.subject_id(),
                reserve_tokens: d.reserve_tokens,
                max_output_tokens_applied: d.max_output_tokens_applied,
                reserved_credits_micro: d.reserved_credits_micro,
                minimal_generation_floor_applied: d.minimal_generation_floor_applied,
                premium: d.effective.tier == ModelTier::Premium,
                in_mult: d.effective.input_tokens_credit_multiplier_micro,
                out_mult: d.effective.output_tokens_credit_multiplier_micro,
                daily_start: d.daily_start,
                monthly_start: d.monthly_start,
            }),
            requester_type: "user",
        }
    }
}

/// Stream results passed to finalization.
#[derive(Debug, Clone)]
pub(crate) struct FinalizeInput {
    pub text: String,
    pub counters: (i32, i32, i32),
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub plan: Option<ContextPlan>,
    pub has_summary: bool,
}

fn usage_known(u: Option<UsageTokens>) -> Option<UsageTokens> {
    u.filter(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Builds the usage event of a turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn usage_event(
    t: &TurnContext,
    terminal_state: &str,
    billing_outcome: &str,
    method: SettlementMethod,
    credits: i64,
    counters: (i32, i32, i32),
    usage: Option<UsageTokens>,
) -> UsageEvent {
    let to_u32 = |v: i32| u32::try_from(v).unwrap_or(0);
    UsageEvent {
        tenant_id: t.tenant_id,
        user_id: Some(t.user_id),
        chat_id: t.chat_id,
        turn_id: Some(t.turn_id),
        request_id: t.request_id,
        effective_model: t.effective_model.clone(),
        selected_model: t.selected_model.clone(),
        terminal_state: terminal_state.to_owned(),
        billing_outcome: billing_outcome.to_owned(),
        usage,
        actual_credits_micro: credits,
        settlement_method: method.as_str().to_owned(),
        policy_version_applied: t.policy_version,
        web_search_calls: to_u32(counters.0),
        code_interpreter_calls: to_u32(counters.1),
        file_search_calls: to_u32(counters.2),
        timestamp: to_time(clock::now()),
        requester_type: t.requester_type.to_owned(),
        dedupe_key: dedupe_key(t.tenant_id, t.turn_id, t.request_id),
        system_task_type: None,
    }
}

/// Builds the turn audit event.
pub(crate) fn turn_audit(
    t: &TurnContext,
    terminal_state: &str,
    error_code: Option<&str>,
    usage: Option<UsageTokens>,
    input: &FinalizeInput,
    quota_decision: &str,
) -> AuditEvent {
    let to_u32 = |v: i32| u32::try_from(v).unwrap_or(0);
    AuditEvent::Turn(TurnAuditEvent {
        event_type: if terminal_state == state::COMPLETED {
            "turn_completed"
        } else {
            "turn_failed"
        }
        .to_owned(),
        timestamp: to_time(clock::now()),
        tenant_id: t.tenant_id,
        requester_type: t.requester_type.to_owned(),
        user_id: Some(t.user_id),
        chat_id: t.chat_id,
        turn_id: t.turn_id,
        request_id: t.request_id,
        selected_model: t.selected_model.clone(),
        effective_model: t.effective_model.clone(),
        terminal_state: terminal_state.to_owned(),
        error_code: error_code.map(ToOwned::to_owned),
        usage,
        latency: AuditLatency {
            ttft_ms: input.ttft_ms,
            total_ms: input.total_ms,
        },
        tool_calls: AuditToolCalls {
            web_search_calls: to_u32(input.counters.0),
            file_search_calls: to_u32(input.counters.2),
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: quota_decision.to_owned(),
                downgrade_from: t.downgrade.then(|| t.selected_model.clone()),
                downgrade_reason: t.downgrade_reason.map(ToOwned::to_owned),
            },
            license: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        quota_scope: None,
        trace_id: None,
    })
}

impl MiniChatService {
    /// Resolves the multipliers of the effective model in the snapshot of the
    /// applied policy version (falls back to the preflight values).
    async fn settlement_reserve(&self, t: &TurnContext) -> Option<TurnReserve> {
        let mut r = t.reserve.clone()?;
        if let Ok(snap) = self.policy.snapshot_version(t.user_id, t.policy_version).await
            && let Some(m) = snap.find(&t.effective_model)
        {
            r.in_mult = m.input_tokens_credit_multiplier_micro;
            r.out_mult = m.output_tokens_credit_multiplier_micro;
            r.premium = m.tier == ModelTier::Premium;
        }
        Some(r)
    }

    /// Settles quota and enqueues usage + audit events; shared step.
    #[allow(clippy::too_many_arguments)]
    async fn settle_and_enqueue(
        svc: &Arc<Self>,
        tx: &DbTx<'_>,
        t: &TurnContext,
        reserve: Option<&TurnReserve>,
        terminal_state: &str,
        billing_outcome: &str,
        method: SettlementMethod,
        error_code: Option<&str>,
        input: &FinalizeInput,
        usage: Option<UsageTokens>,
    ) -> Result<Wake, DomainError> {
        let now = clock::now();
        let (ws, ci, _) = input.counters;
        let (credits, method) = match reserve {
            Some(r) => {
                let res = svc.quota.settle(tx, r, method, ws, ci, now).await?;
                (res.committed_credits_micro, method)
            }
            None => (0, SettlementMethod::Estimated),
        };
        let ev = usage_event(t, terminal_state, billing_outcome, method, credits, input.counters, usage);
        let mut wake = svc.outbox.usage(tx, &ev).await?;
        let decision = if t.downgrade { "downgrade" } else { "allow" };
        let audit = turn_audit(t, terminal_state, error_code, usage, input, decision);
        wake += svc.outbox.audit(tx, t.tenant_id, &audit).await?;
        Ok(wake)
    }

    /// Finalizes a completed (or incomplete) stream.
    ///
    /// Returns `Ok(Some(done))` when committed, `Ok(None)` when the CAS was
    /// lost, `Err(code)` with the SSE error code otherwise.
    pub(crate) async fn finalize_completed(
        self: &Arc<Self>,
        t: &TurnContext,
        input: FinalizeInput,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
    ) -> Result<Option<DoneData>, &'static str> {
        let started = std::time::Instant::now();
        let reserve = self.settlement_reserve(t).await;
        let svc = Arc::clone(self);
        let tt = t.clone();
        let inp = input.clone();
        let inserted = Arc::new(AtomicBool::new(false));
        let ins = Arc::clone(&inserted);
        let summary_enabled = self.cfg.thread_summary_worker.enabled;
        let threshold = self.cfg.thread_summary_worker.compression_threshold_pct;
        let res: Result<(bool, Option<bool>), DomainError> = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let u = usage.unwrap_or_default();
                    let mut am = new_message(
                        tt.message_id,
                        tt.tenant_id,
                        tt.chat_id,
                        tt.request_id,
                        "assistant",
                        &inp.text,
                        Some(tt.effective_model.clone()),
                        now,
                    );
                    am.input_tokens = Set(u.input_tokens);
                    am.output_tokens = Set(u.output_tokens);
                    am.cache_read_input_tokens = Set(u.cache_read_input_tokens);
                    am.cache_write_input_tokens = Set(u.cache_write_input_tokens);
                    am.reasoning_tokens = Set(u.reasoning_tokens);
                    am.provider_response_id = Set(response_id.clone());
                    repo::messages::insert(tx, tt.tenant_id, am).await?;
                    ins.store(true, Ordering::SeqCst);
                    let won = repo::turns::cas_finalize(
                        tx,
                        tt.tenant_id,
                        tt.turn_id,
                        &TerminalUpdate {
                            state: state::COMPLETED,
                            assistant_message_id: Some(tt.message_id),
                            provider_response_id: response_id.clone(),
                            web_search_completed: Some(inp.counters.0),
                            code_interpreter_completed: Some(inp.counters.1),
                            file_search_completed: Some(inp.counters.2),
                            ..TerminalUpdate::default()
                        },
                        now,
                    )
                    .await?;
                    if won == 0 {
                        return Err(DomainError::Internal("CAS lost".to_owned()));
                    }
                    let mut wake = Self::settle_and_enqueue(
                        &svc,
                        tx,
                        &tt,
                        reserve.as_ref(),
                        state::COMPLETED,
                        "completed",
                        SettlementMethod::Actual(u),
                        None,
                        &inp,
                        usage,
                    )
                    .await?;
                    let scheduled = if summary_enabled
                        && let Some(plan) = &inp.plan
                        && summary_trigger(plan, inp.has_summary, threshold)
                    {
                        let (w, s) = svc.schedule_summary(tx, &tt).await?;
                        wake += w;
                        Some(s)
                    } else {
                        None
                    };
                    Ok(((true, scheduled), wake))
                })
            })
            .await;
        self.metrics
            .record("finalization_latency_ms", started.elapsed().as_secs_f64() * 1000.0, &[]);
        match res {
            Ok((_, scheduled)) => {
                if let Some(s) = scheduled {
                    self.metrics.inc(
                        "thread_summary_trigger",
                        1,
                        &[("result", if s { "scheduled" } else { "not_needed" }.to_owned())],
                    );
                }
                let warnings = self.current_warnings(t).await;
                let u = usage.unwrap_or_default();
                Ok(Some(DoneData {
                    usage: DoneUsage {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                    },
                    effective_model: t.effective_model.clone(),
                    selected_model: t.selected_model.clone(),
                    quota_decision: if t.downgrade { "downgrade" } else { "allow" },
                    downgrade_from: t.downgrade.then(|| t.selected_model.clone()),
                    downgrade_reason: t.downgrade_reason.map(ToOwned::to_owned),
                    quota_warnings: Some(warnings),
                }))
            }
            Err(DomainError::Internal(m)) if m == "CAS lost" => Ok(None),
            Err(e) => {
                tracing::error!(error = %e, turn_id = %t.turn_id, "finalization of a completed stream failed");
                if inserted.load(Ordering::SeqCst) {
                    return Err("finalization_failed");
                }
                // The assistant message could not be persisted: finalize as failed.
                match self
                    .finalize_failed(t, input, "message_persistence_failed", "message persistence failed", usage)
                    .await
                {
                    Some(true) => Err("message_persistence_failed"),
                    _ => Err("finalization_failed"),
                }
            }
        }
    }

    async fn current_warnings(&self, t: &TurnContext) -> Vec<crate::domain::models::QuotaWarning> {
        let Ok(limits) = self.policy.user_limits(t.user_id, t.policy_version).await else {
            return Vec::new();
        };
        let Ok(conn) = self.db.conn() else {
            return Vec::new();
        };
        match self
            .quota
            .status(&conn, t.tenant_id, t.user_id, &limits, clock::now())
            .await
        {
            Ok(st) => warnings_from_status(&st),
            Err(_) => Vec::new(),
        }
    }

    /// Finalizes a failed stream. Returns `Some(true)` when the CAS won,
    /// `Some(false)` when lost, `None` on a transaction failure.
    pub(crate) async fn finalize_failed(
        self: &Arc<Self>,
        t: &TurnContext,
        input: FinalizeInput,
        code: &str,
        message: &str,
        usage: Option<UsageTokens>,
    ) -> Option<bool> {
        let reserve = self.settlement_reserve(t).await;
        let svc = Arc::clone(self);
        let tt = t.clone();
        let code_s = code.to_owned();
        let msg_s = message.to_owned();
        let res: Result<bool, DomainError> = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let won = repo::turns::cas_finalize(
                        tx,
                        tt.tenant_id,
                        tt.turn_id,
                        &TerminalUpdate {
                            state: state::FAILED,
                            error_code: Some(code_s.clone()),
                            error_detail: Some(msg_s.clone()),
                            web_search_completed: Some(input.counters.0),
                            code_interpreter_completed: Some(input.counters.1),
                            file_search_completed: Some(input.counters.2),
                            ..TerminalUpdate::default()
                        },
                        now,
                    )
                    .await?;
                    if won == 0 {
                        return Ok((false, Wake::empty()));
                    }
                    let known = usage_known(usage);
                    let method = known.map_or(SettlementMethod::Estimated, SettlementMethod::Actual);
                    let wake = Self::settle_and_enqueue(
                        &svc,
                        tx,
                        &tt,
                        reserve.as_ref(),
                        state::FAILED,
                        "failed",
                        method,
                        Some(&code_s),
                        &input,
                        known,
                    )
                    .await?;
                    Ok((true, wake))
                })
            })
            .await;
        match res {
            Ok(won) => Some(won),
            Err(e) => {
                tracing::error!(error = %e, turn_id = %t.turn_id, "finalization of a failed stream failed");
                None
            }
        }
    }

    /// Finalizes a cancelled stream (client disconnect): partial text is
    /// persisted when non-empty; estimated settlement.
    pub(crate) async fn finalize_cancelled(self: &Arc<Self>, t: &TurnContext, input: FinalizeInput) {
        let reserve = self.settlement_reserve(t).await;
        for with_message in [!input.text.is_empty(), false] {
            let svc = Arc::clone(self);
            let tt = t.clone();
            let inp = input.clone();
            let reserve = reserve.clone();
            let res: Result<bool, DomainError> = self
                .transact(move |tx| {
                    Box::pin(async move {
                        let now = clock::now();
                        if with_message {
                            let am = new_message(
                                tt.message_id,
                                tt.tenant_id,
                                tt.chat_id,
                                tt.request_id,
                                "assistant",
                                &inp.text,
                                Some(tt.effective_model.clone()),
                                now,
                            );
                            repo::messages::insert(tx, tt.tenant_id, am).await?;
                        }
                        let won = repo::turns::cas_finalize(
                            tx,
                            tt.tenant_id,
                            tt.turn_id,
                            &TerminalUpdate {
                                state: state::CANCELLED,
                                assistant_message_id: with_message.then_some(tt.message_id),
                                web_search_completed: Some(inp.counters.0),
                                code_interpreter_completed: Some(inp.counters.1),
                                file_search_completed: Some(inp.counters.2),
                                ..TerminalUpdate::default()
                            },
                            now,
                        )
                        .await?;
                        if won == 0 {
                            return Err(DomainError::Internal("CAS lost".to_owned()));
                        }
                        let wake = Self::settle_and_enqueue(
                            &svc,
                            tx,
                            &tt,
                            reserve.as_ref(),
                            state::CANCELLED,
                            "aborted",
                            SettlementMethod::Estimated,
                            None,
                            &inp,
                            None,
                        )
                        .await?;
                        Ok((true, wake))
                    })
                })
                .await;
            match res {
                Ok(_) => {
                    self.metrics.inc("cancel_effective", 1, &[("trigger", "disconnect".to_owned())]);
                    self.metrics.inc("streams_aborted", 1, &[("trigger", "client_disconnect".to_owned())]);
                    return;
                }
                Err(DomainError::Internal(m)) if m == "CAS lost" => return,
                Err(e) => {
                    tracing::warn!(error = %e, turn_id = %t.turn_id, with_message, "cancel finalization failed");
                }
            }
        }
    }

    /// Enqueues a thread summary task when a frozen target exists.
    /// Returns whether work was scheduled.
    async fn schedule_summary(self: &Arc<Self>, tx: &DbTx<'_>, t: &TurnContext) -> Result<(Wake, bool), DomainError> {
        let target = repo::messages::latest_excluding_request(tx, t.tenant_id, t.chat_id, t.request_id).await?;
        let Some(target) = target else {
            return Ok((Wake::empty(), false));
        };
        let base = repo::summaries::find(tx, t.tenant_id, t.chat_id).await?;
        if let Some(b) = &base
            && b.summarized_up_to_message_id == target.id
        {
            return Ok((Wake::empty(), false));
        }
        let payload = ThreadSummaryPayload {
            tenant_id: t.tenant_id,
            chat_id: t.chat_id,
            system_request_id: Uuid::new_v4(),
            base_frontier_created_at: base.as_ref().map(|b| b.summarized_up_to_created_at),
            base_frontier_message_id: base.as_ref().map(|b| b.summarized_up_to_message_id),
            frozen_target_created_at: target.created_at,
            frozen_target_message_id: target.id,
            system_task_type: "thread_summary_update".to_owned(),
        };
        let wake = self.outbox.thread_summary(tx, &payload).await?;
        Ok((wake, true))
    }
}
