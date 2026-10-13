//! The provider task of a live turn (DESIGN section 3.3 "SSE Event
//! Definitions", "SSE stream close rules", section 4 web search / code
//! interpreter mid-turn limits, section 5.7 "Terminal SSE Event Emission
//! Guard").
//!
//! The task sends `stream_started`, pings until the first content event,
//! relays deltas and tool events as they arrive, keeps the turn's liveness
//! columns current and stops at the first terminal provider event. It then
//! finalizes the turn and, only after a CAS-winning commit, sends the terminal
//! event: `done` for a committed `completed`, `error` otherwise. A client
//! disconnect (guard dropped, receiver gone) cancels the provider stream and
//! finalizes the turn as `cancelled` without sending anything. When the CAS is
//! lost the task ends without a terminal event; the relay then reports
//! `stream_interrupted`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use mini_chat_sdk::{QuotaPolicyDecision, UsageTokens};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use uuid::Uuid;

use super::events::{
    AGENTIC_ITERATIONS_EXCEEDED, CODE_INTERPRETER_CALLS_EXCEEDED, Citation, CitationSource,
    DeltaKind, DoneData, FINALIZATION_FAILED, StreamEvent, StreamStartedData, TextSpan,
    ThreadSummaryInfo, ToolPhase, UNEXPECTED_TOOL_USE, UsageCounts, WEB_SEARCH_CALLS_EXCEEDED,
};
use super::{DisconnectGuard, StreamStart};
use crate::domain::enums::TurnState;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    InputItem, LlmClient, LlmEvent, LlmRequest, RawCitation, ResolvedProvider,
};
use crate::domain::sanitize::sanitize_provider_message;
use crate::domain::services::finalization_service::{
    FinalizationService, FinalizeInput, FinalizeResult, MESSAGE_PERSISTENCE_FAILED,
    SummaryCandidate, TerminalOutcome, ToolCounts,
};
use crate::domain::services::quota_service::{PeriodStarts, QuotaDecisionKind};
use crate::domain::time::db_now;
use crate::infra::db::repos::turn_repo::{self, CompletedToolCounts};
use crate::infra::llm::knowledge::{
    INVALID_ARGUMENTS, KnowledgeRetriever, KnowledgeTarget, SEARCH_FAILED, SEARCH_KNOWLEDGE,
    SEARCH_LIMIT_REACHED, format_output, parse_args,
};
use crate::infra::llm::types::PROVIDER_ERROR;

/// `last_progress_at` is refreshed at most this often on deltas and tool
/// starts (tool completions always write the counters).
const PROGRESS_REFRESH: Duration = Duration::from_secs(30);

const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";
const FILE_SEARCH: &str = "file_search";

const NO_TERMINAL_MESSAGE: &str = "The provider stream ended unexpectedly";
const FINALIZATION_FAILED_MESSAGE: &str = "The response could not be saved";
const PERSISTENCE_FAILED_MESSAGE: &str = "The response could not be persisted";
const UNEXPECTED_TOOL_USE_MESSAGE: &str = "The model requested a tool that is not available";
const AGENTIC_ITERATIONS_MESSAGE: &str =
    "The model exceeded the knowledge search iteration limit for one message";

/// Services and limits the task uses.
pub(super) struct TaskDeps {
    pub llm: Arc<dyn LlmClient>,
    pub finalization: Arc<FinalizationService>,
    pub db: Arc<DBProvider<DomainError>>,
    pub ping_interval: Duration,
    pub web_search_max_calls: u32,
    pub code_interpreter_max_calls: u32,
    pub channel_capacity: usize,
}

/// Everything known about a committed running turn.
pub(super) struct TurnRun {
    pub turn_id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub user_id: Uuid,
    /// Pre-allocated assistant message id.
    pub assistant_message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub premium: bool,
    pub policy_version: u64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub max_output_tokens_applied: i64,
    pub minimal_generation_floor_applied: i64,
    pub periods: PeriodStarts,
    pub quota_decision: QuotaDecisionKind,
    pub downgrade_reason: Option<&'static str>,
    pub summary_trigger: bool,
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
    pub citation_map: HashMap<String, (Uuid, String)>,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    /// Set when `search_knowledge` is offered in the request.
    pub knowledge: Option<KnowledgeRun>,
}

/// Knowledge search of a turn (DESIGN section 4 "Knowledge Search").
pub(super) struct KnowledgeRun {
    pub retriever: Arc<dyn KnowledgeRetriever>,
    pub target: KnowledgeTarget,
    pub max_calls: u32,
    pub top_k: usize,
    pub max_chunk_chars: usize,
}

