//! Provider task of a live turn (spec §8.2; DESIGN §5.7 "Terminal SSE Event
//! Emission Guard").
//!
//! Streams the provider response, forwards every translated event at once
//! through the bounded channel, accumulates the assistant text, counts tool
//! calls, enforces the per-turn tool call limits, refreshes the turn's progress
//! and finally runs the CAS-guarded finalization.
//!
//! Knowledge search (DESIGN §4 "Knowledge Search") makes this an agentic loop:
//! an iteration that ends with `search_knowledge` calls runs the retrievals
//! (at most `max_calls_per_message` per turn; further calls get a "search limit
//! reached" output), appends the calls and outputs to the request and issues
//! the next provider request, for at most `max_calls_per_message + 2`
//! iterations (`agentic_iterations_exceeded`). A call of any other function, or
//! any call while knowledge search is off, ends the turn with
//! `unexpected_tool_use`. Only the final iteration's usage is settled. The terminal `done` / `error`
//! is sent only after `finalize` returned; a client disconnect (cancelled token,
//! closed channel, failed send) finalizes the turn `cancelled` and sends nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use tracing::{debug, info, warn};

use super::PreparedTurn;
use super::citations::map_citation;
use crate::api::rest::dto::{
    Citation, CitationsData, DeltaData, DeltaKind, DoneData, QuotaWarning, ToolData, ToolPhase,
    Usage,
};
use crate::api::rest::sse::SseEvent;
use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::model::{QuotaDecision, error_codes};
use crate::domain::services::finalization::{
    FinalizationService, FinalizeInput, FinalizeResult, TerminalKind,
};
use crate::domain::services::quota_status::TierStatus;
use crate::domain::services::thread_summary::{ThreadSummaryService, should_trigger};
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repos::TurnRepo;
use crate::infra::db::repos::turn::TurnCounters;
use crate::infra::llm::{
    FunctionCall, KnowledgeTurn, LlmClient, LlmError, LlmEvent, LlmUsage, ToolResult, ToolRound,
};
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// Minimum interval between two progress touches of a running turn.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";
const FILE_SEARCH: &str = "file_search";

const SEARCH_KNOWLEDGE: &str = "search_knowledge";

/// Function output of a `search_knowledge` call beyond `max_calls_per_message`.
const SEARCH_LIMIT_OUTPUT: &str = "Knowledge search limit reached for this message. Answer with the information already retrieved.";
/// Function output of a failed retrieval.
const SEARCH_FAILED_OUTPUT: &str =
    "Knowledge search failed. Answer with the information already available.";
/// Function output of a call without a usable `query`.
const SEARCH_INVALID_OUTPUT: &str =
    "Knowledge search failed: the arguments must contain a non-empty \"query\" string.";

const NO_TERMINAL_DETAIL: &str = "provider stream ended without a terminal event";
const PERSISTENCE_FAILED_MESSAGE: &str = "The response could not be saved";
const FINALIZATION_FAILED_MESSAGE: &str = "The response could not be finalized";

/// Infrastructure of the provider task.
pub(super) struct TaskDeps {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub llm: Arc<LlmClient>,
    pub finalization: Arc<FinalizationService>,
}

/// How the provider stream ended.
#[derive(Debug)]
enum Outcome {
    Completed {
        usage: Option<LlmUsage>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: String,
        /// Client-visible (sanitized) message.
        message: String,
        /// Internal detail (stored sanitized in `error_detail`).
        detail: String,
        usage: Option<LlmUsage>,
    },
    /// Client disconnect.
    Cancelled,
}

impl Outcome {
    fn from_error(error: &LlmError, usage: Option<LlmUsage>) -> Self {
        Self::Failed {
            code: error.sse_code().to_owned(),
            message: error.client_message(),
            detail: error.to_string(),
            usage,
        }
    }

    fn limit_exceeded(code: &str, tool: &str, limit: u32) -> Self {
        let text = format!("The model exceeded the limit of {limit} {tool} calls per message");
        Self::failed(code, text, None)
    }

