//! Provider task of a committed turn (S§6.2): streams the provider response
//! through a bounded channel to the SSE writer, enforces the per-turn tool
//! limits, maps citations, refreshes `last_progress_at`, finalizes the turn
//! and only then emits the terminal event.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{AuditLatency, ModelCatalogEntry, TerminalState, UsageTokens};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use tracing::{error, warn};
use uuid::Uuid;

use crate::api::rest::dto::{
    Citation, CitationSource, DeltaKind, DoneData, QuotaDecisionKind, StreamStartedData, TextSpan,
    ThreadSummaryInfo, ToolPhase, Usage,
};
use crate::config::MiniChatConfig;
use crate::domain::billing::TurnReserve;
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::estimation::multipliers;
use crate::domain::ports::{KnowledgeChunk, KnowledgeRetriever, LlmPort, SummaryTriggerInput};
use crate::domain::services::finalization::{
    FinalizationService, FinalizeInput, FinalizeOutcome, MESSAGE_PERSISTENCE_FAILED, Terminal,
    ToolCounts,
};
use crate::domain::services::quota_service::{QuotaDecision, QuotaWarning};
use crate::domain::services::stream_service::{LiveTurn, StreamEvent, TurnSetup};
use crate::domain::tools::SEARCH_KNOWLEDGE;
use crate::infra::db::repos::{ToolCounter, TurnRepo};
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmEvent, LlmRequest, RawCitation, Role,
};
use crate::infra::metrics::MiniChatMetrics;

/// `last_progress_at` is refreshed at most this often (D "Orphan Turn
/// Watchdog").
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
/// `function_call_output` of a `search_knowledge` call beyond
/// `knowledge_search.max_calls_per_message`.
const SEARCH_LIMIT_REACHED: &str =
    "Search limit reached for this message. Answer with the information already retrieved.";
/// `function_call_output` of a failed retrieval.
const SEARCH_FAILED: &str = "The knowledge search failed. Answer without it.";
/// `function_call_output` of a call without a usable `query`.
const SEARCH_INVALID: &str = "Invalid search_knowledge call: a non-empty query is required.";
const FINALIZATION_FAILED: &str = "finalization_failed";
const PROVIDER_ERROR: &str = "provider_error";

/// Turn state kept by the provider task while streaming.
#[derive(Debug)]
struct Progress {
    /// Start of the provider task (TTFT / total latency base).
    started: Instant,
    text: String,
    citations: Vec<Citation>,
    web_search_starts: u32,
    code_interpreter_starts: u32,
    completed: ToolCounts,
    ttft_ms: Option<u64>,
    incomplete: bool,
    /// Last `last_progress_at` write (or the start).
    last_touch: Instant,
    /// When the client disconnect was observed.
    disconnected: Option<Instant>,
    /// Arrival of the first provider event (`ttft_overhead_ms` base).
    first_event: Option<Instant>,
    /// `ttft_overhead_ms` was recorded (first content send done).
    overhead_recorded: bool,
    /// Retrievals started by `search_knowledge` calls in this turn.
    knowledge_calls: u32,
}

/// A function call of the model handled by the knowledge-search loop.
#[derive(Debug, Clone)]
struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Outcome of one provider request of a turn.
enum Iteration {
    /// The turn is over.
    Done(Terminal),
    /// The response ended with `search_knowledge` calls: retrieve and send
    /// the next request.
    ToolUse(Vec<FunctionCall>),
}

/// What the relay does with one provider event.
enum Step {
    Emit(StreamEvent),
    Skip,
    End(Terminal),
    /// A `search_knowledge` call (knowledge search on for the turn).
    Call(FunctionCall),
}

