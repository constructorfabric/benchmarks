//! Provider task of a live turn (DESIGN section 3.3 "Provider Event
//! Translation", "SSE Event Ordering", "Citation File ID and Title
//! Resolution", "Web Search Quota Enforcement" / "Code Interpreter Quota
//! Enforcement" mid-turn limits; section 3.6 "Streaming Cancellation";
//! section 5.7 "Terminal SSE Event Emission Guard").
//!
//! The task opens the provider stream, reads it incrementally and forwards
//! every client-visible event over the bounded channel to the SSE relay
//! (awaiting each send, so a slow client applies backpressure):
//!
//! - text deltas are forwarded and accumulated (the assistant content),
//!   reasoning deltas are only forwarded;
//! - tool starts are counted per tool; the start above
//!   `quota.web_search_max_calls_per_message` (or the code interpreter limit)
//!   cancels the provider stream and fails the turn with
//!   `web_search_calls_exceeded` (`code_interpreter_calls_exceeded`) without
//!   usage; tool completions are counted and persisted on the running turn;
//! - text deltas and tool events refresh `last_progress_at` at most every
//!   progress interval (30 s);
//! - annotations are collected and mapped to citations at the end;
//! - a function tool call is recorded; a provider response that requested
//!   one runs the knowledge-search agentic loop below when every call is
//!   `search_knowledge` and knowledge search is on for the turn, otherwise it
//!   ends the turn as `failed` with `unexpected_tool_use` and the usage of
//!   that response.
//!
//! **Knowledge-search agentic loop** (DESIGN section 4 "Knowledge Search").
//! A response that ends with `search_knowledge` calls ends the provider
//! request; the task serves the calls in order (a retrieval through the
//! knowledge retriever, at most `knowledge_search.max_calls_per_message` per
//! message, later calls get a "search limit reached" output), appends the
//! response's text, the calls and their outputs to the input and issues the
//! next provider request, relaying its events into the same stream. The loop
//! is capped at `max_calls_per_message + 2` provider requests: a response
//! that still asks for a tool at the cap fails the turn with
//! `agentic_iterations_exceeded`. A successful retrieval increments
//! `file_search_completed_count` (persisted at once, for the orphan
//! watchdog); the `file_search_calls` of the usage and audit events counts
//! every retrieval that ran, failed ones included. Only the final provider
//! iteration's usage is settled (ADR-0008). Cancellation is checked during
//! retrievals and between iterations exactly as during streaming.
//!
//! **Cancellation.** The relay cancels the turn token when it is dropped;
//! a closed channel (the receiver was dropped, or a send failed) is treated
//! the same. The task then cancels the provider stream and finalizes the
//! turn as `cancelled` with the partial text, sending nothing.
//!
//! **Pre-provider failures.** When the adapter fails before any outbound
//! call ([`LlmFailure::request_sent`] is `false`), the client still receives
//! `provider_error`, but the turn is stored as `turn_setup_failed` and settled
//! `released` (DESIGN sections 5.7-5.9, see [`super::finalize`]). This holds
//! only while no provider request of the turn was sent: once an agentic
//! iteration reached the provider (and streamed to the client), a later
//! iteration's failure is a sent-request failure whatever the adapter
//! reports, so the turn is stored as a provider failure and settled from the
//! usage (actual or estimated), never released.
//!
//! **Terminal events** are sent only after the finalization transaction
//! committed: `citations` (normally completed stream with at least one mapped
//! item) then `done` with the quota warnings, or `error`. A CAS loser sends
//! nothing; the relay then reports `stream_interrupted` when the client is
//! still connected.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::events::{
    AGENTIC_ITERATIONS_EXCEEDED, AGENTIC_ITERATIONS_EXCEEDED_MESSAGE,
    CODE_INTERPRETER_CALLS_EXCEEDED, CODE_INTERPRETER_CALLS_EXCEEDED_MESSAGE, Citation,
    CitationSource, Delta, DeltaKind, Done, DoneUsage, FINALIZATION_FAILED,
    FINALIZATION_FAILED_MESSAGE, MESSAGE_PERSISTENCE_FAILED, MESSAGE_PERSISTENCE_FAILED_MESSAGE,
    PROVIDER_ERROR, StreamEvent, TextSpan, ToolActivity, ToolPhase, UNEXPECTED_TOOL_USE,
    UNEXPECTED_TOOL_USE_MESSAGE, WEB_SEARCH_CALLS_EXCEEDED, WEB_SEARCH_CALLS_EXCEEDED_MESSAGE,
};
use super::finalize::{Finalized, Finalizer, StreamOutcome, TurnContext, TurnToolCalls};
use crate::domain::billing::TerminalState;
use crate::domain::knowledge::{
    KnowledgeTurn, SEARCH_KNOWLEDGE_TOOL, SearchArguments, invalid_arguments_output,
    limit_reached_output, parse_arguments, results_output, search_failed_output,
};
use crate::domain::llm::{
    LlmAnnotation, LlmAnnotationKind, LlmEvent, LlmFailure, LlmFailureKind, LlmFunctionCall,
    LlmInputItem, LlmProvider, LlmRequest, LlmUsage, ProviderTarget, TOOL_CODE_INTERPRETER,
    TOOL_FILE_SEARCH, TOOL_WEB_SEARCH,
};
use crate::domain::ports::{ToolCallCounts, TurnRepository, storage_now};
use crate::domain::quota_service::QuotaService;