/// A function tool call reported by the current provider request.
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl TurnRun {
    fn downgrade_from(&self) -> Option<String> {
        (self.quota_decision == QuotaDecisionKind::Downgrade).then(|| self.selected_model.clone())
    }

    fn policy_decision(&self) -> QuotaPolicyDecision {
        QuotaPolicyDecision {
            decision: self.quota_decision.as_str().to_owned(),
            downgrade_from: self.downgrade_from(),
            downgrade_reason: self.downgrade_reason.map(str::to_owned),
        }
    }

    /// File citations are mapped to live, ready attachments of the chat
    /// (unknown ids are dropped); web citations pass through.
    fn map_citation(&self, raw: RawCitation) -> Option<Citation> {
        match raw {
            RawCitation::File {
                provider_file_id, ..
            } => self
                .citation_map
                .get(&provider_file_id)
                .map(|(id, filename)| Citation {
                    source: CitationSource::File,
                    title: filename.clone(),
                    url: None,
                    attachment_id: Some(*id),
                    snippet: String::new(),
                    span: None,
                }),
            RawCitation::Web {
                url,
                title,
                snippet,
                span,
            } => Some(Citation {
                source: CitationSource::Web,
                title,
                url: Some(url),
                attachment_id: None,
                snippet,
                span: span.map(|(start, end)| TextSpan { start, end }),
            }),
        }
    }
}

/// Spawns the provider task of `turn`.
pub(super) fn spawn(deps: TaskDeps, turn: TurnRun) -> StreamStart {
    let (tx, rx) = mpsc::channel(deps.channel_capacity.max(1));
    let cancel = CancellationToken::new();
    let guard = DisconnectGuard::new(&cancel);
    tokio::spawn(run(deps, turn, tx, cancel));
    StreamStart::Live(rx, guard)
}

/// How the provider stream ended.
enum Ending {
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
    Cancelled,
}

impl Ending {
    fn failed(code: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            code: code.to_owned(),
            message: message.into(),
            usage: None,
        }
    }
}

/// Accumulated state of the stream.
struct Progress {
    text: String,
    content_started: bool,
    ping_at: Instant,
    last_progress_write: Instant,
    web_search_starts: u32,
    code_interpreter_starts: u32,
    completed: ToolCounts,
    citations: Vec<Citation>,
    /// Function calls of the current provider request.
    function_calls: Vec<PendingCall>,
    /// `search_knowledge` retrievals attempted (counted before they run).
    knowledge_calls: u32,
    /// Attempted retrievals that failed (not in `completed.file_search`).
    knowledge_failed: u32,
}

impl Progress {
    /// Tool counts of the usage and audit events: successful retrievals are
    /// in `completed.file_search`, failed ones are added here.
    fn reported_tools(&self) -> ToolCounts {
        let mut tools = self.completed;
        tools.file_search = tools.file_search.saturating_add(self.knowledge_failed);
        tools
    }
}

async fn run(
    deps: TaskDeps,
    turn: TurnRun,
    tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut driver = Driver {
        deps: &deps,
        turn: &turn,
        tx: &tx,
        cancel: &cancel,
        p: Progress {
            text: String::new(),
            content_started: false,
            ping_at: started + deps.ping_interval,
            last_progress_write: started,
            web_search_starts: 0,
            code_interpreter_starts: 0,
            completed: ToolCounts::default(),
            citations: Vec::new(),
            function_calls: Vec::new(),
            knowledge_calls: 0,
            knowledge_failed: 0,
        },
    };
    let stream_started = StreamEvent::StreamStarted(StreamStartedData {
        request_id: turn.request_id,
        message_id: turn.assistant_message_id,
        is_new_turn: true,
        thread_summary_applied: turn.thread_summary_applied,
    });
    let ending = if tx.send(stream_started).await.is_err() {
        Ending::Cancelled
    } else {
        driver.drive().await
    };
    let latency = started.elapsed();
    let p = driver.p;
    finish(&deps, &turn, &tx, ending, p, latency).await;
}

enum Wake {
    Cancelled,
    Ping,
    Event(Option<LlmEvent>),
}

struct Driver<'a> {
    deps: &'a TaskDeps,
    turn: &'a TurnRun,
    tx: &'a mpsc::Sender<StreamEvent>,
    cancel: &'a CancellationToken,
    p: Progress,
}

