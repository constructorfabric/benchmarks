//! Turn finalization (DESIGN §5.7): CAS on `state = 'running'`, settlement and outbox
//! emission in one transaction; the terminal SSE event is sent only after commit.

use std::collections::HashMap;
use std::time::Instant;

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent, ModelCatalogEntry,
    TurnAuditEvent, UsageEvent, UsageTokens, UserLimits,
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::infra::db::WriteTransaction as _;
use crate::domain::credits::{Reserve, ToolFlags, credits_micro_checked, estimated_settlement_credits, mult};
use crate::domain::error::DomainError;
use crate::domain::events::{Done, DoneUsage, QuotaWarning, StreamEvent};
use crate::domain::quota::{self, PeriodStarts, QuotaDecision};
use crate::domain::repo::{self, state};
use crate::domain::service::Svc;
use crate::infra::db::entities::{chat_turns, messages};
use crate::infra::db::now;
use crate::infra::llm::LlmRequest;
use crate::infra::llm::resolver::ChatTarget;
use crate::infra::outbox::{Enqueuer, ThreadSummaryTask, turn_dedupe_key};

/// Everything the provider task and finalization need.
#[derive(Debug, Clone)]
pub struct TurnCtx {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub assistant_message_id: Uuid,
    pub user_message_id: Uuid,
    pub user_message_created_at: OffsetDateTime,
    pub selected_model: String,
    pub effective: ModelCatalogEntry,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<&'static str>,
    pub reserve: Reserve,
    pub policy_version: u64,
    pub limits: UserLimits,
    pub starts: PeriodStarts,
    pub floor_applied: i64,
    pub tools: ToolFlags,
    pub target: ChatTarget,
    pub request: LlmRequest,
    pub citations: HashMap<String, (Uuid, String)>,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    pub has_summary: bool,
    pub summary_applied: Option<i32>,
    pub started: Instant,
}

/// Terminal outcome of the provider task.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// `response.completed` / `response.incomplete`.
    Completed { text: String, usage: Option<UsageTokens>, response_id: Option<String> },
    /// Provider / tool-limit failure.
    Failed { code: String, message: String, usage: Option<UsageTokens> },
    /// Client disconnect.
    Cancelled { text: String },
}

/// Computed settlement.
#[derive(Debug, Clone, Copy)]
pub struct Settlement {
    /// `actual` / `estimated`.
    pub method: &'static str,
    /// Committed credits.
    pub credits: i64,
    /// Telemetry tokens added to `quota_usage`.
    pub telemetry: (i64, i64),
}

const MESSAGE_PERSISTENCE: &str = "message_persistence:";

/// Billing outcome / settlement of a failed turn.
#[must_use]
pub fn usage_known(u: Option<&UsageTokens>) -> bool {
    u.is_some_and(UsageTokens::is_known)
}

/// Computes the settlement of a turn.
///
/// # Errors
/// Credit computation errors.
pub fn settlement(
    reserve_tokens: i64,
    max_output_applied: i64,
    reserved_credits: i64,
    floor: i64,
    entry: &ModelCatalogEntry,
    actual: Option<&UsageTokens>,
    tolerance: f64,
) -> Result<(Settlement, bool), DomainError> {
    let in_m = mult(entry.input_tokens_credit_multiplier_micro);
    let out_m = mult(entry.output_tokens_credit_multiplier_micro);
    if let Some(u) = actual {
        let credits = credits_micro_checked(u.input_tokens, u.output_tokens, in_m, out_m)
            .map_err(|e| DomainError::internal(format!("settlement: {e}")))?;
        let actual_tokens = u.input_tokens + u.output_tokens;
        let mut committed = credits;
        let mut overshoot = false;
        if reserve_tokens > 0 && actual_tokens > reserve_tokens {
            overshoot = true;
            #[allow(clippy::cast_precision_loss)]
            let factor = actual_tokens as f64 / reserve_tokens as f64;
            if factor > tolerance {
                committed = reserved_credits;
            }
        }
        return Ok((Settlement { method: "actual", credits: committed, telemetry: (u.input_tokens, u.output_tokens) }, overshoot));
    }
    let credits = estimated_settlement_credits(reserve_tokens, max_output_applied, floor, in_m, out_m)
        .map_err(|e| DomainError::internal(format!("settlement: {e}")))?;
    Ok((Settlement { method: "estimated", credits, telemetry: (0, 0) }, false))
}