    fn failed(code: &str, text: String, usage: Option<LlmUsage>) -> Self {
        Self::Failed {
            code: code.to_owned(),
            message: text.clone(),
            detail: text,
            usage,
        }
    }
}

/// `query` and the capped `top_k` of `search_knowledge` arguments.
fn search_args(arguments: &str, cap: usize) -> Option<(String, usize)> {
    let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let query = v.get("query")?.as_str()?.trim();
    if query.is_empty() {
        return None;
    }
    let top_k = v
        .get("top_k")
        .and_then(serde_json::Value::as_u64)
        .and_then(|k| usize::try_from(k).ok())
        .map_or(cap, |k| k.clamp(1, cap.max(1)));
    Some((query.to_owned(), top_k))
}

struct TaskRun {
    deps: TaskDeps,
    t: PreparedTurn,
    tx: mpsc::Sender<SseEvent>,
    cancel: CancellationToken,
    text: String,
    counters: TurnCounters,
    tool_starts: HashMap<String, u32>,
    citations: Vec<Citation>,
    last_touch: Instant,
    /// Function calls of the current provider iteration.
    calls: Vec<FunctionCall>,
    /// `search_knowledge` retrievals run in this turn.
    retrievals: u32,
    /// Usage/audit `file_search_calls`: provider `file_search` done events plus
    /// `search_knowledge` calls (counted before the retrieval runs).
    file_search_calls: u32,
    /// The next text of a later agentic iteration starts with a blank line
    /// (iteration texts are joined by `\n\n`).
    separate_next_text: bool,
}

/// Run the provider task of `t` to its end (finalization included).
pub(super) async fn run(
    deps: TaskDeps,
    t: PreparedTurn,
    tx: mpsc::Sender<SseEvent>,
    cancel: CancellationToken,
) {
    let mut task = TaskRun {
        deps,
        t,
        tx,
        cancel,
        text: String::new(),
        counters: TurnCounters::default(),
        tool_starts: HashMap::new(),
        citations: Vec::new(),
        last_touch: Instant::now(),
        calls: Vec::new(),
        retrievals: 0,
        file_search_calls: 0,
        separate_next_text: false,
    };
    let outcome = task.drive().await;
    task.finish(outcome).await;
}

/// `mini_chat_thread_summary_trigger_total{result="scheduled"}` (logged after
/// the finalization commit).
fn log_summary_scheduled(turn: &chat_turn::Model) {
    info!(turn_id = %turn.id, chat_id = %turn.chat_id, result = "scheduled", "thread summary trigger");
}