/// Message of a provider stream that ended without a terminal event.
pub const STREAM_ENDED_MESSAGE: &str = "provider stream ended unexpectedly";

/// Default interval of `last_progress_at` refreshes.
pub const PROGRESS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Mid-turn limits of the provider task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskLimits {
    /// `quota.web_search_max_calls_per_message`.
    pub web_search_max_calls: u32,
    /// `quota.code_interpreter_max_calls_per_message`.
    pub code_interpreter_max_calls: u32,
    /// Minimum interval between `last_progress_at` refreshes.
    pub progress_interval: Duration,
}

/// Attachment a provider file id resolves to (citation title and id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitedFile {
    pub attachment_id: Uuid,
    pub filename: String,
}

/// The provider-call part of a live turn.
pub struct ProviderTask {
    pub turn: Arc<TurnContext>,
    pub finalizer: Finalizer,
    pub turns: Arc<dyn TurnRepository>,
    pub quota: Arc<QuotaService>,
    pub provider: Arc<dyn LlmProvider>,
    pub target: ProviderTarget,
    pub request: LlmRequest,
    pub limits: TaskLimits,
    /// `provider_file_id -> attachment` of the chat's ready attachments
    /// (built only when `file_search` is sent).
    pub cited_files: HashMap<String, CitedFile>,
    /// Knowledge search of the turn (`search_knowledge` offered); `None`
    /// makes every function call an `unexpected_tool_use`.
    pub knowledge: Option<KnowledgeTurn>,
    pub events: mpsc::Sender<StreamEvent>,
    /// Turn cancellation token (cancelled by the relay on drop).
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for ProviderTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderTask")
            .field("turn_id", &self.turn.turn_id)
            .field("limits", &self.limits)
            .field("knowledge", &self.knowledge)
            .finish_non_exhaustive()
    }
}

/// How the provider phase ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    Completed {
        usage: Option<LlmUsage>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<LlmUsage>,
        /// `false` when the adapter failed before any outbound call.
        request_sent: bool,
    },
    Cancelled,
}

/// How one provider request (iteration) ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Iteration {
    /// The provider phase of the turn ended.
    End(End),
    /// The response requested `search_knowledge` calls (knowledge search on,
    /// every call to that tool); `usage` is the response's usage.
    ToolUse {
        calls: Vec<LlmFunctionCall>,
        usage: Option<LlmUsage>,
    },
}