impl Driver<'_> {
    /// Runs provider requests until one ends without function calls (the
    /// knowledge-search agentic loop issues one request per iteration).
    async fn drive(&mut self) -> Ending {
        let mut request = self.turn.request.clone();
        let mut iteration: u32 = 1;
        loop {
            let ending = self.drive_request(&request).await;
            let calls = std::mem::take(&mut self.p.function_calls);
            let usage = match &ending {
                Ending::Completed { usage, .. } if !calls.is_empty() => *usage,
                _ => return ending,
            };
            if let Some(ending) = self.tool_round(calls, usage, iteration, &mut request).await {
                return ending;
            }
            iteration = iteration.saturating_add(1);
        }
    }

    /// Handles the function calls of iteration `iteration`: appends the calls
    /// and their outputs to `request`. `Some` ends the turn: no knowledge
    /// search or another function (`unexpected_tool_use`), the iteration cap
    /// (`agentic_iterations_exceeded`), or a disconnect.
    async fn tool_round(
        &mut self,
        calls: Vec<PendingCall>,
        usage: Option<UsageTokens>,
        iteration: u32,
        request: &mut LlmRequest,
    ) -> Option<Ending> {
        let turn = self.turn;
        let unexpected = calls.iter().find(|c| c.name != SEARCH_KNOWLEDGE);
        let Some(k) = turn.knowledge.as_ref().filter(|_| unexpected.is_none()) else {
            let name = unexpected.map_or(SEARCH_KNOWLEDGE, |c| c.name.as_str());
            tracing::warn!(turn_id = %turn.turn_id, tool = %name, "unexpected function tool use");
            return Some(Ending::Failed {
                code: UNEXPECTED_TOOL_USE.to_owned(),
                message: UNEXPECTED_TOOL_USE_MESSAGE.to_owned(),
                usage,
            });
        };
        if iteration >= k.max_calls.saturating_add(2) {
            return Some(Ending::Failed {
                code: AGENTIC_ITERATIONS_EXCEEDED.to_owned(),
                message: AGENTIC_ITERATIONS_MESSAGE.to_owned(),
                usage,
            });
        }
        request
            .input
            .extend(calls.iter().map(|c| InputItem::FunctionCall {
                call_id: c.call_id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            }));
        for call in calls {
            let output = match self.search(k, &call.arguments).await {
                Ok(output) => output,
                Err(ending) => return Some(ending),
            };
            request.input.push(InputItem::FunctionCallOutput {
                call_id: call.call_id,
                output,
            });
        }
        None
    }

    /// One `search_knowledge` call: the tool output for the model. Beyond
    /// `max_calls` retrievals the output is "search limit reached"; a failed
    /// retrieval or unusable arguments get an error output. `Err(Cancelled)`
    /// on disconnect.
    async fn search(&mut self, k: &KnowledgeRun, arguments: &str) -> Result<String, Ending> {
        if self.p.knowledge_calls >= k.max_calls {
            return Ok(SEARCH_LIMIT_REACHED.to_owned());
        }
        self.p.knowledge_calls += 1;
        let Some(args) = parse_args(arguments, k.top_k) else {
            self.p.knowledge_failed += 1;
            return Ok(INVALID_ARGUMENTS.to_owned());
        };
        let search = k.retriever.search(&k.target, &args.query, args.top_k);
        tokio::pin!(search);
        let res = loop {
            let pinging = !self.p.content_started;
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Ending::Cancelled),
                () = self.tx.closed() => return Err(Ending::Cancelled),
                r = &mut search => break r,
                () = sleep_until(self.p.ping_at), if pinging => {
                    if !self.ping().await {
                        return Err(Ending::Cancelled);
                    }
                }
            }
        };
        match res {
            Ok(chunks) => {
                self.p.completed.file_search += 1;
                self.write_progress(true).await;
                Ok(format_output(&chunks, k.max_chunk_chars))
            }
            Err(e) => {
                tracing::warn!(turn_id = %self.turn.turn_id, error = %e, "knowledge search failed");
                self.p.knowledge_failed += 1;
                Ok(SEARCH_FAILED.to_owned())
            }
        }
    }

    /// Streams one provider request until its terminal event.
    async fn drive_request(&mut self, request: &LlmRequest) -> Ending {
        let provider_cancel = self.cancel.child_token();
        let mut events = match self.open(&provider_cancel, request).await {
            Ok(events) => events,
            Err(ending) => return ending,
        };
        loop {
            let pinging = !self.p.content_started;
            let wake = tokio::select! {
                biased;
                () = self.cancel.cancelled() => Wake::Cancelled,
                () = self.tx.closed() => Wake::Cancelled,
                ev = events.next() => Wake::Event(ev),
                () = sleep_until(self.p.ping_at), if pinging => Wake::Ping,
            };
            match wake {
                Wake::Cancelled => return Ending::Cancelled,
                Wake::Ping => {
                    if !self.ping().await {
                        return Ending::Cancelled;
                    }
                }
                Wake::Event(None) => {
                    return if self.cancel.is_cancelled() {
                        Ending::Cancelled
                    } else {
                        tracing::warn!(turn_id = %self.turn.turn_id, "provider stream ended without a terminal event");
                        Ending::failed(PROVIDER_ERROR, NO_TERMINAL_MESSAGE)
                    };
                }
                Wake::Event(Some(ev)) => {
                    self.p.ping_at = Instant::now() + self.deps.ping_interval;
                    if let Some(ending) = self.handle(ev, &provider_cancel).await {
                        return ending;
                    }
                }
            }
        }
    }

    /// Opens the provider stream, pinging while it is pending.
    async fn open(
        &mut self,
        provider_cancel: &CancellationToken,
        request: &LlmRequest,
    ) -> Result<BoxStream<'static, LlmEvent>, Ending> {
        let (deps, turn, cancel, tx) = (self.deps, self.turn, self.cancel, self.tx);
        let open = deps
            .llm
            .stream(&turn.provider, request.clone(), provider_cancel.clone());
        tokio::pin!(open);
        loop {
            let res = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Ending::Cancelled),
                () = tx.closed() => return Err(Ending::Cancelled),
                res = &mut open => Some(res),
                () = sleep_until(self.p.ping_at) => None,
            };
            match res {
                Some(Ok(events)) => return Ok(events),
                Some(Err(e)) => {
                    return Err(Ending::Failed {
                        code: e.code.to_owned(),
                        message: e.message,
                        usage: None,
                    });
                }
                None => {
                    if !self.ping().await {
                        return Err(Ending::Cancelled);
                    }
                }
            }
        }
    }

    /// Sends a `ping`; `false` when the client is gone.
    async fn ping(&mut self) -> bool {
        self.p.ping_at = Instant::now() + self.deps.ping_interval;
        self.tx.send(StreamEvent::Ping).await.is_ok()
    }

    /// Handles one provider event; `Some` ends the stream.
    async fn handle(
        &mut self,
        ev: LlmEvent,
        provider_cancel: &CancellationToken,
    ) -> Option<Ending> {
        match ev {
            LlmEvent::TextDelta(text) => {
                if text.is_empty() {
                    return None;
                }
                self.p.text.push_str(&text);
                self.content(StreamEvent::Delta {
                    kind: DeltaKind::Text,
                    content: text,
                })
                .await
            }
            LlmEvent::ReasoningDelta(text) => {
                if text.is_empty() {
                    return None;
                }
                self.content(StreamEvent::Delta {
                    kind: DeltaKind::Reasoning,
                    content: text,
                })
                .await
            }
            LlmEvent::ToolStart { name, details } => {
                if let Some(ending) = self.count_start(&name) {
                    provider_cancel.cancel();
                    return Some(ending);
                }
                self.content(StreamEvent::Tool {
                    phase: ToolPhase::Start,
                    name,
                    details,
                })
                .await
            }
            LlmEvent::ToolDone { name, details } => {
                self.count_done(&name);
                let ending = self
                    .content(StreamEvent::Tool {
                        phase: ToolPhase::Done,
                        name,
                        details,
                    })
                    .await;
                if ending.is_none() {
                    self.write_progress(true).await;
                }
                ending
            }
            LlmEvent::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                self.p.function_calls.push(PendingCall {
                    call_id,
                    name,
                    arguments,
                });
                None
            }
            LlmEvent::Citations(raw) => {
                let mapped = raw.into_iter().filter_map(|c| self.turn.map_citation(c));
                self.p.citations.extend(mapped);
                None
            }
            LlmEvent::Completed {
                usage,
                response_id,
                incomplete_reason,
            } => Some(Ending::Completed {
                usage,
                response_id,
                incomplete_reason,
            }),
            LlmEvent::Failed { error, usage } => Some(Ending::Failed {
                code: error.code.to_owned(),
                message: error.message,
                usage,
            }),
        }
    }

    /// Relays a content event (`delta` / `tool`): ends pings, refreshes the
    /// progress timestamp. `Some(Cancelled)` when the client is gone.
    async fn content(&mut self, ev: StreamEvent) -> Option<Ending> {
        self.p.content_started = true;
        if self.tx.send(ev).await.is_err() {
            return Some(Ending::Cancelled);
        }
        self.write_progress(false).await;
        None
    }

    /// Counts a tool start; `Some` when it exceeds the per-message limit.
    fn count_start(&mut self, name: &str) -> Option<Ending> {
        match name {
            WEB_SEARCH => {
                self.p.web_search_starts += 1;
                (self.p.web_search_starts > self.deps.web_search_max_calls).then(|| {
                    Ending::failed(
                        WEB_SEARCH_CALLS_EXCEEDED,
                        "The model exceeded the web search call limit for one message",
                    )
                })
            }
            CODE_INTERPRETER => {
                self.p.code_interpreter_starts += 1;
                (self.p.code_interpreter_starts > self.deps.code_interpreter_max_calls).then(|| {
                    Ending::failed(
                        CODE_INTERPRETER_CALLS_EXCEEDED,
                        "The model exceeded the code interpreter call limit for one message",
                    )
                })
            }
            _ => None,
        }
    }

    fn count_done(&mut self, name: &str) {
        let c = &mut self.p.completed;
        match name {
            WEB_SEARCH => c.web_search += 1,
            CODE_INTERPRETER => c.code_interpreter += 1,
            FILE_SEARCH => c.file_search += 1,
            _ => {}
        }
    }

    /// Writes `last_progress_at` and the completed tool counters when `force`
    /// or when the last write is older than [`PROGRESS_REFRESH`]. Best effort.
    async fn write_progress(&mut self, force: bool) {
        let now = Instant::now();
        if !force && now.duration_since(self.p.last_progress_write) < PROGRESS_REFRESH {
            return;
        }
        self.p.last_progress_write = now;
        let c = self.p.completed;
        let counts = CompletedToolCounts {
            web_search: i32::try_from(c.web_search).unwrap_or(i32::MAX),
            code_interpreter: i32::try_from(c.code_interpreter).unwrap_or(i32::MAX),
            file_search: i32::try_from(c.file_search).unwrap_or(i32::MAX),
        };
        let res = match self.deps.db.conn() {
            Ok(conn) => {
                turn_repo::record_progress(
                    &conn,
                    self.turn.tenant_id,
                    self.turn.turn_id,
                    db_now(),
                    counts,
                )
                .await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            tracing::warn!(turn_id = %self.turn.turn_id, error = %e, "turn progress not recorded");
        }
    }
}

