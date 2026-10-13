//! Stream finalization (DESIGN 5.7 "`FinalizeTurn` Invariant", "Terminal SSE Event Emission
//! Guard"): one transaction with the CAS on the `running` turn, the assistant message, the quota
//! settlement, the usage + audit outbox events and, for a completed turn whose trigger fires,
//! the thread summary work item. The terminal SSE event is decided only after the commit.

use mini_chat_sdk::{
    AuditEvent, PolicyDecisions, QuotaPolicyDecision, ToolCalls, TurnAuditEvent, TurnLatency,
    UsageEvent, UsageTokens,
};
use opentelemetry::KeyValue;
use opentelemetry::trace::TraceContextExt as _;
use sea_orm::ActiveValue::Set;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use uuid::Uuid;

use super::events::{CitationDto, DonePayload, UsageDto};
use super::{StreamService, TurnContext};
use crate::domain::error::DomainError;
use crate::domain::quota::{
    BillingOutcome, QuotaDecision, SettleInput, SettlementMethod, derive_billing,
};
use crate::domain::thread_summary;
use crate::infra::db::entity::messages;
use crate::infra::db::repo::turns::TurnTerminal;
use crate::infra::db::repo::{messages as message_repo, turns};
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx_with_wakes;
use crate::infra::db::{MessageRole, TurnState};
use crate::infra::llm::ProviderUsage;
use crate::infra::outbox::OutboxRecord;

/// SSE code when the finalization transaction of a completed stream failed.
const FINALIZATION_FAILED: &str = "finalization_failed";
/// Error code when the assistant message of a completed stream could not be persisted.
const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
const FINALIZATION_FAILED_MESSAGE: &str = "The turn could not be finalized";
const PERSISTENCE_FAILED_MESSAGE: &str = "The answer could not be stored";

/// How the stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// Provider `completed`, or `incomplete` with its reason.
    Completed { incomplete_reason: Option<String> },
    /// Terminal error; `error_code` is the SSE code, `client_message` is sanitized.
    Failed {
        error_code: String,
        client_message: String,
    },
    /// The client disconnected.
    Cancelled,
}

/// Completed built-in tool calls of the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounts {
    pub web_search: u32,
    pub code_interpreter: u32,
    /// Provider-native `file_search` done events, or the executed `search_knowledge`
    /// retrievals (failed ones included; limit-reached and invalid calls excluded). The two
    /// tools are never in one request.
    pub file_search: u32,
}

/// What the stream sends after the finalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeResult {
    /// `citations` (when non-empty), then `done`.
    Done(DonePayload, Vec<CitationDto>),
    /// `error{code, message}`.
    Error { code: String, message: String },
    /// Another finalizer already ended the turn: no terminal event from this path.
    CasLost,
    /// Cancelled: the client is gone, nothing is sent.
    NoEvent,
}

/// Failure inside the finalization transaction; the transaction is rolled back.
enum TxError {
    /// The assistant message insert failed (on `PostgreSQL` the transaction is aborted).
    MessageInsert(DomainError),
    Other(DomainError),
}

/// Inputs of one finalization attempt.
struct Attempt<'a> {
    state: TurnState,
    error_code: Option<String>,
    error_detail: Option<String>,
    /// Persist the assistant message with this text.
    message: Option<&'a str>,
    usage: Option<ProviderUsage>,
    response_id: Option<String>,
    counts: ToolCounts,
    multipliers: (i64, i64),
}

/// Finalizes `turn` with `outcome` and returns what the stream sends next.
///
/// `completed` always persists the assistant message (`text` may be empty); `cancelled`
/// persists it when `text` is non-empty, best-effort (an insert failure finalizes again without
/// the message). A failed assistant insert of a completed stream finalizes the turn as
/// `failed` / `message_persistence_failed`. When the transaction itself fails, a completed
/// stream gets `error{finalization_failed}` (the turn stays `running` for the orphan watchdog)
/// and a failed stream keeps its original error.
#[allow(clippy::too_many_arguments)] // the terminal facts of a stream, passed once
pub async fn finalize_turn(
    deps: &StreamService,
    turn: &TurnContext,
    outcome: TerminalOutcome,
    text: &str,
    usage: Option<ProviderUsage>,
    response_id: Option<String>,
    counts: ToolCounts,
    citations: Vec<CitationDto>,
) -> FinalizeResult {
    let (state, error_code, error_detail) = terminal_fields(turn, &outcome);
    let message = match state {
        TurnState::Completed => Some(text),
        TurnState::Cancelled => Some(text).filter(|t| !t.is_empty()),
        TurnState::Failed | TurnState::Running => None,
    };
    let attempt = Attempt {
        state,
        error_code,
        error_detail,
        message,
        usage,
        response_id,
        counts,
        multipliers: multipliers(deps, turn).await,
    };
    match commit(deps, turn, &attempt).await {
        Err(TxError::MessageInsert(err)) => {
            tracing::warn!(turn_id = %turn.turn_id, error = %err, "assistant message insert failed");
            retry_without_message(deps, turn, outcome, attempt).await
        }
        committed => resolve(deps, turn, outcome, committed, usage, citations).await,
    }
}