/// Mutable state of the relay loop.
#[derive(Debug)]
struct RelayState {
    /// Answer text of every iteration.
    text: String,
    web_search_started: u32,
    code_interpreter_started: u32,
    counts: ToolCallCounts,
    /// `file_search_calls` of the usage and audit events: provider
    /// `file_search` completions and every `search_knowledge` retrieval that
    /// ran (failed ones included).
    file_search_calls: u32,
    /// `search_knowledge` retrievals run in this turn.
    knowledge_retrievals: u32,
    annotations: Vec<LlmAnnotation>,
    /// Function tool calls of the current provider response.
    function_calls: Vec<LlmFunctionCall>,
    /// A provider request of this turn was already sent (an earlier
    /// agentic iteration got its response): every later failure counts as
    /// `request_sent`.
    request_sent: bool,
    last_progress: Instant,
}

impl RelayState {
    fn new() -> Self {
        Self {
            text: String::new(),
            web_search_started: 0,
            code_interpreter_started: 0,
            counts: ToolCallCounts::default(),
            file_search_calls: 0,
            knowledge_retrievals: 0,
            annotations: Vec::new(),
            function_calls: Vec::new(),
            request_sent: false,
            last_progress: Instant::now(),
        }
    }

    const fn tool_calls(&self) -> TurnToolCalls {
        TurnToolCalls {
            counts: self.counts,
            file_search_calls: self.file_search_calls,
        }
    }
}

/// Next step of the relay loop.
enum Step {
    Continue,
    End(Iteration),
}

/// The client-safe error of a provider failure: the streaming code and the
/// sanitized message (a `rate_limited` message names the `Retry-After` delay
/// when the provider sent one).
#[must_use]
pub fn failure_error(failure: &LlmFailure) -> (String, String) {
    let mut message = failure.message_sanitized.clone();
    if failure.kind == LlmFailureKind::RateLimited
        && let Some(secs) = failure.retry_after_secs
        && !message.contains(&secs.to_string())
    {
        message = format!("{message} (retry after {secs} seconds)");
    }
    (failure.kind.code().to_owned(), message)
}

/// The end of a provider failure; `earlier_request_sent` (a provider
/// request of this turn was already sent) makes it a sent-request failure
/// even when the adapter failed before its own outbound call.
fn failed(failure: &LlmFailure, earlier_request_sent: bool) -> End {
    let (code, message) = failure_error(failure);
    End::Failed {
        code,
        message,
        usage: failure.usage,
        request_sent: failure.request_sent || earlier_request_sent,
    }
}

/// Maps provider annotations to client citations: web citations keep url,
/// title, snippet and span; file citations resolve the provider file id to
/// the attachment (title = filename, empty snippet, no span) and are omitted
/// when the file is unknown (or its attachment deleted).
#[must_use]
pub fn map_citations<S: std::hash::BuildHasher>(
    annotations: &[LlmAnnotation],
    files: &HashMap<String, CitedFile, S>,
) -> Vec<Citation> {
    annotations
        .iter()
        .filter_map(|annotation| match annotation.kind {
            LlmAnnotationKind::Url => {
                let url = annotation.url.clone()?;
                Some(Citation {
                    source: CitationSource::Web,
                    title: annotation.title.clone().unwrap_or_else(|| url.clone()),
                    url: Some(url),
                    attachment_id: None,
                    snippet: annotation.snippet.clone(),
                    span: annotation
                        .start
                        .zip(annotation.end)
                        .map(|(start, end)| TextSpan { start, end }),
                })
            }
            LlmAnnotationKind::File => {
                let file = files.get(annotation.file_id.as_deref()?)?;
                Some(Citation {
                    source: CitationSource::File,
                    title: file.filename.clone(),
                    url: None,
                    attachment_id: Some(file.attachment_id),
                    snippet: String::new(),
                    span: None,
                })
            }
        })
        .collect()
}