/// Runs the provider task of committed turns.
#[derive(Clone)]
pub struct TurnRunner {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    llm: Arc<dyn LlmPort>,
    finalization: Arc<FinalizationService>,
    config: Arc<MiniChatConfig>,
    metrics: Arc<MiniChatMetrics>,
    knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

impl TurnRunner {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        clock: Arc<dyn Clock>,
        llm: Arc<dyn LlmPort>,
        finalization: Arc<FinalizationService>,
        config: Arc<MiniChatConfig>,
        metrics: Arc<MiniChatMetrics>,
    ) -> Self {
        Self {
            db,
            clock,
            llm,
            finalization,
            config,
            metrics,
            knowledge: None,
        }
    }

    /// Use `knowledge` for the `search_knowledge` calls of turns that offer
    /// the tool.
    #[must_use]
    pub fn with_knowledge(mut self, knowledge: Option<Arc<dyn KnowledgeRetriever>>) -> Self {
        self.knowledge = knowledge;
        self
    }

    /// Spawn the provider task of `setup`; the returned turn carries the
    /// `stream_started` payload and the event channel.
    pub(crate) fn run(&self, setup: TurnSetup) -> LiveTurn {
        // The turn's reserve is committed by now (send and retry / edit).
        self.metrics.quota_reserved();
        let images = setup
            .llm_request
            .input
            .iter()
            .flat_map(|m| &m.content)
            .filter(|c| matches!(c, ContentPart::Image { .. }))
            .count();
        self.metrics
            .image_inputs(u32::try_from(images).unwrap_or(u32::MAX));
        let capacity = usize::from(self.config.streaming.sse_channel_capacity).max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let cancel = CancellationToken::new();
        let started = StreamStartedData {
            request_id: setup.request_id,
            message_id: setup.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: setup.plan.summary_applied.map(|t| ThreadSummaryInfo {
                token_estimate: u32::try_from(t).unwrap_or(0),
            }),
        };
        let runner = self.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move { runner.relay(setup, tx, task_cancel).await });
        LiveTurn {
            started,
            events: rx,
            cancel,
        }
    }

    /// Stream, finalize, then emit the terminal event (only after commit).
    async fn relay(self, s: TurnSetup, tx: mpsc::Sender<StreamEvent>, cancel: CancellationToken) {
        let provider_cancel = cancel.child_token();
        let now = Instant::now();
        let mut p = Progress {
            started: now,
            text: String::new(),
            citations: Vec::new(),
            web_search_starts: 0,
            code_interpreter_starts: 0,
            completed: ToolCounts::default(),
            ttft_ms: None,
            incomplete: false,
            last_touch: now,
            disconnected: None,
            first_event: None,
            overhead_recorded: false,
            knowledge_calls: 0,
        };
        let (provider, model) = (
            s.target.provider_id.clone(),
            s.decision.effective.id.clone(),
        );
        self.metrics.stream_started(&provider, &model);
        let terminal = self.drive(&s, &tx, &cancel, &provider_cancel, &mut p).await;
        // Release the provider connection before the finalization.
        provider_cancel.cancel();
        self.metrics
            .stream_ended(&provider, &model, p.started.elapsed(), p.ttft_ms);
        let abort = p.disconnected.map(|t| t.elapsed());
        if matches!(terminal, Terminal::Cancelled { .. }) {
            if cancel.is_cancelled() {
                self.metrics.cancel_requested();
            }
            self.metrics.stream_disconnected(p.ttft_ms.is_none());
        }
        // A finalization input that cannot be built (credit multipliers
        // out of range) fails the finalization: the turn is not finalized
        // on this path (D§5.3 "Error handling").
        let outcome = match finalize_input(&s, terminal.clone(), &p) {
            Ok(input) => self.finalization.finalize(input).await,
            Err(e) => Err(e),
        };
        if let Ok(FinalizeOutcome::Won {
            committed_state, ..
        }) = &outcome
        {
            self.record_outcome(&provider, &model, &terminal, *committed_state, abort);
        }
        let done = |usage, warnings| completed_events(&s, usage, warnings, p);
        for ev in terminal_events(s.turn_id, &terminal, outcome, done) {
            if tx.send(ev).await.is_err() {
                break;
            }
        }
    }

    /// `ttft_overhead_ms` after the first event sent to the SSE channel.
    fn record_overhead(&self, s: &TurnSetup, p: &mut Progress) {
        if p.overhead_recorded {
            return;
        }
        if let Some(first) = p.first_event {
            p.overhead_recorded = true;
            self.metrics.ttft_overhead(
                &s.target.provider_id,
                &s.decision.effective.id,
                first.elapsed(),
            );
        }
    }

    /// Outcome metrics of a committed finalization.
    fn record_outcome(
        &self,
        provider: &str,
        model: &str,
        terminal: &Terminal,
        committed: TerminalState,
        abort: Option<Duration>,
    ) {
        match (committed, terminal) {
            (
                TerminalState::Completed,
                Terminal::Completed {
                    incomplete_reason, ..
                },
            ) => self
                .metrics
                .stream_completed(provider, model, incomplete_reason.as_deref()),
            (TerminalState::Failed, Terminal::Failed { code, .. }) => {
                self.metrics.stream_failed(provider, model, code);
            }
            (TerminalState::Failed, _) => {
                self.metrics
                    .stream_failed(provider, model, MESSAGE_PERSISTENCE_FAILED);
            }
            (TerminalState::Cancelled, _) => {
                self.metrics.cancel_effective(abort.unwrap_or_default());
                self.metrics.stream_aborted("client_disconnect");
            }
            _ => {}
        }
    }

    /// Run the provider requests of the turn until a terminal outcome: one
    /// request, or the knowledge-search loop (D§4 "Knowledge Search").
    async fn drive(
        &self,
        s: &TurnSetup,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
        provider_cancel: &CancellationToken,
        p: &mut Progress,
    ) -> Terminal {
        let retriever = self.knowledge_of(s);
        let max_iterations = self
            .config
            .knowledge_search
            .max_calls_per_message
            .saturating_add(2);
        let scope = AccessScope::for_tenant(s.chat.tenant_id);
        let mut request = s.llm_request.clone();
        let mut iteration = 0_u32;
        loop {
            iteration += 1;
            if retriever.is_some() && iteration > max_iterations {
                warn!(turn_id = %s.turn_id, max_iterations, "knowledge-search loop exceeded its iteration cap");
                return failed(
                    AGENTIC_ITERATIONS_EXCEEDED,
                    "Too many knowledge search iterations in one message",
                );
            }
            let outcome = self
                .drive_request(s, &scope, &request, tx, cancel, provider_cancel, p)
                .await;
            match (outcome, retriever) {
                (Iteration::Done(terminal), _) => return terminal,
                (Iteration::ToolUse(calls), Some(retriever)) => {
                    self.run_knowledge_calls(s, &scope, retriever.as_ref(), calls, &mut request, p)
                        .await;
                }
                (Iteration::ToolUse(_), None) => {
                    return failed(
                        UNEXPECTED_TOOL_USE,
                        "The model requested an unsupported tool",
                    );
                }
            }
        }
    }

    /// The knowledge retriever when the turn offers `search_knowledge`.
    fn knowledge_of(&self, s: &TurnSetup) -> Option<&Arc<dyn KnowledgeRetriever>> {
        self.knowledge.as_ref().filter(|_| s.tool_flags.knowledge)
    }

    /// Consume one provider stream until a terminal outcome, or until the
    /// response ends with `search_knowledge` calls.
    #[allow(clippy::too_many_arguments)]
    async fn drive_request(
        &self,
        s: &TurnSetup,
        scope: &AccessScope,
        request: &LlmRequest,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
        provider_cancel: &CancellationToken,
        p: &mut Progress,
    ) -> Iteration {
        let opened = tokio::select! {
            biased;
            () = cancel.cancelled() => return Iteration::Done(cancelled(p)),
            () = tx.closed() => return Iteration::Done(cancelled(p)),
            r = self.llm.stream(&s.target, request.clone(), provider_cancel.clone()) => r,
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(f) => {
                return Iteration::Done(Terminal::Failed {
                    code: f.code.as_str().to_owned(),
                    detail: f.message,
                    usage: f.usage,
                });
            }
        };
        let mut calls = Vec::new();
        loop {
            let ev = tokio::select! {
                biased;
                () = cancel.cancelled() => return Iteration::Done(cancelled(p)),
                () = tx.closed() => return Iteration::Done(cancelled(p)),
                ev = stream.next() => ev,
            };
            let Some(ev) = ev else {
                return Iteration::Done(stream_ended(cancel, p));
            };
            p.first_event.get_or_insert_with(Instant::now);
            match self.on_event(s, scope, ev, p).await {
                Step::Skip => continue,
                Step::Call(call) => {
                    calls.push(call);
                    continue;
                }
                Step::End(Terminal::Completed { text, .. }) if !calls.is_empty() => {
                    // A tool-use outcome: the text so far stays part of the answer.
                    p.text = text;
                    p.incomplete = false;
                    return Iteration::ToolUse(calls);
                }
                Step::End(terminal) => return Iteration::Done(terminal),
                Step::Emit(out) => {
                    if !send(tx, cancel, out).await {
                        return Iteration::Done(cancelled(p));
                    }
                    self.record_overhead(s, p);
                }
            }
            self.refresh_progress(s, scope, p).await;
        }
    }

    /// Answer the `search_knowledge` calls of one response and append the
    /// calls and their outputs to `request`. Up to
    /// `knowledge_search.max_calls_per_message` retrievals run per turn;
    /// later calls get [`SEARCH_LIMIT_REACHED`].
    async fn run_knowledge_calls(
        &self,
        s: &TurnSetup,
        scope: &AccessScope,
        retriever: &dyn KnowledgeRetriever,
        calls: Vec<FunctionCall>,
        request: &mut LlmRequest,
        p: &mut Progress,
    ) {
        let cfg = &self.config.knowledge_search;
        let mut outputs = Vec::with_capacity(calls.len());
        for call in &calls {
            let output = if p.knowledge_calls >= cfg.max_calls_per_message {
                SEARCH_LIMIT_REACHED.to_owned()
            } else {
                p.knowledge_calls += 1;
                p.completed.file_search += 1;
                self.retrieve(s, scope, retriever, &call.arguments).await
            };
            outputs.push(ContentPart::FunctionOutput {
                call_id: call.call_id.clone(),
                output,
            });
        }
        request.input.push(InputMessage {
            role: Role::Assistant,
            content: calls
                .into_iter()
                .map(|c| ContentPart::FunctionCall {
                    call_id: c.call_id,
                    name: c.name,
                    arguments: c.arguments,
                })
                .collect(),
        });
        request.input.push(InputMessage {
            role: Role::User,
            content: outputs,
        });
    }

    /// One retrieval: the `function_call_output` text of a call. A
    /// successful retrieval increments `file_search_completed_count`.
    async fn retrieve(
        &self,
        s: &TurnSetup,
        scope: &AccessScope,
        retriever: &dyn KnowledgeRetriever,
        arguments: &str,
    ) -> String {
        let cfg = &self.config.knowledge_search;
        let Some((query, top_k)) = search_arguments(arguments) else {
            warn!(turn_id = %s.turn_id, "search_knowledge call without a query");
            return SEARCH_INVALID.to_owned();
        };
        let top_k = capped_top_k(top_k, cfg.top_k);
        let started = Instant::now();
        let result = retriever.search(&query, top_k).await;
        let chunks = result.as_ref().ok().map(Vec::len);
        self.metrics.knowledge_search(
            if chunks.is_some() { "ok" } else { "error" },
            started.elapsed(),
            chunks,
        );
        match result {
            Ok(chunks) => {
                self.count_retrieval(s, scope).await;
                search_output(&chunks, cfg.max_chunk_chars)
            }
            Err(e) => {
                warn!(turn_id = %s.turn_id, error = %e, "knowledge search failed");
                SEARCH_FAILED.to_owned()
            }
        }
    }

    /// A successful retrieval counts in `file_search_completed_count`.
    async fn count_retrieval(&self, s: &TurnSetup, scope: &AccessScope) {
        if let Err(e) = self
            .increment(scope, s.turn_id, ToolCounter::FileSearch, self.clock.now())
            .await
        {
            warn!(turn_id = %s.turn_id, error = %e, "tool counter update failed");
        }
    }

    /// Refresh `last_progress_at` when the last write is older than
    /// [`PROGRESS_INTERVAL`].
    async fn refresh_progress(&self, s: &TurnSetup, scope: &AccessScope, p: &mut Progress) {
        if p.last_touch.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        p.last_touch = Instant::now();
        if let Err(e) = self.touch(scope, s.turn_id, self.clock.now()).await {
            warn!(turn_id = %s.turn_id, error = %e, "last_progress_at refresh failed");
        }
    }

    /// Translate one provider event (counting tools, collecting citations).
    async fn on_event(
        &self,
        s: &TurnSetup,
        scope: &AccessScope,
        ev: LlmEvent,
        p: &mut Progress,
    ) -> Step {
        match ev {
            LlmEvent::TextDelta(t) => {
                let started = p.started;
                p.ttft_ms.get_or_insert_with(|| millis(started.elapsed()));
                p.text.push_str(&t);
                Step::Emit(StreamEvent::Delta {
                    kind: DeltaKind::Text,
                    content: t,
                })
            }
            LlmEvent::ReasoningDelta(t) => Step::Emit(StreamEvent::Delta {
                kind: DeltaKind::Reasoning,
                content: t,
            }),
            LlmEvent::ToolStart { name, details } => match self.count_start(&name, p) {
                Some(code) => Step::End(failed(code, limit_message(code))),
                None => Step::Emit(StreamEvent::Tool {
                    phase: ToolPhase::Start,
                    name,
                    details,
                }),
            },
            LlmEvent::ToolDone { name, details } => {
                self.on_tool_done(s, scope, &name, p).await;
                Step::Emit(StreamEvent::Tool {
                    phase: ToolPhase::Done,
                    name,
                    details,
                })
            }
            LlmEvent::Citation(raw) => {
                on_citation(s, raw, p);
                Step::Skip
            }
            LlmEvent::Completed(t) => Step::End(Terminal::Completed {
                text: std::mem::take(&mut p.text),
                usage: t.usage,
                response_id: t.response_id,
                incomplete_reason: None,
            }),
            LlmEvent::Incomplete { terminal, reason } => {
                p.incomplete = true;
                Step::End(Terminal::Completed {
                    text: std::mem::take(&mut p.text),
                    usage: terminal.usage,
                    response_id: terminal.response_id,
                    incomplete_reason: Some(reason),
                })
            }
            LlmEvent::Failed(f) => Step::End(Terminal::Failed {
                code: f.code.as_str().to_owned(),
                detail: f.message,
                usage: f.usage,
            }),
            LlmEvent::FunctionCall {
                call_id,
                name,
                arguments,
            } if name == SEARCH_KNOWLEDGE && self.knowledge_of(s).is_some() => {
                Step::Call(FunctionCall {
                    call_id,
                    name,
                    arguments,
                })
            }
            LlmEvent::FunctionCall { name, .. } => {
                warn!(turn_id = %s.turn_id, tool = %name, "unexpected function tool call");
                Step::End(failed(
                    UNEXPECTED_TOOL_USE,
                    "The model requested an unsupported tool",
                ))
            }
        }
    }

    /// Count a completed built-in tool call and persist the counter.
    async fn on_tool_done(&self, s: &TurnSetup, scope: &AccessScope, name: &str, p: &mut Progress) {
        let Some(counter) = count_done(name, p) else {
            return;
        };
        p.last_touch = Instant::now();
        if let Err(e) = self
            .increment(scope, s.turn_id, counter, self.clock.now())
            .await
        {
            warn!(turn_id = %s.turn_id, error = %e, "tool counter update failed");
        }
    }

    /// Count a tool `start`; the error code when the per-turn limit is exceeded.
    fn count_start(&self, name: &str, p: &mut Progress) -> Option<&'static str> {
        let q = &self.config.quota;
        match name {
            "web_search" => {
                p.web_search_starts += 1;
                (p.web_search_starts > q.web_search_max_calls_per_message)
                    .then_some(WEB_SEARCH_CALLS_EXCEEDED)
            }
            "code_interpreter" => {
                p.code_interpreter_starts += 1;
                (p.code_interpreter_starts > q.code_interpreter_max_calls_per_message)
                    .then_some(CODE_INTERPRETER_CALLS_EXCEEDED)
            }
            _ => None,
        }
    }

    async fn increment(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        counter: ToolCounter,
        now: time::OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        TurnRepo
            .increment_tool_count(&conn, scope, turn_id, counter, now)
            .await?;
        Ok(())
    }

    async fn touch(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        now: time::OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        TurnRepo.touch_progress(&conn, scope, turn_id, now).await?;
        Ok(())
    }
}