/// Turn state, error code and error detail of `outcome`; logs the reason of an incomplete
/// response (it is not persisted).
fn terminal_fields(
    turn: &TurnContext,
    outcome: &TerminalOutcome,
) -> (TurnState, Option<String>, Option<String>) {
    match outcome {
        TerminalOutcome::Completed { incomplete_reason } => {
            if let Some(reason) = incomplete_reason {
                tracing::warn!(turn_id = %turn.turn_id, reason = %reason, "stream incomplete");
            }
            (TurnState::Completed, None, None)
        }
        TerminalOutcome::Failed {
            error_code,
            client_message,
        } => (
            TurnState::Failed,
            Some(error_code.clone()),
            Some(client_message.clone()),
        ),
        TerminalOutcome::Cancelled => (TurnState::Cancelled, None, None),
    }
}

/// Second finalization after the assistant insert failed: a completed stream becomes
/// `failed` / `message_persistence_failed`; a cancelled one is finalized without its partial
/// message.
async fn retry_without_message(
    deps: &StreamService,
    turn: &TurnContext,
    outcome: TerminalOutcome,
    attempt: Attempt<'_>,
) -> FinalizeResult {
    if !matches!(outcome, TerminalOutcome::Completed { .. }) {
        let retry = Attempt {
            message: None,
            ..attempt
        };
        let committed = commit(deps, turn, &retry).await;
        return resolve(deps, turn, outcome, committed, retry.usage, Vec::new()).await;
    }
    let retry = Attempt {
        state: TurnState::Failed,
        error_code: Some(MESSAGE_PERSISTENCE_FAILED.to_owned()),
        error_detail: Some(PERSISTENCE_FAILED_MESSAGE.to_owned()),
        message: None,
        ..attempt
    };
    match commit(deps, turn, &retry).await {
        Err(TxError::MessageInsert(err) | TxError::Other(err)) => {
            tracing::error!(turn_id = %turn.turn_id, error = %err, "turn finalization failed");
            error(FINALIZATION_FAILED, FINALIZATION_FAILED_MESSAGE)
        }
        committed => {
            let failed = TerminalOutcome::Failed {
                error_code: MESSAGE_PERSISTENCE_FAILED.to_owned(),
                client_message: PERSISTENCE_FAILED_MESSAGE.to_owned(),
            };
            resolve(deps, turn, failed, committed, retry.usage, Vec::new()).await
        }
    }
}

/// What the stream sends after a commit attempt of `outcome`.
async fn resolve(
    deps: &StreamService,
    turn: &TurnContext,
    outcome: TerminalOutcome,
    committed: Result<bool, TxError>,
    usage: Option<ProviderUsage>,
    citations: Vec<CitationDto>,
) -> FinalizeResult {
    match committed {
        Ok(true) => {}
        Ok(false) => return FinalizeResult::CasLost,
        Err(TxError::MessageInsert(err) | TxError::Other(err)) => {
            tracing::error!(turn_id = %turn.turn_id, error = %err, "turn finalization failed");
            return match outcome {
                TerminalOutcome::Completed { .. } => {
                    error(FINALIZATION_FAILED, FINALIZATION_FAILED_MESSAGE)
                }
                TerminalOutcome::Failed {
                    error_code,
                    client_message,
                } => FinalizeResult::Error {
                    code: error_code,
                    message: client_message,
                },
                TerminalOutcome::Cancelled => FinalizeResult::NoEvent,
            };
        }
    }
    record_outcome(deps, turn, &outcome);
    match outcome {
        TerminalOutcome::Completed { .. } => {
            let warnings = quota_warnings(deps, turn).await;
            FinalizeResult::Done(done_payload(turn, usage, warnings), citations)
        }
        TerminalOutcome::Failed {
            error_code,
            client_message,
        } => FinalizeResult::Error {
            code: error_code,
            message: client_message,
        },
        TerminalOutcome::Cancelled => FinalizeResult::NoEvent,
    }
}