/// Builds the usage event of a user turn.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn usage_event(
    tenant_id: Uuid,
    user_id: Option<Uuid>,
    chat_id: Uuid,
    turn_id: Uuid,
    request_id: Uuid,
    effective: &str,
    selected: &str,
    terminal_state: &str,
    billing_outcome: &str,
    usage: Option<UsageTokens>,
    s: &Settlement,
    policy_version: u64,
    counts: (i32, i32, i32),
) -> UsageEvent {
    UsageEvent {
        tenant_id,
        user_id,
        chat_id,
        turn_id: Some(turn_id),
        request_id,
        effective_model: effective.to_owned(),
        selected_model: selected.to_owned(),
        terminal_state: terminal_state.to_owned(),
        billing_outcome: billing_outcome.to_owned(),
        usage,
        actual_credits_micro: s.credits,
        settlement_method: s.method.to_owned(),
        policy_version_applied: policy_version,
        web_search_calls: u32::try_from(counts.0).unwrap_or(0),
        code_interpreter_calls: u32::try_from(counts.1).unwrap_or(0),
        file_search_calls: u32::try_from(counts.2).unwrap_or(0),
        timestamp: OffsetDateTime::now_utc(),
        requester_type: "user".to_owned(),
        dedupe_key: turn_dedupe_key(tenant_id, turn_id, request_id),
        system_task_type: None,
    }
}

fn audit_event(turn: &TurnCtx, completed: bool, usage: Option<UsageTokens>, counts: (i32, i32, i32), error_code: Option<String>, ttft: Option<Instant>) -> MiniChatAuditEvent {
    MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: if completed { "turn_completed" } else { "turn_failed" }.to_owned(),
        tenant_id: turn.tenant_id,
        user_id: Some(turn.user_id),
        chat_id: turn.chat_id,
        turn_id: turn.turn_id,
        request_id: turn.request_id,
        selected_model: turn.selected_model.clone(),
        effective_model: turn.effective.id.clone(),
        usage,
        latency_ms: u64::try_from(turn.started.elapsed().as_millis()).ok(),
        ttft_ms: ttft.and_then(|t| u64::try_from(t.duration_since(turn.started).as_millis()).ok()),
        tool_calls: AuditToolCalls {
            web_search_calls: u32::try_from(counts.0).unwrap_or(0),
            file_search_calls: u32::try_from(counts.2).unwrap_or(0),
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: turn.decision.as_str().to_owned(),
                downgrade_from: (turn.decision == QuotaDecision::Downgrade).then(|| turn.selected_model.clone()),
                downgrade_reason: turn.downgrade_reason.map(str::to_owned),
            },
            license: None,
        },
        error_code,
        prompt: None,
        response: None,
        attachments: Vec::new(),
        quota_scope: None,
        timestamp: OffsetDateTime::now_utc(),
    })
}

/// Converts status entries into `quota_warnings`.
#[must_use]
pub fn warnings(entries: &[quota::PeriodStatus]) -> Vec<QuotaWarning> {
    entries
        .iter()
        .map(|e| QuotaWarning {
            tier: e.tier,
            period: e.period.as_str(),
            remaining_percentage: e.remaining_percentage,
            warning: e.warning,
            exhausted: e.exhausted,
            next_reset: (e.warning || e.exhausted).then(|| {
                e.next_reset.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
            }),
        })
        .collect()
}

