//! The provider task
//! (`cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`): one spawned
//! task per turn that reads the provider stream, relays events through the
//! bounded channel without buffering, enforces the per-turn tool limits,
//! observes the cancellation token on every await and hands one outcome to
//! the finalization.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt as _;
use mini_chat_sdk::UsageTokens;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::citations::{OutputParts, map_citations};
use super::finalize::{
    FinalizeInput, FinalizeReport, TaskDeps, ToolCounts, TurnOutcome, TurnRun, finalize_stream,
};
use super::ports::{BuiltRequest, ProgressUpdate, ProviderStream, StreamMetrics, TurnRepository};
use super::sanitize::{error_detail, sanitize_error};
use super::types::{
    Annotation, DeltaKind, FunctionCall, ProviderEvent, ProviderFailure, StreamErrorCode,
    StreamEvent, ToolName, ToolPhase, TurnState,
};
use crate::domain::error::DomainError;
use crate::domain::service::quota::service::Clock;
use crate::domain::service::quota::tools::{ToolKind, ToolLimit};

/// The knowledge-search counters of a turn, shared between the provider task
/// and the tool-use handler. The handler updates them as it goes (a call is
/// counted before its retrieval runs), so the counts survive a handler that is
/// dropped by a cancellation or that ends the turn with a failure.
#[derive(Debug, Default)]
pub struct KnowledgeCounters {
    calls: AtomicU32,
    retrievals: AtomicU32,
    completed: AtomicU32,
}

impl KnowledgeCounters {
    /// `search_knowledge` calls answered or being answered, retrievals that
    /// failed and calls answered with the limit output included
    /// (`file_search_calls` of the usage and audit events).
    #[must_use]
    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Counts one `search_knowledge` call.
    pub fn record_call(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    /// Retrievals started (successful or failed); the per-message limit
    /// applies to this count.
    #[must_use]
    pub fn retrievals(&self) -> u32 {
        self.retrievals.load(Ordering::SeqCst)
    }

    /// Counts one retrieval that is about to run.
    pub fn record_retrieval(&self) {
        self.retrievals.fetch_add(1, Ordering::SeqCst);
    }

    /// Retrievals that succeeded (`chat_turns.file_search_completed_count`).
    #[must_use]
    pub fn completed(&self) -> u32 {
        self.completed.load(Ordering::SeqCst)
    }

    /// Counts one successful retrieval.
    pub fn record_completed(&self) {
        self.completed.fetch_add(1, Ordering::SeqCst);
    }
}

/// What a tool-use handler sees of the turn.
pub struct ToolUseContext<'a> {
    /// The turn.
    pub run: &'a TurnRun,
    /// Provider iteration that ended with the tool use (1 for the first).
    pub iteration: u32,
    /// The turn's cancellation token: every await of the handler that waits on
    /// the provider or a retrieval is raced against it.
    pub cancel: &'a CancellationToken,
    /// The request of the iteration that ended with the tool use: the first
    /// request of the turn, or the request the handler returned last.
    pub request: &'a BuiltRequest,
    /// The knowledge-search counters of the turn.
    pub knowledge: &'a KnowledgeCounters,
}

/// What the handler decided.
pub enum ToolUseOutcome {
    /// The next provider request was issued; the task continues on its stream.
    Continue {
        /// The next provider stream.
        stream: Box<dyn ProviderStream>,
        /// The request that opened `stream` (the `request` of the context of
        /// the next tool use).
        request: BuiltRequest,
    },
    /// The turn fails with this classified failure (`agentic_iterations_exceeded`,
    /// `unexpected_tool_use`); the usage of the last request is attached by the
    /// task when the failure carries none.
    Fail(ProviderFailure),
}

/// The agentic-loop extension point of the provider task
/// (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`, slice 7): called
/// when a provider iteration ends with function calls.
#[async_trait]
pub trait ToolUseHandler: Send + Sync {
    /// Answers the calls and issues the next provider request, or fails the
    /// turn.
    async fn handle(
        &self,
        context: &ToolUseContext<'_>,
        calls: &[FunctionCall],
        usage: Option<UsageTokens>,
    ) -> ToolUseOutcome;
}