/// Finalizes the turn and sends the terminal event the finalization result
/// allows.
async fn finish(
    deps: &TaskDeps,
    turn: &TurnRun,
    tx: &mpsc::Sender<StreamEvent>,
    ending: Ending,
    p: Progress,
    latency: Duration,
) {
    let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
    let finalizer = Finalizer {
        deps,
        turn,
        tools: p.reported_tools(),
        latency_ms,
    };
    match ending {
        Ending::Cancelled => {
            let outcome = TerminalOutcome::Cancelled {
                partial_text: p.text,
            };
            if let Err(e) = finalizer.run(outcome).await {
                tracing::error!(turn_id = %turn.turn_id, error = %e, "cancelled turn not finalized");
            }
        }
        Ending::Failed {
            code,
            message,
            usage,
        } => finish_failed(&finalizer, tx, code, &message, usage, p.text).await,
        Ending::Completed {
            usage,
            response_id,
            incomplete_reason,
        } => {
            let citations = if incomplete_reason.is_some() {
                Vec::new()
            } else {
                p.citations
            };
            let outcome = TerminalOutcome::Completed {
                text: p.text,
                usage,
                response_id,
                incomplete_reason,
            };
            finish_completed(&finalizer, tx, outcome, usage, citations).await;
        }
    }
}

