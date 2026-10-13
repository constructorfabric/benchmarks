//! Stream finalization
//! (`cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`,
//! `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`): the terminal
//! decision of a turn, the writes of the finalization transaction and the
//! terminal SSE event.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage, TerminalState,
    TurnAuditEvent, TurnAuditEventType, UsageTokens,
};
use time::OffsetDateTime;
use tokio::time::Instant;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::ports::{
    AssistantMessageWrite, BuiltRequest, FinalizeError, FinalizeWrites, StreamMetrics,
    TurnRepository,
};
use super::tools::ToolsDecision;
use super::types::{DoneEvent, ErrorEvent, StreamErrorCode, StreamEvent, TurnState};
use crate::domain::model::Chat;
use crate::domain::service::quota::finalize::{
    FinalizeRequest, FinalizeResult, FinalizeVariant, FinalizeWon, TerminalClassification,
    TerminalColumns,
};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::preflight::PreflightDecision;
use crate::domain::service::quota::service::{Clock, QuotaService};
use crate::domain::service::quota::settle::ToolCallCounts;
use crate::domain::service::workers::summary_trigger::{
    SummaryTrigger, SummaryTriggerPlan, TriggerResult,
};

/// Collaborators of the provider task and of the finalization.
#[derive(Clone)]
pub struct TaskDeps {
    /// Turn store.
    pub turns: Arc<dyn TurnRepository>,
    /// Provider client.
    pub client: Arc<dyn super::ports::ProviderClient>,
    /// Quota service (tool limits, settlement metrics, warnings).
    pub quota: Arc<QuotaService>,
    /// Metrics sink.
    pub metrics: Arc<dyn StreamMetrics>,
    /// `streaming.sse_ping_interval_seconds` (also the liveness interval).
    pub ping_interval: Duration,
    /// Clock of the timestamps the task writes.
    pub clock: Clock,
    /// The agentic-loop extension point (slice 7 supplies knowledge search).
    pub tool_use: Arc<dyn super::task::ToolUseHandler>,
}

/// Everything the provider task and the finalization know about a turn.
#[derive(Clone, Debug)]
pub struct TurnRun {
    /// The request's security context (provider call).
    pub ctx: SecurityContext,
    /// The chat (`chats.model` is the selected model).
    pub chat: Chat,
    /// Requesting user.
    pub user_id: Uuid,
    /// Request id of the turn.
    pub request_id: Uuid,
    /// Turn id.
    pub turn_id: Uuid,
    /// Pre-allocated assistant message id.
    pub assistant_message_id: Uuid,
    /// `created_at` of the user message (= `started_at`).
    pub user_message_at: OffsetDateTime,
    /// The preflight decision.
    pub decision: PreflightDecision,
    /// The tools of the turn.
    pub tools: ToolsDecision,
    /// The provider request.
    pub request: BuiltRequest,
    /// Provider id of the effective model (metric label).
    pub provider_id: String,
    /// When the stream service accepted the request.
    pub accepted_at: Instant,
    /// The `ContextPlan` outputs and parameters of the thread-summary trigger
    /// (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`).
    pub summary: SummaryTriggerPlan,
}

/// How a turn ended, as the finalization receives it.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// Normal or incomplete completion.
    Completed {
        /// Provider usage.
        usage: Option<UsageTokens>,
        /// Provider response id.
        response_id: Option<String>,
        /// Incomplete reason (metric label only).
        incomplete_reason: Option<String>,
    },
    /// Failure.
    Failed {
        /// Streaming error code.
        code: StreamErrorCode,
        /// Client-safe message.
        message: String,
        /// `error_detail`.
        detail: Option<String>,
        /// Usage reported with the failure.
        usage: Option<UsageTokens>,
        /// Provider response id, when known.
        response_id: Option<String>,
    },
    /// Client disconnect before the provider's terminal event.
    Cancelled,
}

/// The in-memory tool counts of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolCounts {
    /// Completed `web_search` calls.
    pub web_search: u32,
    /// Completed `code_interpreter` calls.
    pub code_interpreter: u32,
    /// Native `file_search` `tool_done` events.
    pub native_file_search: u32,
    /// `search_knowledge` calls, failed retrievals included.
    pub knowledge_calls: u32,
}