impl TaskRun {
    /// `false` when the client is gone.
    async fn emit(&self, ev: SseEvent) -> bool {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => false,
            sent = self.tx.send(ev) => sent.is_ok(),
        }
    }

    /// Run the provider iterations (one without knowledge search) until a
    /// final outcome.
    async fn drive(&mut self) -> Outcome {
        let mut iteration: u32 = 0;
        loop {
            iteration += 1;
            let outcome = self.drive_iteration().await;
            let calls = std::mem::take(&mut self.calls);
            let Outcome::Completed { usage, .. } = &outcome else {
                return outcome;
            };
            if calls.is_empty() {
                return outcome;
            }
            let usage = *usage;
            let knowledge = match &self.t.knowledge {
                Some(k) if calls.iter().all(|c| c.name == SEARCH_KNOWLEDGE) => k.clone(),
                _ => {
                    warn!(turn_id = %self.t.turn.id, calls = calls.len(), "model requested an unexpected function tool");
                    return Outcome::failed(
                        error_codes::UNEXPECTED_TOOL_USE,
                        "The model requested a tool that is not available".to_owned(),
                        usage,
                    );
                }
            };
            let max_iterations = knowledge.max_calls.saturating_add(2);
            if iteration >= max_iterations {
                return Outcome::failed(
                    error_codes::AGENTIC_ITERATIONS_EXCEEDED,
                    format!("The knowledge search loop exceeded {max_iterations} iterations"),
                    usage,
                );
            }
            let Some(round) = self.run_retrievals(&knowledge, calls).await else {
                return Outcome::Cancelled;
            };
            self.t.request.tool_rounds.push(round);
            self.separate_next_text = !self.text.is_empty();
        }
    }

    /// Run the `search_knowledge` calls of one iteration; `None` on a disconnect.
    async fn run_retrievals(
        &mut self,
        k: &KnowledgeTurn,
        calls: Vec<FunctionCall>,
    ) -> Option<ToolRound> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let output = if self.retrievals >= k.max_calls {
                info!(turn_id = %self.t.turn.id, result = "limit_reached", "knowledge search");
                SEARCH_LIMIT_OUTPUT.to_owned()
            } else {
                self.retrievals += 1;
                self.file_search_calls += 1;
                match search_args(&call.arguments, k.top_k) {
                    None => SEARCH_INVALID_OUTPUT.to_owned(),
                    Some((query, top_k)) => self.retrieve(k, &query, top_k).await?,
                }
            };
            results.push(ToolResult { call, output });
        }
        Some(ToolRound { results })
    }

    /// One retrieval: the chunks as a JSON array, or the failure notice;
    /// `None` on a disconnect.
    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    async fn retrieve(&mut self, k: &KnowledgeTurn, query: &str, top_k: usize) -> Option<String> {
        let started = Instant::now();
        let found = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return None,
            () = self.tx.closed() => return None,
            r = k.retriever.search(query, top_k) => r,
        };
        let latency_ms = started.elapsed().as_millis();
        // mini_chat_knowledge_search{result}, _latency_ms, _chunks (log lines)
        Some(
            match found.map(|chunks| (chunks.len(), serde_json::to_string(&chunks))) {
                Ok((chunks, Ok(json))) => {
                    self.counters.file_search += 1;
                    info!(turn_id = %self.t.turn.id, result = "ok", latency_ms, chunks, "knowledge search");
                    json
                }
                Ok((_, Err(err))) => {
                    warn!(turn_id = %self.t.turn.id, result = "error", %err, "knowledge search output not encodable");
                    SEARCH_FAILED_OUTPUT.to_owned()
                }
                Err(err) => {
                    warn!(turn_id = %self.t.turn.id, result = "error", latency_ms, %err, "knowledge search");
                    SEARCH_FAILED_OUTPUT.to_owned()
                }
            },
        )
    }

    /// Stream one provider response until its terminal event or a disconnect.
    async fn drive_iteration(&mut self) -> Outcome {
        let opened = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Outcome::Cancelled,
            () = self.tx.closed() => return Outcome::Cancelled,
            r = self.deps.llm.stream(&self.t.provider, &self.t.request, self.cancel.clone()) => r,
        };
        let mut stream = match opened {
            Ok(s) => s,
            Err(e) => return Outcome::from_error(&e, None),
        };
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Outcome::Cancelled,
                () = self.tx.closed() => return Outcome::Cancelled,
                ev = stream.next() => ev,
            };
            let Some(ev) = next else {
                if self.cancel.is_cancelled() {
                    return Outcome::Cancelled;
                }
                return Outcome::Failed {
                    code: error_codes::PROVIDER_ERROR.to_owned(),
                    message: "provider stream failed".to_owned(),
                    detail: NO_TERMINAL_DETAIL.to_owned(),
                    usage: None,
                };
            };
            if let Some(outcome) = self.on_event(ev).await {
                return outcome;
            }
        }
    }

    /// Handle one provider event; `Some` ends the stream.
    async fn on_event(&mut self, ev: LlmEvent) -> Option<Outcome> {
        match ev {
            LlmEvent::TextDelta(mut content) => {
                if self.separate_next_text && !content.is_empty() {
                    content.insert_str(0, "\n\n");
                    self.separate_next_text = false;
                }
                self.text.push_str(&content);
                self.forward(SseEvent::Delta(DeltaData {
                    kind: DeltaKind::Text,
                    content,
                }))
                .await
            }
            LlmEvent::ReasoningDelta(content) => {
                self.forward(SseEvent::Delta(DeltaData {
                    kind: DeltaKind::Reasoning,
                    content,
                }))
                .await
            }
            LlmEvent::ToolStart { name, details } => {
                let started = {
                    let n = self.tool_starts.entry(name.clone()).or_default();
                    *n += 1;
                    *n
                };
                if let Some(outcome) = self.call_limit(&name, started) {
                    return Some(outcome);
                }
                self.forward(SseEvent::Tool(ToolData {
                    phase: ToolPhase::Start,
                    name,
                    details,
                }))
                .await
            }
            LlmEvent::ToolDone { name, details } => {
                match name.as_str() {
                    WEB_SEARCH => self.counters.web_search += 1,
                    CODE_INTERPRETER => self.counters.code_interpreter += 1,
                    FILE_SEARCH => {
                        self.counters.file_search += 1;
                        self.file_search_calls += 1;
                    }
                    _ => {}
                }
                self.forward(SseEvent::Tool(ToolData {
                    phase: ToolPhase::Done,
                    name,
                    details,
                }))
                .await
            }
            LlmEvent::Citation(raw) => {
                self.citations
                    .extend(map_citation(raw, &self.t.citation_map));
                None
            }
            LlmEvent::FunctionCall(call) => {
                self.calls.push(call);
                None
            }
            LlmEvent::Completed {
                usage,
                response_id,
                incomplete_reason,
            } => Some(Outcome::Completed {
                usage,
                response_id,
                incomplete_reason,
            }),
            LlmEvent::Failed { error, usage } => Some(Outcome::from_error(&error, usage)),
        }
    }

    /// Send a content event and refresh progress; `Some(Cancelled)` when the
    /// client is gone.
    async fn forward(&mut self, ev: SseEvent) -> Option<Outcome> {
        if !self.emit(ev).await {
            return Some(Outcome::Cancelled);
        }
        self.touch_progress().await;
        None
    }

    /// Per-turn call limit of a started tool (counted on `start` events).
    fn call_limit(&self, tool: &str, started: u32) -> Option<Outcome> {
        let q = &self.deps.cfg.quota;
        match tool {
            WEB_SEARCH if started > q.web_search_max_calls_per_message => {
                Some(Outcome::limit_exceeded(
                    error_codes::WEB_SEARCH_CALLS_EXCEEDED,
                    WEB_SEARCH,
                    q.web_search_max_calls_per_message,
                ))
            }
            CODE_INTERPRETER if started > q.code_interpreter_max_calls_per_message => {
                Some(Outcome::limit_exceeded(
                    error_codes::CODE_INTERPRETER_CALLS_EXCEEDED,
                    CODE_INTERPRETER,
                    q.code_interpreter_max_calls_per_message,
                ))
            }
            _ => None,
        }
    }

    /// Refresh `last_progress_at` and the counters, at most every 30 s.
    async fn touch_progress(&mut self) {
        if self.last_touch.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last_touch = Instant::now();
        let touched = match self.deps.db.conn() {
            Ok(conn) => TurnRepo::touch_progress(&conn, self.t.turn.id, self.counters).await,
            Err(e) => Err(e),
        };
        if let Err(err) = touched {
            warn!(turn_id = %self.t.turn.id, %err, "turn progress touch failed");
        }
    }

    /// Finalize the turn, then send the terminal event the result allows.
    async fn finish(self, outcome: Outcome) {
        let t = &self.t;
        let (terminal, usage, provider_response_id) = match &outcome {
            Outcome::Completed {
                usage,
                response_id,
                incomplete_reason,
            } => (
                TerminalKind::Completed {
                    incomplete_reason: incomplete_reason.clone(),
                },
                *usage,
                response_id.clone(),
            ),
            Outcome::Failed {
                code,
                detail,
                usage,
                ..
            } => (
                TerminalKind::Failed {
                    code: code.clone(),
                    detail: detail.clone(),
                },
                *usage,
                None,
            ),
            Outcome::Cancelled => (TerminalKind::Cancelled, None, None),
        };
        let thread_summary = if matches!(outcome, Outcome::Completed { .. }) {
            self.thread_summary_task().await
        } else {
            None
        };
        let scheduled = thread_summary.is_some();
        let latency_ms = i64::try_from(t.started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let result = self
            .deps
            .finalization
            .finalize(FinalizeInput {
                turn: t.turn.clone(),
                chat_model: t.chat_model.clone(),
                user_id: t.user_id,
                terminal,
                text: self.text.clone(),
                usage,
                provider_response_id,
                assistant_message_id: t.message_id,
                counters: self.counters,
                file_search_calls: self.file_search_calls,
                daily_start: t.decision.daily_start,
                monthly_start: t.decision.monthly_start,
                decision: Some((t.decision.decision, t.decision.downgrade_reason)),
                latency_ms,
                thread_summary,
            })
            .await;
        debug!(turn_id = %t.turn.id, ?result, "turn finalized");
        if scheduled && matches!(result, FinalizeResult::Won { .. }) {
            log_summary_scheduled(&t.turn);
        }

        let events = match (outcome, result) {
            (Outcome::Cancelled, _) | (_, FinalizeResult::Lost) => Vec::new(),
            (
                Outcome::Completed {
                    usage,
                    incomplete_reason,
                    ..
                },
                FinalizeResult::Won { quota_status, .. },
            ) => {
                let mut events = Vec::with_capacity(2);
                if incomplete_reason.is_none() && !self.citations.is_empty() {
                    events.push(SseEvent::Citations(CitationsData {
                        items: self.citations.clone(),
                    }));
                }
                events.push(SseEvent::Done(self.done(usage, quota_status)));
                events
            }
            (Outcome::Completed { .. }, FinalizeResult::PersistenceFailed) => {
                vec![SseEvent::error(
                    error_codes::MESSAGE_PERSISTENCE_FAILED,
                    PERSISTENCE_FAILED_MESSAGE,
                )]
            }
            (Outcome::Completed { .. }, FinalizeResult::TxFailed) => vec![SseEvent::error(
                error_codes::FINALIZATION_FAILED,
                FINALIZATION_FAILED_MESSAGE,
            )],
            // A failed stream reports its own code even when finalization failed.
            (Outcome::Failed { code, message, .. }, _) => vec![SseEvent::error(&code, message)],
        };
        for ev in events {
            if !self.emit(ev).await {
                break;
            }
        }
    }

    /// Thread-summary task of a completed turn (spec §14): only when the worker
    /// is enabled and the turn's context plan meets the trigger; the target
    /// frontier is frozen now, before the turn's assistant message exists.
    /// Failures are logged and schedule nothing (the turn is unaffected).
    async fn thread_summary_task(&self) -> Option<ThreadSummaryPayload> {
        let cfg = &self.deps.cfg.thread_summary_worker;
        let t = &self.t;
        if !cfg.enabled || !should_trigger(&t.plan, t.has_summary, cfg.compression_threshold_pct) {
            return None;
        }
        let built = match self.deps.db.conn() {
            Ok(conn) => {
                ThreadSummaryService::build_payload(
                    &conn,
                    t.turn.tenant_id,
                    t.turn.chat_id,
                    t.turn.request_id,
                )
                .await
            }
            Err(e) => Err(e),
        };
        match built {
            Ok(Some(payload)) => Some(payload),
            Ok(None) => {
                info!(turn_id = %t.turn.id, chat_id = %t.turn.chat_id, result = "not_needed", "thread summary trigger");
                None
            }
            Err(err) => {
                warn!(turn_id = %t.turn.id, %err, "thread summary scheduling failed; no summary task");
                None
            }
        }
    }

    /// `done` of a completed turn.
    fn done(&self, usage: Option<LlmUsage>, quota_status: Vec<TierStatus>) -> DoneData {
        let d = &self.t.decision;
        let usage = usage.unwrap_or_default();
        let downgrade = d.decision == QuotaDecision::Downgrade;
        DoneData {
            usage: Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            },
            effective_model: d.effective.id.clone(),
            selected_model: self.t.chat_model.clone(),
            quota_decision: d.decision.into(),
            downgrade_from: downgrade.then(|| self.t.chat_model.clone()),
            downgrade_reason: d
                .downgrade_reason
                .filter(|_| downgrade)
                .map(|r| r.as_str().to_owned()),
            quota_warnings: Some(QuotaWarning::from_status(quota_status)),
        }
    }
}