/// Builds the finalization input of the turn's outcome and finalizes.
struct Finalizer<'a> {
    deps: &'a TaskDeps,
    turn: &'a TurnRun,
    tools: ToolCounts,
    latency_ms: u64,
}

impl Finalizer<'_> {
    async fn run(&self, outcome: TerminalOutcome) -> Result<FinalizeResult, DomainError> {
        self.deps
            .finalization
            .finalize(finalize_input(
                self.turn,
                outcome,
                self.tools,
                self.latency_ms,
            ))
            .await
    }
}

/// Failed stream: `error{code}` after a CAS-winning commit, and also when the
/// finalization itself failed (the client gets the original code); nothing
/// when another finalizer won.
async fn finish_failed(
    f: &Finalizer<'_>,
    tx: &mpsc::Sender<StreamEvent>,
    code: String,
    message: &str,
    usage: Option<UsageTokens>,
    partial_text: String,
) {
    let message = sanitize_provider_message(message);
    let res = f
        .run(TerminalOutcome::Failed {
            error_code: code.clone(),
            error_detail: Some(message.clone()),
            usage,
            partial_text,
        })
        .await;
    match res {
        Ok(FinalizeResult { won: false, .. }) => {
            tracing::info!(turn_id = %f.turn.turn_id, "failed turn already finalized elsewhere");
        }
        Ok(_) => send_terminal(tx, StreamEvent::error(code, message)).await,
        Err(e) => {
            tracing::error!(turn_id = %f.turn.turn_id, error = %e, "failed turn not finalized");
            send_terminal(tx, StreamEvent::error(code, message)).await;
        }
    }
}