/// What the finalization needs besides the run.
#[derive(Debug, Clone)]
pub struct FinalizeInput<'a> {
    /// The turn.
    pub run: &'a TurnRun,
    /// The outcome.
    pub outcome: &'a TurnOutcome,
    /// The accumulated assistant text (reasoning excluded).
    pub text: &'a str,
    /// The in-memory tool counts.
    pub counts: ToolCounts,
    /// Time to first content event.
    pub ttft_ms: Option<u64>,
}

/// The result of a finalization.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeReport {
    /// The committed terminal state; `None` when the CAS was lost or the
    /// transaction failed (the turn is not ours or stays `running`).
    pub state: Option<TurnState>,
    /// The terminal event to send when the client is connected: `done` after a
    /// won `completed`, `error` after a won `failed` (or a failed finalization
    /// of a `completed` / `failed` outcome); never for a lost CAS, a
    /// `cancelled` outcome or a failed `cancelled` finalization.
    pub event: Option<StreamEvent>,
}

/// One run of the finalization transaction.
#[derive(Clone)]
struct Attempt {
    classification: TerminalClassification,
    columns: TerminalColumns,
    usage: Option<UsageTokens>,
    /// The assistant message to persist, when any.
    with_message: bool,
    audit_type: TurnAuditEventType,
}

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-finalization:p1
/// Finalizes the turn through the CAS-guarded transaction.
pub async fn finalize_stream(deps: &TaskDeps, input: &FinalizeInput<'_>) -> FinalizeReport {
    let began = Instant::now();
    let first = attempt_of(input);
    match run(deps, input, &first).await {
        Err(FinalizeError::MessageInsert(error)) => {
            message_insert_failed(deps, input, &first, error, began).await
        }
        result => report_of(deps, input, result, began).await,
    }
}