/// The handler of a build without knowledge search: every tool use is
/// `unexpected_tool_use`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectToolUse;

#[async_trait]
impl ToolUseHandler for RejectToolUse {
    async fn handle(
        &self,
        _context: &ToolUseContext<'_>,
        _calls: &[FunctionCall],
        _usage: Option<UsageTokens>,
    ) -> ToolUseOutcome {
        ToolUseOutcome::Fail(ProviderFailure::generic(StreamErrorCode::UnexpectedToolUse))
    }
}

/// Progress writes of a running turn are throttled to this interval.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// The result of driving the provider side of a turn.
enum Driven {
    /// The turn ends with this outcome and is finalized.
    Outcome(TurnOutcome),
    /// Another finalizer (the orphan watchdog) moved the turn: nothing is
    /// finalized, settled or sent.
    Moved,
}

/// `mini_chat_active_streams` while the task lives: added on creation,
/// subtracted on drop, so every end of the task (also a panic or an abort)
/// balances it.
struct ActiveStream {
    metrics: Arc<dyn StreamMetrics>,
}

impl ActiveStream {
    fn new(metrics: Arc<dyn StreamMetrics>) -> Self {
        metrics.active_streams_add(1);
        Self { metrics }
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-end-metrics
impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.metrics.active_streams_add(-1);
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-end-metrics

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-cancellation:p1
/// Runs the turn after the go-signal.
///
/// The task waits for `go` first: the setup task fires it after it handed the
/// relay handle to the handler (or dropped it, which cancels `cancel`), so a
/// handler that is already gone is seen before the provider call. A closed
/// `go` channel counts as no go.
pub async fn run_provider_task(
    deps: Arc<TaskDeps>,
    run: TurnRun,
    tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
    go: oneshot::Receiver<()>,
) {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-start-metrics
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-start
    deps.metrics
        .stream_started(&run.provider_id, &run.decision.effective_model);
    let _active = ActiveStream::new(Arc::clone(&deps.metrics));
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-start
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-start-metrics
    let mut state = State::new(&deps, &run);
    let driven = drive(&deps, &run, &tx, &cancel, go, &mut state).await;
    // A turn another finalizer moved ends here: nothing is finalized, settled
    // or sent, and the relay sends `stream_interrupted`.
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-return-task
    let Driven::Outcome(outcome) = driven else {
        return;
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-return-task
    state.progress.flush().await;
    let ttft_ms = state.ttft_ms();
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-finalize
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-finalize-provider-outcome
    let report = finalize_stream(
        &deps,
        &FinalizeInput {
            run: &run,
            outcome: &outcome,
            text: &state.text,
            counts: state.counts,
            ttft_ms,
        },
    )
    .await;
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-finalize-provider-outcome
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-finalize
    record_outcome(&deps, &run, &outcome, &report);
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-send-terminal
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-return-silent
    if let Some(event) = report.event
        && !cancel.is_cancelled()
    {
        send(&tx, &cancel, event).await;
    }
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-return-silent
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-send-terminal
}

/// What the task accumulates while it reads.
struct State {
    text: String,
    parts: OutputParts,
    annotations: Vec<Annotation>,
    counts: ToolCounts,
    web_starts: u32,
    code_starts: u32,
    opened_at: Instant,
    first_content: Option<Instant>,
    progress: ProgressWriter,
    /// The request of the current provider iteration.
    request: BuiltRequest,
    knowledge: KnowledgeCounters,
    /// The completed knowledge retrievals already handed to the progress writer.
    knowledge_persisted: u32,
}

impl State {
    fn new(deps: &TaskDeps, run: &TurnRun) -> Self {
        Self {
            text: String::new(),
            parts: OutputParts::new(),
            annotations: Vec::new(),
            counts: ToolCounts::default(),
            web_starts: 0,
            code_starts: 0,
            opened_at: Instant::now(),
            first_content: None,
            progress: ProgressWriter::new(deps, run),
            request: run.request.clone(),
            knowledge: KnowledgeCounters::default(),
            knowledge_persisted: 0,
        }
    }

    fn ttft_ms(&self) -> Option<u64> {
        self.first_content.map(|at| {
            u64::try_from(at.duration_since(self.opened_at).as_millis()).unwrap_or(u64::MAX)
        })
    }
}

/// Sends `event` unless the turn is cancelled first; `false` when the client
/// is gone (cancelled or the receiver was dropped).
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-no-buffering
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-send-failed
async fn send(
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    event: StreamEvent,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = tx.send(event) => sent.is_ok(),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-send-failed
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-no-buffering

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The client left: the pending operation was dropped, record the time to
/// abort and end with `cancelled`.
// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-abort-provider
// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-else-cancel
fn aborted(deps: &TaskDeps, observed: Instant) -> Driven {
    deps.metrics.time_to_abort_ms(millis(observed.elapsed()));
    Driven::Outcome(TurnOutcome::Cancelled)
}
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-else-cancel
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-abort-provider

fn failed_from(failure: ProviderFailure) -> TurnOutcome {
    TurnOutcome::Failed {
        code: failure.code,
        message: failure.message,
        detail: failure.detail,
        usage: failure.usage,
        response_id: None,
    }
}

fn limit_failure(code: &str) -> TurnOutcome {
    let code = if code == "web_search_calls_exceeded" {
        StreamErrorCode::WebSearchCallsExceeded
    } else {
        StreamErrorCode::CodeInterpreterCallsExceeded
    };
    TurnOutcome::Failed {
        code,
        message: code.generic_message().to_owned(),
        detail: None,
        usage: None,
        response_id: None,
    }
}

/// The provider's own error, with the sanitized text for the client.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-provider-event
fn provider_error(code: Option<&str>, message: &str, usage: Option<UsageTokens>) -> TurnOutcome {
    let sanitized = sanitize_error(message);
    let client_text = if sanitized.trim().is_empty() {
        StreamErrorCode::ProviderError.generic_message().to_owned()
    } else {
        sanitized
    };
    let detail = error_detail(code, message);
    TurnOutcome::Failed {
        code: StreamErrorCode::ProviderError,
        message: client_text,
        detail: (!detail.is_empty()).then_some(detail),
        usage,
        response_id: None,
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-provider-event

/// The provider stream of the turn: one per provider iteration.
type Stream = Box<dyn ProviderStream>;

/// The in-flight liveness check of the turn.
type LivenessCheck = JoinHandle<Result<Option<TurnState>, DomainError>>;

/// The result of a liveness check task.
type LivenessResult = Result<Result<Option<TurnState>, DomainError>, tokio::task::JoinError>;

/// What the loop does after one provider event.
enum Flow {
    /// Read the next event of this stream.
    Next(Stream),
    /// The provider side is over (the stream was dropped).
    End(Driven),
}

/// The borrowed parts of the turn the event handlers share.
struct Drive<'a> {
    deps: &'a TaskDeps,
    run: &'a TurnRun,
    tx: &'a mpsc::Sender<StreamEvent>,
    cancel: &'a CancellationToken,
    state: &'a mut State,
    iteration: u32,
}

async fn drive(
    deps: &Arc<TaskDeps>,
    run: &TurnRun,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    go: oneshot::Receiver<()>,
    state: &mut State,
) -> Driven {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-cancelled-before-call
    let went = tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        go = go => go.is_ok(),
    };
    if !went || cancel.is_cancelled() {
        return Driven::Outcome(TurnOutcome::Cancelled);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-cancelled-before-call

    state.opened_at = Instant::now();
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-proxy-request
    let opened = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        opened = deps.client.open(&run.ctx, &run.request) => Some(opened),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-proxy-request
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-call-failed
    let mut stream = match opened {
        None => return aborted(deps, Instant::now()),
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-call-failed-outcome
        Some(Err(failure)) => return Driven::Outcome(failed_from(failure)),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-call-failed-outcome
        Some(Ok(stream)) => stream,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-call-failed

    let mut liveness =
        tokio::time::interval_at(Instant::now() + deps.ping_interval, deps.ping_interval);
    liveness.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut check: Option<LivenessCheck> = None;
    let mut session = Drive {
        deps,
        run,
        tx,
        cancel,
        state,
        iteration: 1,
    };

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-while-streaming
    loop {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-race-cancel
        let step = tokio::select! {
            biased;
            () = cancel.cancelled() => Step::Cancelled,
            _ = liveness.tick() => Step::Tick,
            checked = poll_check(&mut check), if check.is_some() => Step::Checked(checked),
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-read-event
            event = stream.next_event() => Step::Event(event),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-read-event
        };
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-race-cancel
        let observed = Instant::now();
        match step {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-cancelled
            Step::Cancelled => {
                drop(stream);
                return aborted(deps, observed);
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-cancelled
            Step::Tick => {
                if check.is_none() {
                    check = Some(spawn_check(deps, run));
                }
            }
            Step::Checked(result) => {
                check = None;
                if moved_by_another_path(run, result) {
                    drop(stream);
                    return Driven::Moved;
                }
            }
            Step::Event(event) => match session.on_event(stream, event, observed).await {
                Flow::Next(next) => stream = next,
                Flow::End(driven) => return driven,
            },
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-while-streaming
}

/// `true` when the liveness check found the turn finalized by another path;
/// a failed check is logged and retried at the next interval.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-turn-moved
fn moved_by_another_path(run: &TurnRun, result: LivenessResult) -> bool {
    let checked = result
        .map_err(|error| error.to_string())
        .and_then(|checked| checked.map_err(|error| error.to_string()));
    match checked {
        Ok(Some(TurnState::Running)) => false,
        Ok(state_now) => {
            tracing::info!(turn_id = %run.turn_id, ?state_now, "the turn was finalized by another path, the stream ends");
            true
        }
        Err(error) => {
            tracing::warn!(%error, "the liveness check failed, it is retried at the next interval");
            false
        }
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-turn-moved

impl Drive<'_> {
    /// Relays a content event; `false` when the client is gone.
    async fn relay(&mut self, event: StreamEvent, received: Instant) -> bool {
        relay(
            self.deps,
            self.run,
            self.tx,
            self.cancel,
            self.state,
            event,
            received,
        )
        .await
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-match-internal
    async fn on_event(
        &mut self,
        stream: Stream,
        event: Option<Result<ProviderEvent, ProviderFailure>>,
        observed: Instant,
    ) -> Flow {
        let event = match event {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-eof
            None => {
                drop(stream);
                return Flow::End(Driven::Outcome(TurnOutcome::Failed {
                    code: StreamErrorCode::ProviderError,
                    message: StreamErrorCode::ProviderError.generic_message().to_owned(),
                    detail: None,
                    usage: None,
                    response_id: None,
                }));
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-if-eof
            Some(Err(failure)) => {
                drop(stream);
                return Flow::End(Driven::Outcome(failed_from(failure)));
            }
            Some(Ok(event)) => event,
        };
        match event {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-relay-text
            ProviderEvent::TextDelta {
                text,
                item_id,
                content_index,
            } => {
                self.state.text.push_str(&text);
                self.state
                    .parts
                    .entry((item_id, content_index))
                    .or_default()
                    .push_str(&text);
                let event = StreamEvent::Delta {
                    kind: DeltaKind::Text,
                    content: text,
                };
                if !self.relay(event, observed).await {
                    return self.client_gone(stream);
                }
                self.state.progress.touch();
                Flow::Next(stream)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-relay-text
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-relay-reasoning
            ProviderEvent::ReasoningDelta { text } => {
                let event = StreamEvent::Delta {
                    kind: DeltaKind::Reasoning,
                    content: text,
                };
                if self.relay(event, observed).await {
                    self.state.progress.touch();
                    Flow::Next(stream)
                } else {
                    self.client_gone(stream)
                }
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-relay-reasoning
            ProviderEvent::ToolStart { name, details } => {
                self.on_tool_start(stream, name, details, observed).await
            }
            ProviderEvent::ToolDone { name, details } => {
                self.on_tool_done(stream, name, details, observed).await
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-collect-annotation
            ProviderEvent::Annotation(annotation) => {
                self.state.annotations.push(annotation);
                Flow::Next(stream)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-collect-annotation
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-outcome-completed
            // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-if-terminal-received
            ProviderEvent::Completed {
                usage,
                response_id,
                incomplete_reason,
            } => {
                drop(stream);
                if incomplete_reason.is_none() {
                    send_citations(self.run, self.tx, self.cancel, self.state).await;
                }
                Flow::End(Driven::Outcome(TurnOutcome::Completed {
                    usage,
                    response_id,
                    incomplete_reason,
                }))
            }
            // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-if-terminal-received
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-outcome-completed
            ProviderEvent::ToolUse { calls, usage } => {
                self.on_tool_use(stream, &calls, usage).await
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-outcome-error
            ProviderEvent::ProviderError {
                code,
                message,
                usage,
            } => {
                drop(stream);
                Flow::End(Driven::Outcome(provider_error(
                    code.as_deref(),
                    &message,
                    usage,
                )))
            } // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-outcome-error
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-match-internal

    /// The relay or a channel send found the client gone.
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-failed-send
    fn client_gone(&self, stream: Stream) -> Flow {
        drop(stream);
        Flow::End(aborted(self.deps, Instant::now()))
    }
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-failed-send

    async fn on_tool_start(
        &mut self,
        stream: Stream,
        name: ToolName,
        details: serde_json::Value,
        observed: Instant,
    ) -> Flow {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-limited-start
        if let Some(code) = limit_breach(self.deps, self.state, name) {
            drop(stream);
            return Flow::End(Driven::Outcome(limit_failure(code)));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-limited-start
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-other-start
        let event = StreamEvent::Tool {
            phase: ToolPhase::Start,
            name,
            details,
        };
        if !self.relay(event, observed).await {
            return self.client_gone(stream);
        }
        self.state.progress.touch();
        Flow::Next(stream)
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-other-start
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-tool-done
    async fn on_tool_done(
        &mut self,
        stream: Stream,
        name: ToolName,
        details: serde_json::Value,
        observed: Instant,
    ) -> Flow {
        let state = &mut *self.state;
        match name {
            ToolName::WebSearch => {
                state.counts.web_search += 1;
                state.progress.set_web(state.counts.web_search);
            }
            ToolName::CodeInterpreter => {
                state.counts.code_interpreter += 1;
                state.progress.set_code(state.counts.code_interpreter);
            }
            ToolName::FileSearch => {
                state.counts.native_file_search += 1;
                state.progress.set_file(state.counts.native_file_search);
            }
            _ => {}
        }
        let event = StreamEvent::Tool {
            phase: ToolPhase::Done,
            name,
            details,
        };
        if self.relay(event, observed).await {
            Flow::Next(stream)
        } else {
            self.client_gone(stream)
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-tool-done

    /// Hands the function calls to the agentic-loop handler; its next provider
    /// stream replaces the current one.
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-tool-use
    async fn on_tool_use(
        &mut self,
        stream: Stream,
        calls: &[FunctionCall],
        usage: Option<UsageTokens>,
    ) -> Flow {
        drop(stream);
        let context = ToolUseContext {
            run: self.run,
            iteration: self.iteration,
            cancel: self.cancel,
            request: &self.state.request,
            knowledge: &self.state.knowledge,
        };
        let handled = tokio::select! {
            biased;
            () = self.cancel.cancelled() => None,
            outcome = self.deps.tool_use.handle(&context, calls, usage) => Some(outcome),
        };
        // The handler counts as it goes: keep the counts whatever it did, and
        // persist the successful retrievals with `last_progress_at`.
        self.state.counts.knowledge_calls = self.state.knowledge.calls();
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-retrieve
        let completed = self.state.knowledge.completed();
        if completed != self.state.knowledge_persisted {
            self.state.knowledge_persisted = completed;
            self.state.progress.set_file(completed);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-retrieve
        match handled {
            None => Flow::End(aborted(self.deps, Instant::now())),
            Some(ToolUseOutcome::Continue {
                stream: next,
                request,
            }) => {
                self.state.request = request;
                self.iteration += 1;
                Flow::Next(next)
            }
            Some(ToolUseOutcome::Fail(mut failure)) => {
                failure.usage = failure.usage.or(usage);
                Flow::End(Driven::Outcome(failed_from(failure)))
            }
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-case-tool-use
}

enum Step {
    Cancelled,
    Tick,
    Checked(LivenessResult),
    Event(Option<Result<ProviderEvent, ProviderFailure>>),
}

async fn poll_check(check: &mut Option<LivenessCheck>) -> LivenessResult {
    match check {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

/// The liveness check runs off the relay path: one query, tenant-scoped and
/// bound to the chat.
fn spawn_check(deps: &TaskDeps, run: &TurnRun) -> LivenessCheck {
    let turns = Arc::clone(&deps.turns);
    let (tenant, chat, turn) = (run.chat.tenant_id, run.chat.id, run.turn_id);
    tokio::spawn(async move { turns.turn_state(tenant, chat, turn).await })
}

/// Counts a `web_search` / `code_interpreter` start and returns the limit
/// code when the per-turn limit is exceeded.
fn limit_breach(deps: &TaskDeps, state: &mut State, name: ToolName) -> Option<&'static str> {
    let (kind, count) = match name {
        ToolName::WebSearch => {
            state.web_starts += 1;
            (ToolKind::WebSearch, state.web_starts)
        }
        ToolName::CodeInterpreter => {
            state.code_starts += 1;
            (ToolKind::CodeInterpreter, state.code_starts)
        }
        _ => return None,
    };
    match deps.quota.tool_call_limit(kind, count) {
        ToolLimit::Within => None,
        ToolLimit::Exceeded { error_code } => Some(error_code),
    }
}

/// Sends a content event; records the time to first token on the first one.
/// `false` when the client is gone.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-ttft
async fn relay(
    deps: &TaskDeps,
    run: &TurnRun,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    state: &mut State,
    event: StreamEvent,
    received: Instant,
) -> bool {
    let first = state.first_content.is_none();
    if first {
        state.first_content = Some(received);
    }
    let sent = send(tx, cancel, event).await;
    if first && sent {
        let model = &run.decision.effective_model;
        deps.metrics.ttft_provider_ms(
            &run.provider_id,
            model,
            millis(received.duration_since(state.opened_at)),
        );
        deps.metrics
            .ttft_overhead_ms(&run.provider_id, model, millis(received.elapsed()));
    }
    sent
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-ttft

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-send-citations
async fn send_citations(
    run: &TurnRun,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    state: &State,
) {
    let items = map_citations(&state.annotations, &state.parts, &run.tools.citation_map);
    if !items.is_empty() {
        let _ = send(tx, cancel, StreamEvent::Citations(items)).await;
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-send-citations

/// `incomplete` reasons are labels: unknown ones count as `other`.
fn reason_label(reason: &str) -> &str {
    match reason {
        "max_output_tokens" | "max_tokens" | "content_filter" => reason,
        _ => "other",
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-outcome
fn record_outcome(deps: &TaskDeps, run: &TurnRun, outcome: &TurnOutcome, report: &FinalizeReport) {
    let (provider, model) = (&run.provider_id, &run.decision.effective_model);
    match report.state {
        Some(TurnState::Completed) => {
            deps.metrics.stream_completed(provider, model);
            if let TurnOutcome::Completed {
                incomplete_reason: Some(reason),
                ..
            } = outcome
            {
                deps.metrics
                    .stream_incomplete(provider, model, reason_label(reason));
            }
        }
        Some(TurnState::Failed) => {
            let code = match (&report.event, outcome) {
                (Some(StreamEvent::Error(error)), _) => error.code,
                (_, TurnOutcome::Failed { code, .. }) => *code,
                _ => StreamErrorCode::ProviderError,
            };
            deps.metrics.stream_failed(provider, model, code.as_str());
        }
        Some(TurnState::Cancelled) => {}
        Some(TurnState::Running) | None => return,
    }
    deps.metrics
        .stream_total_latency_ms(provider, model, millis(run.accepted_at.elapsed()));
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-outcome

// ---- progress writes ------------------------------------------------------------------------

type ProgressWrite = JoinHandle<Result<(), DomainError>>;

/// Single-statement progress writes, dispatched off the relay path with at
/// most one in flight; a failed write is logged and retried with the next
/// event, and the changes of a busy period coalesce into the next write.
struct ProgressWriter {
    turns: Arc<dyn TurnRepository>,
    clock: Clock,
    tenant: Uuid,
    chat: Uuid,
    turn: Uuid,
    last_refresh: Instant,
    pending: ProgressUpdate,
    in_flight: Option<(ProgressWrite, ProgressUpdate)>,
}

fn is_empty(update: &ProgressUpdate) -> bool {
    update.at.is_none()
        && update.web_search_done.is_none()
        && update.code_interpreter_done.is_none()
        && update.file_search_done.is_none()
}

fn has_counts(update: &ProgressUpdate) -> bool {
    update.web_search_done.is_some()
        || update.code_interpreter_done.is_some()
        || update.file_search_done.is_some()
}

/// `first` filled in by `second` where `first` has nothing.
fn merge(first: ProgressUpdate, second: ProgressUpdate) -> ProgressUpdate {
    ProgressUpdate {
        at: first.at.or(second.at),
        web_search_done: first.web_search_done.or(second.web_search_done),
        code_interpreter_done: first.code_interpreter_done.or(second.code_interpreter_done),
        file_search_done: first.file_search_done.or(second.file_search_done),
    }
}

impl ProgressWriter {
    fn new(deps: &TaskDeps, run: &TurnRun) -> Self {
        Self {
            turns: Arc::clone(&deps.turns),
            clock: Arc::clone(&deps.clock),
            tenant: run.chat.tenant_id,
            chat: run.chat.id,
            turn: run.turn_id,
            last_refresh: run.accepted_at,
            pending: ProgressUpdate::default(),
            in_flight: None,
        }
    }

    fn set_web(&mut self, count: u32) {
        self.pending.web_search_done = Some(count);
        self.after_counts();
    }

    fn set_code(&mut self, count: u32) {
        self.pending.code_interpreter_done = Some(count);
        self.after_counts();
    }

    fn set_file(&mut self, count: u32) {
        self.pending.file_search_done = Some(count);
        self.after_counts();
    }

    fn after_counts(&mut self) {
        self.pending.at = Some((self.clock)());
        self.pump();
    }

    /// A text or tool event happened: refresh `last_progress_at` when 30 s
    /// passed since the last refresh.
    fn touch(&mut self) {
        if self.pending.at.is_none() && self.last_refresh.elapsed() >= PROGRESS_INTERVAL {
            self.pending.at = Some((self.clock)());
        }
        self.pump();
    }

    fn reap(&mut self) {
        let finished = self
            .in_flight
            .as_ref()
            .is_some_and(|(handle, _)| handle.is_finished());
        if !finished {
            return;
        }
        let Some((handle, update)) = self.in_flight.take() else {
            return;
        };
        match handle.now_or_never() {
            Some(Ok(Ok(()))) | None => {}
            Some(Ok(Err(_))) => self.pending = merge(self.pending, update),
            Some(Err(error)) => tracing::warn!(%error, "a progress write did not complete"),
        }
    }

    fn pump(&mut self) {
        self.reap();
        if self.in_flight.is_none() && !is_empty(&self.pending) {
            let update = std::mem::take(&mut self.pending);
            if update.at.is_some() {
                self.last_refresh = Instant::now();
            }
            let turns = Arc::clone(&self.turns);
            let (tenant, chat, turn) = (self.tenant, self.chat, self.turn);
            let handle = tokio::spawn(async move {
                let result = turns.record_progress(tenant, chat, turn, &update).await;
                if let Err(error) = &result {
                    tracing::warn!(%error, "a progress write failed, it is retried with the next event");
                }
                result
            });
            self.in_flight = Some((handle, update));
        }
    }

    /// Waits for the write in flight; a failed one is merged back into the
    /// pending update.
    async fn settle_in_flight(&mut self) {
        let Some((handle, update)) = self.in_flight.take() else {
            return;
        };
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "a progress write failed");
                self.pending = merge(self.pending, update);
            }
            Err(error) => tracing::warn!(%error, "a progress write did not complete"),
        }
    }

    /// Writes the outstanding counts before the finalization: a progress write
    /// is `WHERE state = 'running'`, so it only has an effect before it.
    async fn flush(&mut self) {
        self.settle_in_flight().await;
        if !has_counts(&self.pending) {
            return;
        }
        let update = std::mem::take(&mut self.pending);
        if let Err(error) = self
            .turns
            .record_progress(self.tenant, self.chat, self.turn, &update)
            .await
        {
            tracing::warn!(%error, "the final progress write failed");
        }
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