/// Completed stream: `citations` (if any) and `done` only after a CAS-winning
/// commit of `completed`; `message_persistence_failed` when finalization
/// downgraded the turn, `finalization_failed` when it failed, nothing when
/// another finalizer won.
async fn finish_completed(
    f: &Finalizer<'_>,
    tx: &mpsc::Sender<StreamEvent>,
    outcome: TerminalOutcome,
    usage: Option<UsageTokens>,
    citations: Vec<Citation>,
) {
    let turn = f.turn;
    let quota_warnings = match f.run(outcome).await {
        Err(e) => {
            tracing::error!(turn_id = %turn.turn_id, error = %e, "completed turn not finalized");
            let ev = StreamEvent::error(FINALIZATION_FAILED, FINALIZATION_FAILED_MESSAGE);
            return send_terminal(tx, ev).await;
        }
        Ok(FinalizeResult { won: false, .. }) => {
            tracing::info!(turn_id = %turn.turn_id, "completed turn already finalized elsewhere");
            return;
        }
        Ok(FinalizeResult {
            state: TurnState::Completed,
            quota_warnings,
            ..
        }) => quota_warnings,
        Ok(_) => {
            let ev = StreamEvent::error(MESSAGE_PERSISTENCE_FAILED, PERSISTENCE_FAILED_MESSAGE);
            return send_terminal(tx, ev).await;
        }
    };
    if !citations.is_empty() && tx.send(StreamEvent::Citations(citations)).await.is_err() {
        return;
    }
    // The provider's usage of the terminal event; zeros only when the provider
    // reported none (`done.usage` is required by the contract).
    let u = usage.unwrap_or_default();
    let done = DoneData {
        usage: UsageCounts {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        },
        effective_model: turn.effective_model.clone(),
        selected_model: turn.selected_model.clone(),
        quota_decision: turn.quota_decision,
        downgrade_from: turn.downgrade_from(),
        downgrade_reason: turn.downgrade_reason.map(str::to_owned),
        quota_warnings,
    };
    send_terminal(tx, StreamEvent::Done(done)).await;
}

/// Sends the terminal event; a client that is already gone is ignored (close
/// rule 3: the committed outcome stands).
async fn send_terminal(tx: &mpsc::Sender<StreamEvent>, ev: StreamEvent) {
    if tx.send(ev).await.is_err() {
        tracing::debug!("client gone before the terminal event");
    }
}

fn finalize_input(
    turn: &TurnRun,
    outcome: TerminalOutcome,
    tool_counts: ToolCounts,
    latency_ms: u64,
) -> FinalizeInput {
    FinalizeInput {
        turn_id: turn.turn_id,
        chat_id: turn.chat_id,
        tenant_id: turn.tenant_id,
        request_id: turn.request_id,
        requester_user_id: turn.user_id,
        selected_model: turn.selected_model.clone(),
        effective_model: turn.effective_model.clone(),
        policy_version: turn.policy_version,
        reserve_tokens: turn.reserve_tokens,
        reserved_credits_micro: turn.reserved_credits_micro,
        max_output_tokens_applied: turn.max_output_tokens_applied,
        minimal_generation_floor_applied: turn.minimal_generation_floor_applied,
        periods: turn.periods,
        premium: turn.premium,
        assistant_message_id: turn.assistant_message_id,
        outcome,
        tool_counts,
        quota_decision: turn.policy_decision(),
        latency_ms,
        summary_candidate: Some(SummaryCandidate {
            trigger: turn.summary_trigger,
        }),
    }
}