/// Send one event unless the turn is cancelled; `false` = client gone.
async fn send(tx: &mpsc::Sender<StreamEvent>, cancel: &CancellationToken, ev: StreamEvent) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        r = tx.send(ev) => r.is_ok(),
    }
}

fn cancelled(p: &mut Progress) -> Terminal {
    p.disconnected.get_or_insert_with(Instant::now);
    Terminal::Cancelled {
        partial_text: std::mem::take(&mut p.text),
    }
}

fn failed(code: &str, detail: &str) -> Terminal {
    Terminal::Failed {
        code: code.to_owned(),
        detail: detail.to_owned(),
        usage: None,
    }
}

fn limit_message(code: &str) -> &'static str {
    if code == WEB_SEARCH_CALLS_EXCEEDED {
        "Too many web search calls in one message"
    } else {
        "Too many code interpreter calls in one message"
    }
}

fn error_event(code: &str, message: &str) -> StreamEvent {
    StreamEvent::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Client citation of a provider annotation: file citations resolve through
/// the chat's `provider_file_id → (attachment_id, filename)` map (unknown or
/// deleted files are omitted); web citations keep url / title / snippet and
/// the span when both offsets are known.
fn map_citation(raw: RawCitation, files: &HashMap<String, (Uuid, String)>) -> Option<Citation> {
    match raw {
        RawCitation::File {
            provider_file_id, ..
        } => {
            let (attachment_id, filename) = files.get(&provider_file_id)?;
            Some(Citation {
                source: CitationSource::File,
                title: filename.clone(),
                url: None,
                attachment_id: Some(*attachment_id),
                snippet: String::new(),
                score: None,
                span: None,
            })
        }
        RawCitation::Url {
            url,
            title,
            start,
            end,
            snippet,
        } => Some(Citation {
            source: CitationSource::Web,
            title,
            url: Some(url),
            attachment_id: None,
            snippet,
            score: None,
            span: match (start, end) {
                (Some(start), Some(end)) => Some(TextSpan {
                    start: start as u64,
                    end: end as u64,
                }),
                _ => None,
            },
        }),
    }
}

/// `done` payload of a CAS-winning completed turn.
fn done_data(
    s: &TurnSetup,
    usage: Usage,
    warnings: Vec<crate::api::rest::dto::QuotaWarning>,
) -> DoneData {
    let d = &s.decision;
    let downgraded = d.decision == QuotaDecision::Downgrade;
    DoneData {
        usage,
        effective_model: d.effective.id.clone(),
        selected_model: s.selected_model.clone(),
        quota_decision: if downgraded {
            QuotaDecisionKind::Downgrade
        } else {
            QuotaDecisionKind::Allow
        },
        downgrade_from: downgraded.then(|| s.selected_model.clone()),
        downgrade_reason: if downgraded {
            d.downgrade_reason.map(str::to_owned)
        } else {
            None
        },
        quota_warnings: Some(warnings),
    }
}
/// Count a tool `done`; the persisted counter of a built-in tool.
fn count_done(name: &str, p: &mut Progress) -> Option<ToolCounter> {
    let c = &mut p.completed;
    match name {
        "web_search" => {
            c.web_search += 1;
            Some(ToolCounter::WebSearch)
        }
        "code_interpreter" => {
            c.code_interpreter += 1;
            Some(ToolCounter::CodeInterpreter)
        }
        "file_search" => {
            c.file_search += 1;
            Some(ToolCounter::FileSearch)
        }
        _ => None,
    }
}

/// Terminal SSE events, gated on the finalization outcome (D§5.7 "Terminal
/// SSE Event Emission Guard"). A CAS loser sends nothing (the SSE writer then
/// reports `stream_interrupted`); a cancelled turn sends nothing.
/// `done` builds the events of a committed completed turn.
fn terminal_events(
    turn_id: Uuid,
    terminal: &Terminal,
    outcome: Result<FinalizeOutcome, DomainError>,
    done: impl FnOnce(UsageTokens, Vec<QuotaWarning>) -> Vec<StreamEvent>,
) -> Vec<StreamEvent> {
    match (terminal, outcome) {
        (Terminal::Cancelled { .. }, _) | (_, Ok(FinalizeOutcome::Lost)) => Vec::new(),
        (
            Terminal::Completed { usage, .. },
            Ok(FinalizeOutcome::Won {
                committed_state: TerminalState::Completed,
                quota_warnings,
            }),
        ) => done(usage.unwrap_or_default(), quota_warnings),
        (Terminal::Completed { .. }, Ok(FinalizeOutcome::Won { .. })) => vec![error_event(
            MESSAGE_PERSISTENCE_FAILED,
            "The response could not be saved",
        )],
        (Terminal::Completed { .. }, Err(e)) => {
            error!(turn_id = %turn_id, error = %e, "finalization failed");
            vec![error_event(
                FINALIZATION_FAILED,
                "The response could not be finalized",
            )]
        }
        (Terminal::Failed { code, detail, .. }, outcome) => {
            if let Err(e) = outcome {
                error!(turn_id = %turn_id, error = %e, "finalization of a failed turn failed");
            }
            vec![error_event(code, detail)]
        }
    }
}

/// `citations` (at most one, not after an incomplete response) then `done`.
fn completed_events(
    s: &TurnSetup,
    usage: UsageTokens,
    warnings: Vec<QuotaWarning>,
    p: Progress,
) -> Vec<StreamEvent> {
    let mut out = Vec::with_capacity(2);
    if !p.incomplete && !p.citations.is_empty() {
        out.push(StreamEvent::Citations(p.citations));
    }
    out.push(StreamEvent::Done(Box::new(done_data(
        s,
        Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        },
        warnings.into_iter().map(Into::into).collect(),
    ))));
    out
}