impl ProviderTask {
    /// Runs the task to its end (module docs).
    pub async fn run(self) {
        let mut state = RelayState::new();
        let end = self.drive(&mut state).await;
        self.finish(end, state).await;
    }

    /// The provider phase: one provider request, or the agentic loop of
    /// knowledge search (module docs).
    async fn drive(&self, state: &mut RelayState) -> End {
        let mut request = self.request.clone();
        let mut iteration: u32 = 1;
        loop {
            let text_start = state.text.len();
            let provider_cancel = self.cancel.child_token();
            let outcome = self.relay(&request, &provider_cancel, state).await;
            // Aborts the provider request when the stream did not end by itself.
            provider_cancel.cancel();
            let (calls, usage) = match outcome {
                Iteration::End(end) => return end,
                Iteration::ToolUse { calls, usage } => (calls, usage),
            };
            // This iteration's request reached the provider.
            state.request_sent = true;
            let Some(knowledge) = self.knowledge.as_ref() else {
                // `handle` reports tool use only with knowledge search on.
                return unexpected_tool_use(usage);
            };
            if iteration >= knowledge.limits.max_iterations() {
                tracing::warn!(
                    target: "mini_chat.stream",
                    turn_id = %self.turn.turn_id,
                    iterations = iteration,
                    "knowledge search agentic loop exceeded its iteration cap"
                );
                return End::Failed {
                    code: AGENTIC_ITERATIONS_EXCEEDED.to_owned(),
                    message: AGENTIC_ITERATIONS_EXCEEDED_MESSAGE.to_owned(),
                    usage,
                    request_sent: true,
                };
            }
            let mut outputs = Vec::with_capacity(calls.len());
            for call in &calls {
                let Some(output) = self.serve_call(knowledge, call, state).await else {
                    return End::Cancelled;
                };
                outputs.push(LlmInputItem::FunctionCallOutput {
                    call_id: call.call_id.clone(),
                    output,
                });
            }
            let text = &state.text[text_start..];
            if !text.is_empty() {
                request.input.push(LlmInputItem::Assistant {
                    text: text.to_owned(),
                });
            }
            request
                .input
                .extend(calls.into_iter().map(LlmInputItem::FunctionCall));
            request.input.extend(outputs);
            iteration += 1;
        }
    }

    /// The output of one `search_knowledge` call: a retrieval while the
    /// per-message limit allows it, else "search limit reached"; `None` when
    /// the turn was cancelled during the retrieval.
    async fn serve_call(
        &self,
        knowledge: &KnowledgeTurn,
        call: &LlmFunctionCall,
        state: &mut RelayState,
    ) -> Option<String> {
        let limits = knowledge.limits;
        if state.knowledge_retrievals >= limits.max_calls {
            return Some(limit_reached_output());
        }
        let Some(arguments) = parse_arguments(&call.arguments, limits.top_k) else {
            tracing::warn!(
                target: "mini_chat.stream",
                turn_id = %self.turn.turn_id,
                "search_knowledge call without a usable query"
            );
            return Some(invalid_arguments_output());
        };
        self.retrieve(knowledge, &arguments, state).await
    }