/// CAS transition of a running turn.
#[allow(clippy::too_many_arguments)]
async fn cas_finalize(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    turn_id: Uuid,
    new_state: &str,
    assistant_message_id: Option<Uuid>,
    provider_response_id: Option<String>,
    error_code: Option<String>,
    error_detail: Option<String>,
    counts: (i32, i32, i32),
) -> Result<bool, DomainError> {
    let ts = now();
    let res = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::State, Expr::value(new_state))
        .col_expr(chat_turns::Column::AssistantMessageId, Expr::value(assistant_message_id))
        .col_expr(chat_turns::Column::ProviderResponseId, Expr::value(provider_response_id))
        .col_expr(chat_turns::Column::ErrorCode, Expr::value(error_code))
        .col_expr(chat_turns::Column::ErrorDetail, Expr::value(error_detail))
        .col_expr(chat_turns::Column::WebSearchCompletedCount, Expr::value(counts.0))
        .col_expr(chat_turns::Column::CodeInterpreterCompletedCount, Expr::value(counts.1))
        .col_expr(chat_turns::Column::FileSearchCompletedCount, Expr::value(counts.2))
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(state::RUNNING)),
        )
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Inserts the assistant message of a turn.
#[allow(clippy::too_many_arguments)]
async fn insert_assistant(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    turn: &TurnCtx,
    content: &str,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
) -> Result<(), DomainError> {
    let u = usage.unwrap_or_default();
    let am = messages::ActiveModel {
        id: Set(turn.assistant_message_id),
        tenant_id: Set(turn.tenant_id),
        chat_id: Set(turn.chat_id),
        request_id: Set(Some(turn.request_id)),
        role: Set("assistant".into()),
        content: Set(content.to_owned()),
        content_type: Set("text".into()),
        token_estimate: Set(0),
        provider_response_id: Set(response_id),
        request_kind: Set("chat".into()),
        features_used: Set(serde_json::Value::Array(vec![])),
        input_tokens: Set(u.input_tokens.max(0)),
        output_tokens: Set(u.output_tokens.max(0)),
        cache_read_input_tokens: Set(u.cache_read_input_tokens.max(0)),
        cache_write_input_tokens: Set(u.cache_write_input_tokens.max(0)),
        reasoning_tokens: Set(u.reasoning_tokens.max(0)),
        model: Set(Some(turn.effective.id.clone())),
        is_compressed: Set(false),
        created_at: Set(now()),
        deleted_at: Set(None),
    };
    secure_insert::<messages::Entity>(am, scope, tx)
        .await
        .map_err(|e| DomainError::Internal(format!("{MESSAGE_PERSISTENCE} {e}")))?;
    Ok(())
}