/// Credit multipliers of the effective model; out of range is an internal
/// error that fails the finalization (D§5.3 "Error handling").
fn effective_multipliers(m: &ModelCatalogEntry) -> Result<(i64, i64), DomainError> {
    multipliers(m).map_err(|e| {
        warn!(model = %m.id, error = %e, "credit multipliers out of range; turn not finalized");
        DomainError::Internal(format!("credit multipliers of model {}: {e}", m.id))
    })
}

/// Finalization input of the turn from its setup and the stream outcome.
///
/// # Errors
/// The effective model's credit multipliers are out of range.
fn finalize_input(
    s: &TurnSetup,
    terminal: Terminal,
    p: &Progress,
) -> Result<FinalizeInput, DomainError> {
    let d = &s.decision;
    let mults = effective_multipliers(&d.effective)?;
    Ok(FinalizeInput {
        turn_id: s.turn_id,
        tenant: s.chat.tenant_id,
        user: s.ctx.subject_id(),
        chat_id: s.chat.id,
        request_id: s.request_id,
        terminal,
        assistant_message_id: s.assistant_message_id,
        effective_model: d.effective.id.clone(),
        selected_model: s.selected_model.clone(),
        effective_tier: d.effective_tier,
        multipliers: mults,
        periods: d.periods,
        reserve: TurnReserve {
            reserve_tokens: d.plan.reserve_tokens,
            max_output_tokens_applied: d.plan.max_output_tokens_applied,
            reserved_credits_micro: d.plan.reserved_credits_micro,
            minimal_generation_floor_applied: d.plan.minimal_generation_floor_applied,
        },
        policy_version: d.policy_version,
        decision: d.decision,
        downgrade_reason: d.downgrade_reason,
        tool_counts: p.completed,
        summary_trigger: SummaryTriggerInput {
            messages_truncated: s.plan.messages_truncated,
            assembled_context_tokens: s.plan.assembled_context_tokens,
            effective_budget: s.plan.effective_budget,
            summary_applied: s.plan.summary_applied.is_some(),
        },
        latency: AuditLatency {
            ttft_ms: p.ttft_ms,
            total_ms: millis(p.started.elapsed()),
        },
        limits: s.limits,
        started_at: s.started_at,
    })
}