/// The report of one finalization run.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-match-finalize-result
async fn report_of(
    deps: &TaskDeps,
    input: &FinalizeInput<'_>,
    result: Result<FinalizeResult, FinalizeError>,
    began: Instant,
) -> FinalizeReport {
    match result {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-lost
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-late-result
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-orphan-terminal
        // @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-orphaned
        Ok(FinalizeResult::Lost) => lost(),
        // @cpt-end:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-orphaned
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-orphan-terminal
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-late-result
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-lost
        Ok(FinalizeResult::Won(won)) => {
            let event = on_won(deps, input, &won, began).await;
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-return-final
            FinalizeReport {
                state: Some(turn_state(won.terminal_state)),
                event,
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-return-final
        }
        Err(error) => failed_transaction(input, &error),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-match-finalize-result

/// The assistant message INSERT failed: a completed turn is re-finalized as
/// `message_persistence_failed`, a cancelled one without its partial message,
/// a failed one has no message to lose.
async fn message_insert_failed(
    deps: &TaskDeps,
    input: &FinalizeInput<'_>,
    first: &Attempt,
    error: crate::domain::error::DomainError,
    began: Instant,
) -> FinalizeReport {
    match input.outcome {
        TurnOutcome::Completed {
            usage, response_id, ..
        } => {
            tracing::error!(%error, "the assistant message could not be persisted");
            persistence_failed(deps, input, *usage, response_id.clone(), began).await
        }
        TurnOutcome::Cancelled => {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-partial-failed
            tracing::warn!(%error, "the partial assistant message could not be persisted");
            let mut second = first.clone();
            second.with_message = false;
            second.columns.assistant_message_id = None;
            let result = run(deps, input, &second).await;
            report_of(deps, input, result, began).await
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-partial-failed
        }
        TurnOutcome::Failed { .. } => {
            failed_transaction(input, &FinalizeError::MessageInsert(error))
        }
    }
}

// @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-terminal-immutable
fn lost() -> FinalizeReport {
    FinalizeReport {
        state: None,
        event: None,
    }
}
// @cpt-end:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-terminal-immutable

/// The transaction failed: the turn stays `running`.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-finalize-error
// @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-stay-running
fn failed_transaction(input: &FinalizeInput<'_>, error: &FinalizeError) -> FinalizeReport {
    let cause = match error {
        FinalizeError::MessageInsert(error) | FinalizeError::Other(error) => error,
    };
    tracing::error!(error = %cause, turn_id = %input.run.turn_id, "the finalization transaction failed");
    let event = match input.outcome {
        TurnOutcome::Completed { .. } => Some(error_event(
            StreamErrorCode::FinalizationFailed,
            StreamErrorCode::FinalizationFailed.generic_message(),
        )),
        TurnOutcome::Failed { code, message, .. } => Some(error_event(*code, message)),
        TurnOutcome::Cancelled => None,
    };
    FinalizeReport { state: None, event }
}
// @cpt-end:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-stay-running
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-finalize-error

fn error_event(code: StreamErrorCode, message: &str) -> StreamEvent {
    StreamEvent::Error(ErrorEvent {
        code,
        message: message.to_owned(),
    })
}

/// The second run of a completed turn whose message INSERT failed: `failed` /
/// `message_persistence_failed` without a message, the usage kept.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-persist-failed
async fn persistence_failed(
    deps: &TaskDeps,
    input: &FinalizeInput<'_>,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
    began: Instant,
) -> FinalizeReport {
    let code = StreamErrorCode::MessagePersistenceFailed;
    let second = Attempt {
        classification: TerminalClassification::Failed {
            error_code: code.as_str().to_owned(),
        },
        columns: TerminalColumns {
            state: TerminalState::Failed,
            error_code: Some(code.as_str().to_owned()),
            error_detail: None,
            assistant_message_id: None,
            provider_response_id: response_id,
        },
        usage,
        with_message: false,
        audit_type: audit_type(TerminalState::Failed),
    };
    let failed_outcome = TurnOutcome::Failed {
        code,
        message: code.generic_message().to_owned(),
        detail: None,
        usage: second.usage,
        response_id: second.columns.provider_response_id.clone(),
    };
    let rerun = FinalizeInput {
        outcome: &failed_outcome,
        ..input.clone()
    };
    match run(deps, &rerun, &second).await {
        Ok(FinalizeResult::Won(won)) => {
            let event = on_won(deps, &rerun, &won, began).await;
            FinalizeReport {
                state: Some(turn_state(won.terminal_state)),
                event,
            }
        }
        Ok(FinalizeResult::Lost) => lost(),
        Err(error) => {
            tracing::error!(?error, "the message-free finalization failed too");
            FinalizeReport {
                state: None,
                event: Some(error_event(
                    StreamErrorCode::FinalizationFailed,
                    StreamErrorCode::FinalizationFailed.generic_message(),
                )),
            }
        }
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-persist-failed

fn turn_state(state: TerminalState) -> TurnState {
    match state {
        TerminalState::Completed => TurnState::Completed,
        TerminalState::Failed => TurnState::Failed,
        TerminalState::Cancelled => TurnState::Cancelled,
    }
}

/// `turn_completed` for a completed turn, `turn_failed` for failed and
/// cancelled turns (there is no cancelled type).
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-type
fn audit_type(state: TerminalState) -> TurnAuditEventType {
    match state {
        TerminalState::Completed => TurnAuditEventType::TurnCompleted,
        TerminalState::Failed | TerminalState::Cancelled => TurnAuditEventType::TurnFailed,
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-type

/// The first run of the finalization for the outcome.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-match-outcome
fn attempt_of(input: &FinalizeInput<'_>) -> Attempt {
    match input.outcome {
        TurnOutcome::Completed {
            usage, response_id, ..
        } => completed_attempt(input.run, *usage, response_id.clone()),
        TurnOutcome::Failed {
            code,
            detail,
            usage,
            response_id,
            ..
        } => failed_attempt(*code, detail.clone(), *usage, response_id.clone()),
        TurnOutcome::Cancelled => cancelled_attempt(input.run, input.text),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-match-outcome

/// `completed` (also `incomplete`): the pre-allocated assistant message, the
/// provider response id, no error code.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-final-completed
// @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-completed
fn completed_attempt(
    run: &TurnRun,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
) -> Attempt {
    Attempt {
        classification: TerminalClassification::Completed,
        columns: TerminalColumns {
            state: TerminalState::Completed,
            error_code: None,
            error_detail: None,
            assistant_message_id: Some(run.assistant_message_id),
            provider_response_id: response_id,
        },
        usage,
        with_message: true,
        audit_type: audit_type(TerminalState::Completed),
    }
}
// @cpt-end:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-completed
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-final-completed

/// `failed`: the code, the sanitized `error_detail`, no assistant message.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-final-failed
// @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-failed
fn failed_attempt(
    code: StreamErrorCode,
    detail: Option<String>,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
) -> Attempt {
    Attempt {
        classification: TerminalClassification::Failed {
            error_code: code.as_str().to_owned(),
        },
        columns: TerminalColumns {
            state: TerminalState::Failed,
            error_code: Some(code.as_str().to_owned()),
            error_detail: detail,
            assistant_message_id: None,
            provider_response_id: response_id,
        },
        usage,
        with_message: false,
        audit_type: audit_type(TerminalState::Failed),
    }
}
// @cpt-end:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-failed
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-final-failed

/// `cancelled`: the partial text is persisted only when it is not empty; no
/// usage is passed.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-final-cancelled
// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-finalize-cancelled
// @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-cancelled
fn cancelled_attempt(run: &TurnRun, text: &str) -> Attempt {
    let persist = !text.is_empty();
    Attempt {
        classification: TerminalClassification::Cancelled,
        columns: TerminalColumns {
            state: TerminalState::Cancelled,
            error_code: None,
            error_detail: None,
            assistant_message_id: persist.then_some(run.assistant_message_id),
            provider_response_id: None,
        },
        usage: None,
        with_message: persist,
        audit_type: audit_type(TerminalState::Cancelled),
    }
}
// @cpt-end:cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle:p1:inst-to-cancelled
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-finalize-cancelled
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-final-cancelled

impl ToolCounts {
    /// The `file_search` count of the turn: the knowledge call count when
    /// `search_knowledge` was offered, else the native `file_search` count;
    /// never a sum.
    fn file_search(self, knowledge_offered: bool) -> u32 {
        if knowledge_offered {
            self.knowledge_calls
        } else {
            self.native_file_search
        }
    }
}

/// Builds and runs the finalization transaction.
async fn run(
    deps: &TaskDeps,
    input: &FinalizeInput<'_>,
    attempt: &Attempt,
) -> Result<FinalizeResult, FinalizeError> {
    let run = input.run;
    let now = (deps.clock)();
    let usage = attempt.usage;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-build-finalize-request
    let file_search = input.counts.file_search(run.tools.knowledge.is_some());
    let request = FinalizeRequest {
        turn_id: run.turn_id,
        variant: FinalizeVariant::Stream {
            tenant_id: run.chat.tenant_id,
            period_keys: run.decision.period_keys,
            selected_model: run.chat.model.clone(),
        },
        classification: attempt.classification.clone(),
        columns: attempt.columns.clone(),
        usage,
        tool_calls: ToolCallCounts {
            web_search: input.counts.web_search,
            code_interpreter: input.counts.code_interpreter,
            file_search,
        },
        snapshot: SettlementSnapshot::Available(Box::new(run.decision.snapshot.clone())),
        now,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-build-finalize-request
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-precompute
    let assistant_message = attempt.with_message.then(|| AssistantMessageWrite {
        id: run.assistant_message_id,
        tenant_id: run.chat.tenant_id,
        chat_id: run.chat.id,
        request_id: run.request_id,
        content: input.text.to_owned(),
        provider_response_id: attempt.columns.provider_response_id.clone(),
        usage,
        model: run.decision.effective_model.clone(),
        created_at: now.max(run.user_message_at + time::Duration::microseconds(1)),
    });
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-fields
    let audit = TurnAuditEvent {
        event_type: attempt.audit_type,
        tenant_id: run.chat.tenant_id,
        requester_user_id: run.user_id,
        chat_id: run.chat.id,
        turn_id: run.turn_id,
        request_id: run.request_id,
        selected_model: run.chat.model.clone(),
        effective_model: run.decision.effective_model.clone(),
        usage: AuditUsage {
            input_tokens: usage.map_or(0, |u| u.input_tokens),
            output_tokens: usage.map_or(0, |u| u.output_tokens),
        },
        latency_ms: millis(run.accepted_at.elapsed()),
        tool_calls: AuditToolCalls {
            web_search: input.counts.web_search,
            file_search,
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: run.decision.quota_decision.as_str().to_owned(),
            },
        },
        timestamp: now,
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-fields
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-empty-content
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-empty-content
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-trigger-off
    let summary = Some(SummaryTrigger {
        plan: run.summary.clone(),
        completed: matches!(attempt.classification, TerminalClassification::Completed),
        tenant_id: run.chat.tenant_id,
        chat_id: run.chat.id,
        causing_request_id: run.request_id,
    });
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-trigger-off
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-precompute
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-run-finalize
    deps.turns
        .finalize(
            &request,
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-return-audit
            &FinalizeWrites {
                assistant_message,
                audit,
                summary,
            },
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-return-audit
        )
        .await
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-run-finalize
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The bookkeeping and the terminal event of a won finalization.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-won
async fn on_won(
    deps: &TaskDeps,
    input: &FinalizeInput<'_>,
    won: &FinalizeWon,
    began: Instant,
) -> Option<StreamEvent> {
    let run = input.run;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-turn
    deps.quota.record_finalized(won);
    deps.metrics
        .finalization_latency_ms(began.elapsed().as_secs_f64() * 1000.0);
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-return-trigger-result
    if let Some(label) = won.summary_trigger.and_then(TriggerResult::label) {
        deps.metrics.thread_summary_trigger(label);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-return-trigger-result
    let state = turn_state(won.terminal_state);
    let (code, usage) = match input.outcome {
        TurnOutcome::Completed { usage, .. } => (None, *usage),
        TurnOutcome::Failed { code, usage, .. } => (Some(code.as_str()), *usage),
        TurnOutcome::Cancelled => (None, None),
    };
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-record-cancel-effective
    if state == TurnState::Cancelled {
        deps.metrics.cancel_effective();
        deps.metrics.streams_aborted();
    }
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-record-cancel-effective
    let file_search = input.counts.file_search(run.tools.knowledge.is_some());
    let ttft = input
        .ttft_ms
        .map_or_else(|| "none".to_owned(), |millis| millis.to_string());
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-cost-record
    tracing::info!(
        chat_id = %run.chat.id,
        turn_id = %run.turn_id,
        request_id = %run.request_id,
        provider_id = %run.provider_id,
        effective_model = %run.decision.effective_model,
        state = state.as_str(),
        error_code = code.unwrap_or("none"),
        input_tokens = usage.map_or(0, |u| u.input_tokens),
        output_tokens = usage.map_or(0, |u| u.output_tokens),
        web_search_calls = input.counts.web_search,
        file_search_calls = file_search,
        code_interpreter_calls = input.counts.code_interpreter,
        ttft_ms = %ttft,
        latency_ms = millis(run.accepted_at.elapsed()),
        "turn finalized"
    );
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-cost-record
    match (state, input.outcome) {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-if-done
        (TurnState::Completed, _) => Some(done_event(deps, run, usage).await),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-if-done
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-build-error
        (TurnState::Failed, TurnOutcome::Failed { code, message, .. }) => {
            Some(error_event(*code, message))
        }
        (TurnState::Failed, _) => Some(error_event(
            StreamErrorCode::MessagePersistenceFailed,
            StreamErrorCode::MessagePersistenceFailed.generic_message(),
        )),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-build-error
        (_, _) => None,
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-case-won

async fn done_event(deps: &TaskDeps, run: &TurnRun, usage: Option<UsageTokens>) -> StreamEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-compute-warnings
    let quota_warnings = match deps
        .quota
        .quota_warnings(run.chat.tenant_id, run.user_id, &run.decision.limits)
        .await
    {
        Ok(warnings) => Some(warnings),
        Err(error) => {
            tracing::error!(%error, "the quota warnings could not be read");
            None
        }
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-compute-warnings
    let decision = &run.decision;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-build-done
    StreamEvent::Done(DoneEvent {
        usage: (
            usage.map_or(0, |u| u.input_tokens),
            usage.map_or(0, |u| u.output_tokens),
        ),
        effective_model: decision.effective_model.clone(),
        selected_model: run.chat.model.clone(),
        quota_decision: decision.quota_decision,
        downgrade_from: decision.downgrade_from.clone(),
        downgrade_reason: decision.downgrade_reason,
        quota_warnings,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-build-done
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "finalize_summary_tests.rs"]
mod summary_tests;