    /// Runs one retrieval (counted in `file_search_calls` before it runs, in
    /// `file_search_completed_count` when it succeeds) and returns its
    /// output; `None` when the turn was cancelled meanwhile.
    async fn retrieve(
        &self,
        knowledge: &KnowledgeTurn,
        arguments: &SearchArguments,
        state: &mut RelayState,
    ) -> Option<String> {
        state.knowledge_retrievals += 1;
        state.file_search_calls = state.file_search_calls.saturating_add(1);
        let result = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return None,
            () = self.events.closed() => return None,
            result = knowledge.retriever.search(
                knowledge.tenant_id,
                &arguments.query,
                arguments.top_k,
            ) => result,
        };
        let (output, succeeded) = match result {
            Ok(chunks) => {
                state.counts.file_search = state.counts.file_search.saturating_add(1);
                let output =
                    results_output(&chunks, arguments.top_k, knowledge.limits.max_chunk_chars);
                (output, true)
            }
            Err(err) => {
                tracing::warn!(
                    target: "mini_chat.stream",
                    turn_id = %self.turn.turn_id,
                    error = %err,
                    "knowledge search retrieval failed"
                );
                (search_failed_output(), false)
            }
        };
        // A success changed `file_search_completed_count`: persisted at once.
        self.record_progress(state, succeeded).await;
        Some(output)
    }

    /// One provider request, relayed until its terminal event.
    async fn relay(
        &self,
        request: &LlmRequest,
        provider_cancel: &CancellationToken,
        state: &mut RelayState,
    ) -> Iteration {
        let opened = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Iteration::End(End::Cancelled),
            () = self.events.closed() => return Iteration::End(End::Cancelled),
            opened = self.provider.stream(
                &self.target,
                request.clone(),
                provider_cancel.clone(),
            ) => opened,
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(failure) => return Iteration::End(failed(&failure, state.request_sent)),
        };
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Iteration::End(End::Cancelled),
                () = self.events.closed() => return Iteration::End(End::Cancelled),
                next = stream.next() => next,
            };
            let Some(event) = next else {
                if self.cancel.is_cancelled() || self.events.is_closed() {
                    return Iteration::End(End::Cancelled);
                }
                return Iteration::End(End::Failed {
                    code: PROVIDER_ERROR.to_owned(),
                    message: STREAM_ENDED_MESSAGE.to_owned(),
                    usage: None,
                    request_sent: true,
                });
            };
            if let Step::End(outcome) = self.handle(event, state).await {
                return outcome;
            }
        }
    }

    async fn handle(&self, event: LlmEvent, state: &mut RelayState) -> Step {
        match event {
            LlmEvent::TextDelta(content) => {
                state.text.push_str(&content);
                self.forward(
                    StreamEvent::Delta(Delta {
                        kind: DeltaKind::Text,
                        content,
                    }),
                    state,
                    false,
                )
                .await
            }
            LlmEvent::ReasoningDelta(content) => {
                self.forward(
                    StreamEvent::Delta(Delta {
                        kind: DeltaKind::Reasoning,
                        content,
                    }),
                    state,
                    false,
                )
                .await
            }
            LlmEvent::ToolStarted { name, details } => {
                if let Some(end) = self.count_start(&name, state) {
                    return Step::End(Iteration::End(end));
                }
                self.forward(tool(ToolPhase::Start, name, details), state, false)
                    .await
            }
            LlmEvent::ToolCompleted { name, details } => {
                let counted = count_completion(&name, &mut state.counts);
                if name == TOOL_FILE_SEARCH {
                    state.file_search_calls = state.file_search_calls.saturating_add(1);
                }
                self.forward(tool(ToolPhase::Done, name, details), state, counted)
                    .await
            }
            LlmEvent::Annotation(annotation) => {
                state.annotations.push(annotation);
                Step::Continue
            }
            LlmEvent::FunctionCall(call) => {
                state.function_calls.push(call);
                Step::Continue
            }
            LlmEvent::Completed {
                response_id,
                usage,
                incomplete_reason,
                output_text: _,
            } => {
                let calls = std::mem::take(&mut state.function_calls);
                if calls.is_empty() {
                    return Step::End(Iteration::End(End::Completed {
                        usage,
                        response_id,
                        incomplete_reason,
                    }));
                }
                let handled = self.knowledge.is_some()
                    && calls.iter().all(|call| call.name == SEARCH_KNOWLEDGE_TOOL);
                if handled {
                    return Step::End(Iteration::ToolUse { calls, usage });
                }
                let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
                tracing::warn!(
                    target: "mini_chat.stream",
                    turn_id = %self.turn.turn_id,
                    tools = ?names,
                    knowledge_search = self.knowledge.is_some(),
                    "the model requested a function tool the turn does not handle"
                );
                Step::End(Iteration::End(unexpected_tool_use(usage)))
            }
            LlmEvent::Failed(failure) => {
                Step::End(Iteration::End(failed(&failure, state.request_sent)))
            }
        }
    }

    /// Counts a tool start; the end of the turn when a per-message limit is
    /// exceeded.
    fn count_start(&self, name: &str, state: &mut RelayState) -> Option<End> {
        if name == TOOL_WEB_SEARCH {
            state.web_search_started += 1;
            if state.web_search_started > self.limits.web_search_max_calls {
                return Some(End::Failed {
                    code: WEB_SEARCH_CALLS_EXCEEDED.to_owned(),
                    message: WEB_SEARCH_CALLS_EXCEEDED_MESSAGE.to_owned(),
                    usage: None,
                    request_sent: true,
                });
            }
        } else if name == TOOL_CODE_INTERPRETER {
            state.code_interpreter_started += 1;
            if state.code_interpreter_started > self.limits.code_interpreter_max_calls {
                return Some(End::Failed {
                    code: CODE_INTERPRETER_CALLS_EXCEEDED.to_owned(),
                    message: CODE_INTERPRETER_CALLS_EXCEEDED_MESSAGE.to_owned(),
                    usage: None,
                    request_sent: true,
                });
            }
        }
        None
    }

    /// Sends a content event and records progress; a failed send is a
    /// client disconnect.
    async fn forward(&self, event: StreamEvent, state: &mut RelayState, counted: bool) -> Step {
        if self.events.send(event).await.is_err() {
            return Step::End(Iteration::End(End::Cancelled));
        }
        self.record_progress(state, counted).await;
        Step::Continue
    }

    /// Persists the tool counters (when they changed) and refreshes
    /// `last_progress_at` (when the interval elapsed), in one statement.
    async fn record_progress(&self, state: &mut RelayState, counts_changed: bool) {
        let due = state.last_progress.elapsed() >= self.limits.progress_interval;
        if !due && !counts_changed {
            return;
        }
        let progress_at = due.then(storage_now);
        if due {
            state.last_progress = Instant::now();
        }
        if let Err(err) = self
            .turns
            .record_progress(
                &self.turn.scope,
                self.turn.turn_id,
                state.counts,
                progress_at,
            )
            .await
        {
            tracing::warn!(
                target: "mini_chat.stream",
                turn_id = %self.turn.turn_id,
                error = %err,
                "turn progress could not be recorded"
            );
        }
    }

    async fn finish(self, end: End, state: RelayState) {
        let calls = state.tool_calls();
        let (outcome, citations) = self.outcome(end, state);

        let finalized = self.finalizer.finalize(&self.turn, &outcome, calls).await;
        if let Err(err) = &finalized {
            tracing::error!(
                target: "mini_chat.stream",
                turn_id = %self.turn.turn_id,
                error = %err,
                "turn finalization failed; the turn stays running for the orphan watchdog"
            );
        }
        for event in self.terminal_events(&outcome, finalized, citations).await {
            if self.events.send(event).await.is_err() {
                break;
            }
        }
    }

    /// The finalization outcome of `end` and the citations to send before
    /// `done` (normally completed streams only).
    fn outcome(&self, end: End, state: RelayState) -> (StreamOutcome, Vec<Citation>) {
        let RelayState {
            text, annotations, ..
        } = state;
        match end {
            End::Completed {
                usage,
                response_id,
                incomplete_reason,
            } => {
                let citations = if let Some(reason) = &incomplete_reason {
                    tracing::warn!(
                        target: "mini_chat.stream",
                        turn_id = %self.turn.turn_id,
                        reason = %reason,
                        "stream incomplete"
                    );
                    Vec::new()
                } else {
                    map_citations(&annotations, &self.cited_files)
                };
                (
                    StreamOutcome::Completed {
                        text,
                        usage,
                        response_id,
                    },
                    citations,
                )
            }
            End::Failed {
                code,
                message,
                usage,
                request_sent,
            } => (
                StreamOutcome::Failed {
                    code,
                    message,
                    usage,
                    request_sent,
                },
                Vec::new(),
            ),
            End::Cancelled => (StreamOutcome::Cancelled { text }, Vec::new()),
        }
    }

    /// The events sent after finalization (Terminal SSE Event Emission Guard).
    async fn terminal_events(
        &self,
        outcome: &StreamOutcome,
        finalized: Result<Finalized, crate::domain::error::DomainError>,
        citations: Vec<Citation>,
    ) -> Vec<StreamEvent> {
        match (outcome, finalized) {
            (StreamOutcome::Cancelled { .. }, _) | (_, Ok(Finalized::Lost)) => Vec::new(),
            (StreamOutcome::Failed { code, message, .. }, _) => {
                vec![StreamEvent::error(code.clone(), message.clone())]
            }
            (StreamOutcome::Completed { .. }, Err(_)) => vec![StreamEvent::error(
                FINALIZATION_FAILED,
                FINALIZATION_FAILED_MESSAGE,
            )],
            (StreamOutcome::Completed { usage, .. }, Ok(Finalized::Committed { terminal, .. })) => {
                if terminal != TerminalState::Completed {
                    return vec![StreamEvent::error(
                        MESSAGE_PERSISTENCE_FAILED,
                        MESSAGE_PERSISTENCE_FAILED_MESSAGE,
                    )];
                }
                let mut events = Vec::with_capacity(2);
                if !citations.is_empty() {
                    events.push(StreamEvent::Citations(citations));
                }
                events.push(StreamEvent::Done(Box::new(self.done(*usage).await)));
                events
            }
        }
    }

    async fn done(&self, usage: Option<LlmUsage>) -> Done {
        let decision = &self.turn.decision;
        let quota_warnings = match self
            .quota
            .warnings_for(
                self.turn.owner(),
                &decision.user_limits,
                OffsetDateTime::now_utc(),
            )
            .await
        {
            Ok(warnings) => Some(warnings),
            Err(err) => {
                tracing::warn!(
                    target: "mini_chat.stream",
                    turn_id = %self.turn.turn_id,
                    error = %err,
                    "quota warnings could not be computed"
                );
                None
            }
        };
        Done {
            usage: usage.map_or_else(DoneUsage::default, |usage| DoneUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            }),
            effective_model: decision.effective_model_id().to_owned(),
            selected_model: self.turn.selected_model.clone(),
            quota_decision: decision.decision,
            downgrade_from: decision.downgrade_from.clone(),
            downgrade_reason: decision.downgrade_reason,
            quota_warnings,
        }
    }
}

/// `unexpected_tool_use` with the usage of the response that asked for it.
fn unexpected_tool_use(usage: Option<LlmUsage>) -> End {
    End::Failed {
        code: UNEXPECTED_TOOL_USE.to_owned(),
        message: UNEXPECTED_TOOL_USE_MESSAGE.to_owned(),
        usage,
        request_sent: true,
    }
}

fn tool(phase: ToolPhase, name: String, details: serde_json::Value) -> StreamEvent {
    StreamEvent::Tool(ToolActivity {
        phase,
        name,
        details,
    })
}

/// Counts a completed tool call; `true` when a persisted counter changed.
fn count_completion(name: &str, counts: &mut ToolCallCounts) -> bool {
    let counter = match name {
        TOOL_WEB_SEARCH => &mut counts.web_search,
        TOOL_CODE_INTERPRETER => &mut counts.code_interpreter,
        TOOL_FILE_SEARCH => &mut counts.file_search,
        _ => return false,
    };
    *counter = counter.saturating_add(1);
    true
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