fn error(code: &str, message: &str) -> FinalizeResult {
    FinalizeResult::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// Credit multipliers of the effective model in the policy version the turn was admitted
/// under; the preflight's catalog entry when that snapshot cannot be read.
async fn multipliers(deps: &StreamService, turn: &TurnContext) -> (i64, i64) {
    let model = &turn.decision.effective_model;
    let fallback = (
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    );
    match deps
        .policy
        .snapshot(turn.user_id, turn.decision.policy_version)
        .await
    {
        Ok(snapshot) => snapshot
            .model_catalog
            .iter()
            .find(|m| m.id == model.id)
            .map_or(fallback, |m| {
                (
                    m.input_tokens_credit_multiplier_micro,
                    m.output_tokens_credit_multiplier_micro,
                )
            }),
        Err(err) => {
            tracing::warn!(turn_id = %turn.turn_id, error = %err,
                "policy snapshot unavailable at finalization; using the preflight multipliers");
            fallback
        }
    }
}

/// One finalization transaction; `Ok(false)` when the CAS lost. Wakes fire after the commit.
async fn commit(
    deps: &StreamService,
    turn: &TurnContext,
    a: &Attempt<'_>,
) -> Result<bool, TxError> {
    let now = db_now();
    let (billing, method) = derive_billing(a.state, a.error_code.as_deref(), a.usage.as_ref());
    let terminal = TurnTerminal {
        state: a.state,
        error_code: a.error_code.clone(),
        error_detail: a.error_detail.clone(),
        provider_response_id: a.response_id.clone(),
        assistant_message_id: a.message.map(|_| turn.assistant_message_id),
        now,
    };
    let message = a
        .message
        .map(|text| assistant_message(turn, text, a.usage, a.response_id.clone(), now));
    let settle = settle_input(turn, a, method);
    let (quota, outbox) = (deps.quota.clone(), deps.outbox.clone());
    let (turn_tx, state, error_code, usage, counts) = (
        turn.clone(),
        a.state,
        a.error_code.clone(),
        a.usage,
        a.counts,
    );
    let scope = AccessScope::for_tenant(turn.tenant_id);
    let summary_cfg = &deps.cfg.thread_summary_worker;
    let summarize = a.state == TurnState::Completed
        && summary_cfg.enabled
        && thread_summary::evaluate_trigger(&turn.summary, summary_cfg.compression_threshold_pct);

    // Everything below runs again from the CAS on a retried attempt (`write_tx_with_wakes`
    // retries contention; a lost CAS and every other error are final).
    let message_failed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&message_failed);
    let committed = write_tx_with_wakes(&deps.db, |tx, wakes| {
        let (quota, outbox, turn) = (quota.clone(), outbox.clone(), turn_tx.clone());
        let (scope, terminal, message, settle) = (
            scope.clone(),
            terminal.clone(),
            message.clone(),
            settle.clone(),
        );
        let error_code = error_code.clone();
        let flag = Arc::clone(&flag);
        flag.store(false, Ordering::SeqCst);
        Box::pin(async move {
            if !turns::finalize_running(tx, &scope, turn.turn_id, &terminal).await? {
                return Ok(None);
            }
            if let Some(message) = message {
                message_repo::insert(tx, &scope, message)
                    .await
                    .inspect_err(|_| {
                        flag.store(true, Ordering::SeqCst);
                    })?;
            }
            let settled = quota.settle(tx, settle).await?;
            let facts = EventFacts {
                state,
                error_code,
                billing,
                method,
                usage,
                counts,
                committed_credits_micro: settled.committed_credits_micro,
                now,
            };
            let subject = EventSubject::from_context(&turn);
            wakes.add(
                outbox
                    .enqueue(tx, OutboxRecord::usage(&usage_event(&subject, &facts))?)
                    .await?,
            );
            wakes.add(
                outbox
                    .enqueue(tx, OutboxRecord::audit(&audit_event(&subject, &facts))?)
                    .await?,
            );
            let scheduled = if summarize {
                let (scheduled, wake) =
                    thread_summary::enqueue_if_needed(tx, &outbox, &turn).await?;
                if let Some(wake) = wake {
                    wakes.add(wake);
                }
                Some(scheduled)
            } else {
                None
            };
            Ok(Some((settled.metrics, scheduled)))
        })
    })
    .await;
    match committed {
        // Metrics only after the commit: the closure may have run several times.
        Ok(Some((metrics, scheduled))) => {
            deps.quota.record_facts(metrics);
            if let Some(scheduled) = scheduled {
                let result = if scheduled { "scheduled" } else { "not_needed" };
                deps.metrics
                    .thread_summary_trigger
                    .add(1, &[KeyValue::new("result", result)]);
            }
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(err) if message_failed.load(Ordering::SeqCst) => Err(TxError::MessageInsert(err)),
        Err(err) => Err(TxError::Other(err)),
    }
}

fn assistant_message(
    turn: &TurnContext,
    text: &str,
    usage: Option<ProviderUsage>,
    response_id: Option<String>,
    now: OffsetDateTime,
) -> messages::ActiveModel {
    let u = usage.unwrap_or_default();
    messages::ActiveModel {
        id: Set(turn.assistant_message_id),
        tenant_id: Set(turn.tenant_id),
        chat_id: Set(turn.chat_id),
        request_id: Set(Some(turn.request_id)),
        role: Set(MessageRole::Assistant.as_str().to_owned()),
        content: Set(text.to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(response_id),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(u.input_tokens),
        output_tokens: Set(u.output_tokens),
        cache_read_input_tokens: Set(u.cache_read_input_tokens),
        cache_write_input_tokens: Set(u.cache_write_input_tokens),
        reasoning_tokens: Set(u.reasoning_tokens),
        model: Set(Some(turn.decision.effective_model.id.clone())),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(None),
    }
}

fn settle_input(turn: &TurnContext, a: &Attempt<'_>, method: SettlementMethod) -> SettleInput {
    let d = &turn.decision;
    SettleInput {
        tenant_id: turn.tenant_id,
        user_id: turn.user_id,
        is_premium: d.effective_is_premium,
        periods: d.periods,
        turn_reserved_credits_micro: d.reserve.reserved_credits_micro,
        reserve_tokens: d.reserve.reserve_tokens,
        max_output_tokens_applied: i64::from(d.reserve.max_output_tokens_applied),
        minimal_generation_floor_applied: i64::from(d.reserve.minimal_generation_floor_applied),
        in_mult: a.multipliers.0,
        out_mult: a.multipliers.1,
        method,
        usage: a.usage,
        web_search_calls: i64::from(a.counts.web_search),
        code_interpreter_calls: i64::from(a.counts.code_interpreter),
    }
}

/// Values shared by the usage and audit events of one finalization.
pub(crate) struct EventFacts {
    pub state: TurnState,
    pub error_code: Option<String>,
    pub billing: BillingOutcome,
    pub method: SettlementMethod,
    pub usage: Option<ProviderUsage>,
    pub counts: ToolCounts,
    pub committed_credits_micro: i64,
    pub now: OffsetDateTime,
}

/// The turn the usage and audit events describe: what the stream finalization knows from its
/// [`TurnContext`] and the orphan watchdog from the persisted row.
#[derive(Clone)]
pub(crate) struct EventSubject {
    pub tenant_id: Uuid,
    /// `None` for a system turn or a row without `requester_user_id`.
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    pub policy_version_applied: i64,
    pub quota: QuotaPolicyDecision,
    pub total_ms: u64,
}

impl EventSubject {
    fn from_context(turn: &TurnContext) -> Self {
        let d = &turn.decision;
        let downgraded = d.quota_decision == QuotaDecision::Downgrade;
        Self {
            tenant_id: turn.tenant_id,
            user_id: Some(turn.user_id),
            chat_id: turn.chat_id,
            turn_id: turn.turn_id,
            request_id: turn.request_id,
            effective_model: d.effective_model.id.clone(),
            selected_model: turn.selected_model.clone(),
            policy_version_applied: d.policy_version,
            quota: QuotaPolicyDecision {
                decision: d.quota_decision.as_str().to_owned(),
                downgrade_from: downgraded.then(|| turn.selected_model.clone()),
                downgrade_reason: d.downgrade_reason.map(str::to_owned),
            },
            total_ms: u64::try_from(turn.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        }
    }
}

pub(crate) fn usage_event(s: &EventSubject, f: &EventFacts) -> UsageEvent {
    UsageEvent {
        tenant_id: s.tenant_id,
        user_id: s.user_id,
        chat_id: s.chat_id,
        turn_id: Some(s.turn_id),
        request_id: s.request_id,
        effective_model: s.effective_model.clone(),
        selected_model: s.selected_model.clone(),
        terminal_state: f.state.as_str().to_owned(),
        billing_outcome: f.billing.as_str().to_owned(),
        usage: f.usage.map(UsageTokens::from),
        actual_credits_micro: f.committed_credits_micro,
        settlement_method: f.method.as_str().to_owned(),
        policy_version_applied: s.policy_version_applied,
        web_search_calls: f.counts.web_search,
        code_interpreter_calls: f.counts.code_interpreter,
        file_search_calls: f.counts.file_search,
        timestamp: f.now,
        requester_type: "user".to_owned(),
        dedupe_key: format!(
            "{}/{}/{}",
            s.tenant_id.simple(),
            s.turn_id.simple(),
            s.request_id.simple()
        ),
        system_task_type: None,
    }
}

pub(crate) fn audit_event(s: &EventSubject, f: &EventFacts) -> AuditEvent {
    AuditEvent::Turn(TurnAuditEvent {
        event_type: if f.state == TurnState::Completed {
            "turn_completed"
        } else {
            "turn_failed"
        }
        .to_owned(),
        timestamp: f.now,
        tenant_id: s.tenant_id,
        requester_type: "user".to_owned(),
        actor_user_id: s.user_id,
        chat_id: s.chat_id,
        turn_id: s.turn_id,
        request_id: s.request_id,
        selected_model: s.selected_model.clone(),
        effective_model: s.effective_model.clone(),
        terminal_state: f.state.as_str().to_owned(),
        error_code: f.error_code.clone(),
        usage: f.usage.map(UsageTokens::from),
        latency: TurnLatency {
            ttft_ms: None,
            total_ms: s.total_ms,
        },
        tool_calls: ToolCalls {
            web_search_calls: f.counts.web_search,
            file_search_calls: f.counts.file_search,
        },
        policy_decisions: PolicyDecisions {
            quota: s.quota.clone(),
            license: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        quota_scope: None,
        trace_id: current_trace_id(),
    })
}

/// Trace id of the current OpenTelemetry span, when one is active.
fn current_trace_id() -> Option<String> {
    let context = tracing::Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// `quota_warnings` of the user after the commit; `None` when they cannot be read.
async fn quota_warnings(
    deps: &StreamService,
    turn: &TurnContext,
) -> Option<Vec<crate::domain::quota::QuotaWarning>> {
    let conn = deps.db.conn().ok()?;
    deps.quota
        .warnings(
            &conn,
            turn.tenant_id,
            turn.user_id,
            &turn.decision.limits,
            OffsetDateTime::now_utc(),
        )
        .await
        .inspect_err(|err| tracing::warn!(turn_id = %turn.turn_id, error = %err, "quota warnings unavailable"))
        .ok()
}

fn done_payload(
    turn: &TurnContext,
    usage: Option<ProviderUsage>,
    quota_warnings: Option<Vec<crate::domain::quota::QuotaWarning>>,
) -> DonePayload {
    let d = &turn.decision;
    let u = usage.unwrap_or_default();
    let downgraded = d.quota_decision == QuotaDecision::Downgrade;
    DonePayload {
        usage: UsageDto {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        },
        effective_model: d.effective_model.id.clone(),
        selected_model: turn.selected_model.clone(),
        quota_decision: d.quota_decision.as_str(),
        downgrade_from: downgraded.then(|| turn.selected_model.clone()),
        downgrade_reason: d.downgrade_reason.map(str::to_owned),
        quota_warnings,
    }
}

/// Outcome metrics of a committed finalization: `stream_completed` (+ `stream_incomplete`),
/// `stream_failed`, or `cancel_effective` + `streams_aborted` for a cancelled turn; total
/// latency.
fn record_outcome(deps: &StreamService, turn: &TurnContext, outcome: &TerminalOutcome) {
    let labels = [
        KeyValue::new("provider", turn.provider_id.clone()),
        KeyValue::new("model", turn.decision.effective_model.id.clone()),
    ];
    let metrics = &deps.metrics;
    match outcome {
        TerminalOutcome::Completed { incomplete_reason } => {
            metrics.stream_completed.add(1, &labels);
            if let Some(reason) = incomplete_reason {
                let mut labels = labels.to_vec();
                labels.push(KeyValue::new("reason", reason.clone()));
                metrics.stream_incomplete.add(1, &labels);
            }
        }
        TerminalOutcome::Failed { error_code, .. } => {
            let mut labels = labels.to_vec();
            labels.push(KeyValue::new("error_code", error_code.clone()));
            metrics.stream_failed.add(1, &labels);
        }
        TerminalOutcome::Cancelled => {
            metrics
                .cancel_effective
                .add(1, &[KeyValue::new("trigger", "disconnect")]);
            metrics
                .streams_aborted
                .add(1, &[KeyValue::new("trigger", "client_disconnect")]);
        }
    }
    #[allow(clippy::cast_precision_loss)] // histogram sample in milliseconds
    deps.metrics
        .stream_total_latency_ms
        .record(turn.started_at.elapsed().as_millis() as f64, &labels);
}