/// `query` (non-empty) and optional `top_k` of a `search_knowledge` call.
fn search_arguments(arguments: &str) -> Option<(String, Option<u32>)> {
    let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let query = v.get("query")?.as_str()?.trim();
    if query.is_empty() {
        return None;
    }
    let top_k = v
        .get("top_k")
        .and_then(serde_json::Value::as_u64)
        .map(|k| u32::try_from(k).unwrap_or(u32::MAX));
    Some((query.to_owned(), top_k))
}

/// The model's `top_k` capped at `knowledge_search.top_k` (that cap when
/// the model gave none).
fn capped_top_k(requested: Option<u32>, cfg_top_k: usize) -> u32 {
    let cap = u32::try_from(cfg_top_k).unwrap_or(u32::MAX).max(1);
    requested.map_or(cap, |k| k.clamp(1, cap))
}

/// `function_call_output` of a retrieval: `{"results": [{filename, score,
/// text}]}` with each text cut at `max_chars` characters.
fn search_output(chunks: &[KnowledgeChunk], max_chars: usize) -> String {
    let results: Vec<serde_json::Value> = chunks
        .iter()
        .map(|c| {
            serde_json::json!({
                "filename": c.filename,
                "score": c.score,
                "text": c.text.chars().take(max_chars).collect::<String>(),
            })
        })
        .collect();
    serde_json::json!({ "results": results }).to_string()
}

/// File citations resolve only when `file_search` was sent.
fn on_citation(s: &TurnSetup, raw: RawCitation, p: &mut Progress) {
    let empty = HashMap::new();
    let files = if s.tool_flags.file_search {
        &s.citation_map
    } else {
        &empty
    };
    if let Some(c) = map_citation(raw, files) {
        p.citations.push(c);
    }
}

/// The provider stream ended without a terminal event.
fn stream_ended(cancel: &CancellationToken, p: &mut Progress) -> Terminal {
    if cancel.is_cancelled() {
        cancelled(p)
    } else {
        failed(PROVIDER_ERROR, "Provider stream ended unexpectedly")
    }
}

#[cfg(test)]
#[path = "turn_runner_tests.rs"]
mod tests;