/// Frozen target frontier of a summary: latest non-deleted message not of the given request.
///
/// # Errors
/// Database errors.
pub async fn frozen_target(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    exclude_request: Uuid,
) -> Result<Option<messages::Model>, DomainError> {
    Ok(messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(messages::Column::RequestId.is_null())
                        .add(messages::Column::RequestId.ne(exclude_request)),
                ),
        )
        .secure()
        .scope_with(scope)
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Enqueues a thread summary task when the trigger fired; returns whether it was scheduled.
///
/// # Errors
/// Database / outbox errors.
pub async fn schedule_summary(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    outbox: &Enqueuer,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<(bool, Wake), DomainError> {
    let Some(target) = frozen_target(tx, scope, chat_id, request_id).await? else {
        return Ok((false, Wake::empty()));
    };
    let current = repo::thread_summary(tx, scope, chat_id).await?;
    if let Some(s) = &current
        && (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (target.created_at, target.id)
    {
        return Ok((false, Wake::empty()));
    }
    let task = ThreadSummaryTask {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: current.as_ref().map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: current.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".to_owned(),
    };
    let wake = outbox.thread_summary(tx, &task).await?;
    Ok((true, wake))
}

impl Svc {
    fn settlement_entry(turn: &TurnCtx, snapshot: Option<&mini_chat_sdk::PolicySnapshot>) -> ModelCatalogEntry {
        snapshot
            .and_then(|s| s.model(&turn.effective.id).cloned())
            .unwrap_or_else(|| turn.effective.clone())
    }

    /// Finalizes a turn; returns the terminal event to send (if any).
    #[allow(
        clippy::cognitive_complexity,
        reason = "turn finalization orchestration: one branch per outcome with metrics and terminal events"
    )]
    pub async fn finalize_turn(
        &self,
        turn: &TurnCtx,
        outcome: Outcome,
        counts: (i32, i32, i32),
        first_token: Option<Instant>,
    ) -> Option<StreamEvent> {
        let started = Instant::now();
        let snapshot = self.policy.snapshot_version(turn.user_id, turn.policy_version).await.ok();
        let entry = Self::settlement_entry(turn, snapshot.as_ref());
        let labels = [("provider", turn.target.provider_id.as_str()), ("model", turn.effective.id.as_str())];
        let result = match outcome {
            Outcome::Completed { text, usage, response_id } => {
                match self.finalize_completed(turn, &entry, &text, usage, response_id, counts, first_token).await {
                    Ok(Some(done)) => {
                        self.metrics.inc("stream_completed_total", &labels);
                        Some(StreamEvent::Done(done))
                    }
                    Ok(None) => Some(StreamEvent::Error {
                        code: "stream_interrupted".into(),
                        message: "The stream was interrupted".into(),
                    }),
                    Err(DomainError::Internal(m)) if m.starts_with(MESSAGE_PERSISTENCE) => {
                        tracing::error!(error = %m, "assistant message persistence failed");
                        if let Err(e) = self
                            .finalize_failed(turn, &entry, "message_persistence_failed", "The answer could not be saved", usage, counts, first_token)
                            .await
                        {
                            tracing::warn!(error = %e, "failing the turn after persistence failure failed");
                        }
                        Some(StreamEvent::Error {
                            code: "message_persistence_failed".into(),
                            message: "The answer could not be saved".into(),
                        })
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "finalization failed");
                        Some(StreamEvent::Error { code: "finalization_failed".into(), message: "The turn could not be finalized".into() })
                    }
                }
            }
            Outcome::Failed { code, message, usage } => {
                self.metrics.inc("stream_failed_total", &[("provider", &turn.target.provider_id), ("model", &turn.effective.id), ("error_code", &code)]);
                match self.finalize_failed(turn, &entry, &code, &message, usage, counts, first_token).await {
                    Ok(false) => Some(StreamEvent::Error { code: "stream_interrupted".into(), message: "The stream was interrupted".into() }),
                    Ok(true) | Err(_) => Some(StreamEvent::Error { code, message }),
                }
            }
            Outcome::Cancelled { text } => {
                self.metrics.inc("cancel_requested_total", &[("trigger", "client_disconnect")]);
                if let Ok(true) = self.finalize_cancelled(turn, &entry, &text, counts, first_token).await {
                    self.metrics.inc("cancel_effective_total", &[("trigger", "client_disconnect")]);
                    self.metrics.inc("streams_aborted_total", &[("trigger", "client_disconnect")]);
                }
                let stage = if first_token.is_some() { "mid_stream" } else { "before_first_token" };
                self.metrics.inc("stream_disconnected_total", &[("stage", stage)]);
                None
            }
        };
        #[allow(clippy::cast_precision_loss)]
        {
            self.metrics.record("finalization_latency_ms", started.elapsed().as_millis() as f64, &[]);
            self.metrics.record("stream_total_latency_ms", turn.started.elapsed().as_millis() as f64, &labels);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn finalize_completed(
        &self,
        turn: &TurnCtx,
        entry: &ModelCatalogEntry,
        text: &str,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        counts: (i32, i32, i32),
        first_token: Option<Instant>,
    ) -> Result<Option<Done>, DomainError> {
        let actual = usage.unwrap_or_default();
        let (s, overshoot) = settlement(
            turn.reserve.reserve_tokens,
            turn.reserve.max_output_tokens_applied,
            turn.reserve.reserved_credits_micro,
            turn.floor_applied,
            entry,
            Some(&actual),
            self.cfg.quota.overshoot_tolerance_factor,
        )?;
        let trigger = self.cfg.thread_summary_worker.enabled
            && crate::domain::context::summary_trigger(
                &crate::domain::context::ContextPlan {
                    instructions: String::new(),
                    input: Vec::new(),
                    assembled_tokens: turn.assembled_tokens,
                    effective_budget: turn.effective_budget,
                    messages_truncated: turn.messages_truncated,
                    summary_applied: None,
                },
                turn.has_summary,
                self.cfg.thread_summary_worker.compression_threshold_pct,
            );
        let scope = AccessScope::for_tenant(turn.tenant_id);
        let outbox = self.outbox.clone();
        let t = turn.clone();
        let text = text.to_owned();
        let tier = entry.tier;
        let audit = audit_event(turn, true, usage, counts, None, first_token);
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    if !cas_finalize(tx, &scope, t.turn_id, state::COMPLETED, Some(t.assistant_message_id), response_id.clone(), None, None, counts).await? {
                        return Ok(None);
                    }
                    insert_assistant(tx, &scope, &t, &text, usage, response_id).await?;
                    quota::apply_settlement(tx, t.tenant_id, t.user_id, &t.starts, tier, t.reserve.reserved_credits_micro, s.credits, s.telemetry, (counts.0, counts.1)).await?;
                    let ev = usage_event(t.tenant_id, Some(t.user_id), t.chat_id, t.turn_id, t.request_id, &t.effective.id, &t.selected_model, "completed", "completed", usage, &s, t.policy_version, counts);
                    let mut wake = outbox.usage(tx, &ev).await?;
                    wake += outbox.audit(tx, t.tenant_id, &audit).await?;
                    let scheduled = if trigger {
                        let (sched, w) = schedule_summary(tx, &scope, &outbox, t.tenant_id, t.chat_id, t.request_id).await?;
                        wake += w;
                        Some(sched)
                    } else {
                        None
                    };
                    let rows = quota::read_rows(tx, t.tenant_id, t.user_id, &t.starts).await?;
                    Ok(Some((wake, rows, scheduled)))
                })
            })
            .await?;
        let Some((wake, rows, scheduled)) = res else { return Ok(None) };
        wake.fire();
        if overshoot {
            for p in ["daily", "monthly"] {
                self.metrics.inc("quota_overshoot_total", &[("period", p)]);
            }
        }
        for p in ["daily", "monthly"] {
            self.metrics.inc("quota_commit_total", &[("period", p)]);
        }
        if counts.1 > 0 {
            self.metrics.add("code_interpreter_calls_total", u64::try_from(counts.1).unwrap_or(0), &[("model", turn.effective.id.as_str())]);
        }
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record("quota_actual_tokens", (actual.input_tokens + actual.output_tokens) as f64, &[]);
        if let Some(sched) = scheduled {
            self.metrics.inc("thread_summary_trigger_total", &[("result", if sched { "scheduled" } else { "not_needed" })]);
        }
        let entries = quota::status_entries(&rows, &turn.limits, turn.starts, self.cfg.quota.warning_threshold_pct);
        let downgrade = turn.decision == QuotaDecision::Downgrade;
        Ok(Some(Done {
            usage: DoneUsage { input_tokens: actual.input_tokens, output_tokens: actual.output_tokens },
            effective_model: turn.effective.id.clone(),
            selected_model: turn.selected_model.clone(),
            quota_decision: turn.decision.as_str(),
            downgrade_from: downgrade.then(|| turn.selected_model.clone()),
            downgrade_reason: if downgrade { turn.downgrade_reason.map(str::to_owned) } else { None },
            quota_warnings: Some(warnings(&entries)),
        }))
    }

    /// Finalizes a failed turn; returns whether the CAS was won.
    ///
    /// # Errors
    /// Database / settlement errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn finalize_failed(
        &self,
        turn: &TurnCtx,
        entry: &ModelCatalogEntry,
        code: &str,
        message: &str,
        usage: Option<UsageTokens>,
        counts: (i32, i32, i32),
        first_token: Option<Instant>,
    ) -> Result<bool, DomainError> {
        let known = usage.filter(UsageTokens::is_known);
        let (s, _) = settlement(
            turn.reserve.reserve_tokens,
            turn.reserve.max_output_tokens_applied,
            turn.reserve.reserved_credits_micro,
            turn.floor_applied,
            entry,
            known.as_ref(),
            self.cfg.quota.overshoot_tolerance_factor,
        )?;
        let scope = AccessScope::for_tenant(turn.tenant_id);
        let outbox = self.outbox.clone();
        let t = turn.clone();
        let tier = entry.tier;
        let code = code.to_owned();
        let detail = message.to_owned();
        let audit = audit_event(turn, false, known, counts, Some(code.clone()), first_token);
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    if !cas_finalize(tx, &scope, t.turn_id, state::FAILED, None, None, Some(code), Some(detail), counts).await? {
                        return Ok(None);
                    }
                    quota::apply_settlement(tx, t.tenant_id, t.user_id, &t.starts, tier, t.reserve.reserved_credits_micro, s.credits, s.telemetry, (counts.0, counts.1)).await?;
                    let ev = usage_event(t.tenant_id, Some(t.user_id), t.chat_id, t.turn_id, t.request_id, &t.effective.id, &t.selected_model, "failed", "failed", known, &s, t.policy_version, counts);
                    let mut wake = outbox.usage(tx, &ev).await?;
                    wake += outbox.audit(tx, t.tenant_id, &audit).await?;
                    Ok(Some(wake))
                })
            })
            .await?;
        Ok(match res {
            Some(w) => {
                w.fire();
                true
            }
            None => false,
        })
    }

    async fn finalize_cancelled(
        &self,
        turn: &TurnCtx,
        entry: &ModelCatalogEntry,
        text: &str,
        counts: (i32, i32, i32),
        first_token: Option<Instant>,
    ) -> Result<bool, DomainError> {
        let (s, _) = settlement(
            turn.reserve.reserve_tokens,
            turn.reserve.max_output_tokens_applied,
            turn.reserve.reserved_credits_micro,
            turn.floor_applied,
            entry,
            None,
            self.cfg.quota.overshoot_tolerance_factor,
        )?;
        for with_message in [!text.is_empty(), false] {
            let scope = AccessScope::for_tenant(turn.tenant_id);
            let outbox = self.outbox.clone();
            let t = turn.clone();
            let tier = entry.tier;
            let text = text.to_owned();
            let audit = audit_event(turn, false, None, counts, None, first_token);
            let res = self
                .db
                .write_transaction(move |tx| {
                    Box::pin(async move {
                        let msg_id = with_message.then_some(t.assistant_message_id);
                        if !cas_finalize(tx, &scope, t.turn_id, state::CANCELLED, msg_id, None, None, None, counts).await? {
                            return Ok(None);
                        }
                        if with_message {
                            insert_assistant(tx, &scope, &t, &text, None, None).await?;
                        }
                        quota::apply_settlement(tx, t.tenant_id, t.user_id, &t.starts, tier, t.reserve.reserved_credits_micro, s.credits, s.telemetry, (counts.0, counts.1)).await?;
                        let ev = usage_event(t.tenant_id, Some(t.user_id), t.chat_id, t.turn_id, t.request_id, &t.effective.id, &t.selected_model, "cancelled", "aborted", None, &s, t.policy_version, counts);
                        let mut wake = outbox.usage(tx, &ev).await?;
                        wake += outbox.audit(tx, t.tenant_id, &audit).await?;
                        Ok(Some(wake))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    w.fire();
                    return Ok(true);
                }
                Ok(None) => return Ok(false),
                Err(e) if with_message => {
                    tracing::warn!(error = %e, "partial assistant message not persisted on cancel");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }
}
