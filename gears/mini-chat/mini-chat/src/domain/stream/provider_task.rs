//! The provider task of a turn: opens the provider stream, forwards text, reasoning and tool
//! events as they arrive (no buffering), enforces the per-message tool call limits, runs the
//! knowledge-search agentic loop, and finalizes the turn on the provider's terminal event, on a
//! limit breach, on an unexpected function call or on a client disconnect.
//!
//! Agentic loop (DESIGN "Knowledge Search"): when the turn offers `search_knowledge`, a provider
//! response that ends with `search_knowledge` calls is a tool-use outcome: each call is answered
//! (a retrieval, or "search limit reached" past `max_calls_per_message`), the calls and their
//! outputs are appended to the input and the next request is issued, at most
//! `max_calls_per_message + 2` requests in total (beyond: `agentic_iterations_exceeded`). Only
//! the final response's usage is settled.
//!
//! A client disconnect is the cancellation token firing (the relay was dropped) or a failed
//! channel send: the provider stream is dropped at once (which cancels the provider call), the
//! turn is finalized as `cancelled` and nothing is sent.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use opentelemetry::KeyValue;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::AccessScope;
use uuid::Uuid;

use super::events::{CitationDto, CitationSource, DeltaKind, StreamEvent, TextSpan, ToolPhase};
use super::finalize::{FinalizeResult, TerminalOutcome, ToolCounts, finalize_turn};
use super::knowledge::{self, AgenticLoop, CallPlan, SEARCH_KNOWLEDGE};
use super::{StreamService, TurnContext};
use crate::infra::db::repo::turns::{self, ToolCounter};
use crate::infra::db::ts::db_now;
use crate::infra::llm::{
    ChatAdapter, InputItem, ProviderError, ProviderEvent, ProviderEventStream, ProviderRequest,
    ProviderUsage, RawCitation,
};

/// `last_progress_at` is refreshed at most this often.
const PROGRESS_REFRESH: Duration = Duration::from_secs(30);

const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";
const FILE_SEARCH: &str = "file_search";

const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
const UNEXPECTED_TOOL_USE_MESSAGE: &str = "The model requested a tool that is not available";

/// Label of the cancellation metrics (the only P1 cancellation source).
const TRIGGER_DISCONNECT: &str = "disconnect";

/// Runs the provider call of `turn`, sending events into `tx`.
pub async fn run(
    svc: StreamService,
    turn: TurnContext,
    adapter: Arc<dyn ChatAdapter>,
    request: ProviderRequest,
    tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
) {
    let mut task = TurnRun::new(&svc, &turn, &tx);
    let opened = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        result = adapter.stream(&turn.target, request.clone()) => Some(result),
    };
    match opened {
        None => {
            let disconnect = Disconnect::by_token();
            task.disconnected(disconnect, Instant::now()).await;
        }
        Some(Err(err)) => task.provider_failed(&err).await,
        Some(Ok(events)) => task.relay(&adapter, request, events, &cancel).await,
    }
}

/// A `search_knowledge` call of the current provider response.
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// How a client disconnect was observed.
struct Disconnect {
    at: Instant,
    /// The cancellation token fired (else: a failed channel send).
    by_token: bool,
}

impl Disconnect {
    fn by_token() -> Self {
        Self {
            at: Instant::now(),
            by_token: true,
        }
    }

    fn by_send() -> Self {
        Self {
            at: Instant::now(),
            by_token: false,
        }
    }
}

/// What the read loop does after one provider event.
enum Step {
    Continue,
    /// The provider stream is no longer needed; the turn ends with `End`.
    Stop(End),
}

enum End {
    Disconnected(Disconnect),
    /// The response ended with `search_knowledge` calls: answer them and continue the loop.
    ToolUse,
    Failed {
        code: &'static str,
        message: &'static str,
        /// Usage the provider reported before the failure (settled as `actual`).
        usage: Option<ProviderUsage>,
    },
    ProviderFailed(ProviderError),
    Completed {
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        citations: Vec<RawCitation>,
    },
    Incomplete {
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        reason: String,
    },
}

/// Per-turn state of the provider task.
struct TurnRun<'a> {
    svc: &'a StreamService,
    turn: &'a TurnContext,
    tx: &'a mpsc::Sender<StreamEvent>,
    /// Accumulated answer text.
    text: String,
    /// A `delta` was sent (`stream_disconnected` stage).
    content_sent: bool,
    web_search_started: u32,
    code_interpreter_started: u32,
    completed: ToolCounts,
    last_progress: Instant,
    /// The knowledge-search loop, when the turn offers `search_knowledge`.
    agentic: Option<AgenticLoop>,
    /// `search_knowledge` calls of the current response.
    pending_calls: Vec<PendingCall>,
    /// Usage of the last response that ended with a tool use (not settled unless the loop
    /// fails on its iteration cap).
    tool_use_usage: Option<ProviderUsage>,
}

impl<'a> TurnRun<'a> {
    fn new(
        svc: &'a StreamService,
        turn: &'a TurnContext,
        tx: &'a mpsc::Sender<StreamEvent>,
    ) -> Self {
        Self {
            svc,
            turn,
            tx,
            text: String::new(),
            content_sent: false,
            web_search_started: 0,
            code_interpreter_started: 0,
            completed: ToolCounts::default(),
            last_progress: Instant::now(),
            agentic: turn.knowledge.clone().map(AgenticLoop::new),
            pending_calls: Vec::new(),
            tool_use_usage: None,
        }
    }

    /// Reads provider events until the turn ends (following the agentic loop from response to
    /// response), then drops the provider stream and finalizes.
    async fn relay(
        &mut self,
        adapter: &Arc<dyn ChatAdapter>,
        mut request: ProviderRequest,
        mut events: ProviderEventStream,
        cancel: &CancellationToken,
    ) {
        let end = loop {
            let end = self.read(&mut events, cancel).await;
            drop(events);
            if !matches!(end, End::ToolUse) {
                break end;
            }
            match self.next_iteration(adapter, &mut request, cancel).await {
                Ok(next) => events = next,
                Err(end) => break end,
            }
        };
        let aborted_at = Instant::now();
        self.finish(end, aborted_at).await;
    }

    /// Reads one provider response until it ends.
    async fn read(&mut self, events: &mut ProviderEventStream, cancel: &CancellationToken) -> End {
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                event = events.next() => Some(event),
            };
            let Some(event) = next else {
                return End::Disconnected(Disconnect::by_token());
            };
            match self.on_event(event).await {
                Step::Continue => {}
                Step::Stop(end) => return end,
            }
        }
    }

    /// Answers the pending `search_knowledge` calls, appends them and their outputs to the input
    /// and opens the next provider response; `Err` ends the turn.
    async fn next_iteration(
        &mut self,
        adapter: &Arc<dyn ChatAdapter>,
        request: &mut ProviderRequest,
        cancel: &CancellationToken,
    ) -> Result<ProviderEventStream, End> {
        let Some(agentic) = self.agentic.as_ref() else {
            return Err(unexpected_tool_use());
        };
        if !agentic.may_continue() {
            tracing::warn!(turn_id = %self.turn.turn_id, max = agentic.params.max_iterations(),
                "knowledge search loop exceeded its iteration cap");
            return Err(End::Failed {
                code: AGENTIC_ITERATIONS_EXCEEDED,
                message: "The model exceeded the knowledge search iteration limit",
                usage: self.tool_use_usage,
            });
        }
        for call in std::mem::take(&mut self.pending_calls) {
            let output = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(End::Disconnected(Disconnect::by_token())),
                output = self.answer(&call.arguments) => output,
            };
            // A retrieval can be slow: keep `last_progress_at` fresh for the orphan watchdog.
            self.progress().await;
            request.input.push(InputItem::FunctionCall {
                call_id: call.call_id.clone(),
                name: call.name,
                arguments: call.arguments,
            });
            request.input.push(InputItem::FunctionCallOutput {
                call_id: call.call_id,
                output,
            });
        }
        if let Some(agentic) = self.agentic.as_mut() {
            agentic.requests += 1;
        }
        let opened = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(End::Disconnected(Disconnect::by_token())),
            result = adapter.stream(&self.turn.target, request.clone()) => result,
        };
        opened.map_err(End::ProviderFailed)
    }

    /// The function output of one `search_knowledge` call. Only an executed retrieval is
    /// counted: in memory before it runs (`file_search_calls` of the usage and audit events, so a
    /// failed retrieval counts) and on the turn row (`file_search_completed_count`) when it
    /// succeeds.
    async fn answer(&mut self, arguments: &str) -> String {
        let Some(agentic) = self.agentic.as_mut() else {
            return String::new();
        };
        let plan = agentic.plan(arguments);
        let (
            CallPlan::Search {
                query,
                max_num_results,
            },
            Some(retriever),
        ) = (&plan, &self.svc.knowledge)
        else {
            debug_assert!(
                !matches!(plan, CallPlan::Search { .. }),
                "knowledge params are only built with a retriever"
            );
            return knowledge::static_output(&plan);
        };
        let params = agentic.params.clone();
        self.completed.file_search += 1;
        let (output, ok) = knowledge::search(
            retriever,
            &params,
            &self.svc.metrics,
            query,
            *max_num_results,
        )
        .await;
        if ok {
            self.bump_counter(ToolCounter::FileSearch).await;
        }
        output
    }

    /// Handles one provider event (`None`: the stream ended without a terminal event).
    async fn on_event(&mut self, event: Option<ProviderEvent>) -> Step {
        let Some(event) = event else {
            return Step::Stop(End::ProviderFailed(ProviderError::provider(
                "provider stream ended without a terminal event",
            )));
        };
        match event {
            ProviderEvent::TextDelta(delta) => {
                self.text.push_str(&delta);
                self.content(DeltaKind::Text, delta).await
            }
            ProviderEvent::ReasoningDelta(delta) => self.content(DeltaKind::Reasoning, delta).await,
            ProviderEvent::ToolStart { name, details } => {
                if let Some(code) = self.start_tool(&name) {
                    return Step::Stop(code);
                }
                self.tool(ToolPhase::Start, name, details).await
            }
            ProviderEvent::ToolDone { name, details } => {
                self.complete_tool(&name).await;
                self.tool(ToolPhase::Done, name, details).await
            }
            ProviderEvent::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                if self.agentic.is_some() && name == SEARCH_KNOWLEDGE {
                    self.pending_calls.push(PendingCall {
                        call_id,
                        name,
                        arguments,
                    });
                    return Step::Continue;
                }
                // The only function tool is `search_knowledge`, offered with knowledge search.
                tracing::warn!(turn_id = %self.turn.turn_id, tool = %name, "unexpected function call");
                Step::Stop(unexpected_tool_use())
            }
            ProviderEvent::Completed { usage, .. } if !self.pending_calls.is_empty() => {
                // A tool-use outcome: only the final response's usage is settled.
                self.tool_use_usage = usage;
                Step::Stop(End::ToolUse)
            }
            ProviderEvent::Completed {
                response_id,
                usage,
                citations,
            } => Step::Stop(End::Completed {
                response_id,
                usage,
                citations,
            }),
            ProviderEvent::Incomplete {
                response_id,
                usage,
                reason,
            } => Step::Stop(End::Incomplete {
                response_id,
                usage,
                reason,
            }),
            ProviderEvent::Failed(err) => Step::Stop(End::ProviderFailed(err)),
        }
    }

    async fn content(&mut self, kind: DeltaKind, content: String) -> Step {
        self.content_sent = true;
        self.progress().await;
        self.forward(StreamEvent::Delta { kind, content }).await
    }

    async fn tool(&mut self, phase: ToolPhase, name: String, details: Value) -> Step {
        self.progress().await;
        self.forward(StreamEvent::Tool {
            phase,
            name,
            details,
        })
        .await
    }

    /// Sends a non-terminal event; a closed channel is a client disconnect.
    async fn forward(&self, event: StreamEvent) -> Step {
        if self.tx.send(event).await.is_err() {
            return Step::Stop(End::Disconnected(Disconnect::by_send()));
        }
        Step::Continue
    }

    /// Counts a started tool call; the end of the turn when it exceeds the per-message limit.
    fn start_tool(&mut self, name: &str) -> Option<End> {
        let quota = &self.svc.cfg.quota;
        let (started, limit, code, message) = match name {
            WEB_SEARCH => (
                &mut self.web_search_started,
                quota.web_search_max_calls_per_message,
                WEB_SEARCH_CALLS_EXCEEDED,
                "The model exceeded the web search call limit of one message",
            ),
            CODE_INTERPRETER => (
                &mut self.code_interpreter_started,
                quota.code_interpreter_max_calls_per_message,
                CODE_INTERPRETER_CALLS_EXCEEDED,
                "The model exceeded the code interpreter call limit of one message",
            ),
            _ => return None,
        };
        *started += 1;
        (*started > limit).then(|| {
            tracing::warn!(turn_id = %self.turn.turn_id, tool = %name, limit, "tool call limit exceeded");
            End::Failed {
                code,
                message,
                usage: None,
            }
        })
    }

    /// Counts a completed tool call in memory (usage event, settlement) and on the turn row.
    async fn complete_tool(&mut self, name: &str) {
        let counter = match name {
            WEB_SEARCH => {
                self.completed.web_search += 1;
                ToolCounter::WebSearch
            }
            CODE_INTERPRETER => {
                self.completed.code_interpreter += 1;
                ToolCounter::CodeInterpreter
            }
            FILE_SEARCH => {
                self.completed.file_search += 1;
                ToolCounter::FileSearch
            }
            _ => return,
        };
        self.bump_counter(counter).await;
    }

    /// Increments a completed-tool counter of the turn row (read by the orphan watchdog).
    async fn bump_counter(&self, counter: ToolCounter) {
        let scope = AccessScope::for_tenant(self.turn.tenant_id);
        let updated = match self.svc.db.conn() {
            Ok(conn) => turns::add_tool_completion(&conn, &scope, self.turn.turn_id, counter).await,
            Err(err) => Err(err),
        };
        if let Err(err) = updated {
            tracing::warn!(turn_id = %self.turn.turn_id, error = %err, "tool counter update failed");
        }
    }

    /// Refreshes `last_progress_at` when the last refresh is [`PROGRESS_REFRESH`] old.
    async fn progress(&mut self) {
        let now = Instant::now();
        if !progress_due(self.last_progress, now) {
            return;
        }
        self.last_progress = now;
        let scope = AccessScope::for_tenant(self.turn.tenant_id);
        let updated = match self.svc.db.conn() {
            Ok(conn) => turns::touch_progress(&conn, &scope, self.turn.turn_id, db_now()).await,
            Err(err) => Err(err),
        };
        if let Err(err) = updated {
            tracing::warn!(turn_id = %self.turn.turn_id, error = %err, "progress update failed");
        }
    }

    /// Finalizes the turn after the provider stream was dropped at `aborted_at`.
    async fn finish(&mut self, end: End, aborted_at: Instant) {
        match end {
            End::Disconnected(disconnect) => self.disconnected(disconnect, aborted_at).await,
            // `relay` resolves every tool-use outcome before finishing.
            End::ToolUse => {
                let outcome = TerminalOutcome::Failed {
                    error_code: UNEXPECTED_TOOL_USE.to_owned(),
                    client_message: UNEXPECTED_TOOL_USE_MESSAGE.to_owned(),
                };
                self.finalize_and_emit(outcome, None, None, Vec::new())
                    .await;
            }
            End::Failed {
                code,
                message,
                usage,
            } => {
                let outcome = TerminalOutcome::Failed {
                    error_code: code.to_owned(),
                    client_message: message.to_owned(),
                };
                self.finalize_and_emit(outcome, usage, None, Vec::new())
                    .await;
            }
            End::ProviderFailed(err) => self.provider_failed(&err).await,
            End::Completed {
                response_id,
                usage,
                citations,
            } => {
                let citations = map_citations(&citations, &self.turn.file_citation_map);
                let outcome = TerminalOutcome::Completed {
                    incomplete_reason: None,
                };
                self.finalize_and_emit(outcome, usage, response_id, citations)
                    .await;
            }
            End::Incomplete {
                response_id,
                usage,
                reason,
            } => {
                let outcome = TerminalOutcome::Completed {
                    incomplete_reason: Some(reason),
                };
                self.finalize_and_emit(outcome, usage, response_id, Vec::new())
                    .await;
            }
        }
    }

    /// Finalizes a provider failure (settled on its usage when known) and sends its `error`.
    async fn provider_failed(&self, err: &ProviderError) {
        let outcome = TerminalOutcome::Failed {
            error_code: err.sse_code().to_owned(),
            client_message: err.client_message(),
        };
        self.finalize_and_emit(outcome, err.usage, None, Vec::new())
            .await;
    }

    async fn finalize_and_emit(
        &self,
        outcome: TerminalOutcome,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
        citations: Vec<CitationDto>,
    ) {
        let result = finalize_turn(
            self.svc,
            self.turn,
            outcome,
            &self.text,
            usage,
            response_id,
            self.completed,
            citations,
        )
        .await;
        emit(self.tx, result).await;
    }

    /// Records the disconnect metrics and finalizes the turn as `cancelled`; nothing is sent.
    async fn disconnected(&self, disconnect: Disconnect, aborted_at: Instant) {
        let metrics = &self.svc.metrics;
        let trigger = [KeyValue::new("trigger", TRIGGER_DISCONNECT)];
        if disconnect.by_token {
            metrics.cancel_requested.add(1, &trigger);
        }
        #[allow(clippy::cast_precision_loss)] // histogram sample in milliseconds
        metrics.time_to_abort_ms.record(
            aborted_at
                .saturating_duration_since(disconnect.at)
                .as_millis() as f64,
            &trigger,
        );
        let stage = if self.content_sent {
            "mid_stream"
        } else {
            "before_first_token"
        };
        metrics
            .stream_disconnected
            .add(1, &[KeyValue::new("stage", stage)]);

        let result = finalize_turn(
            self.svc,
            self.turn,
            TerminalOutcome::Cancelled,
            &self.text,
            None,
            None,
            self.completed,
            Vec::new(),
        )
        .await;
        if result == FinalizeResult::CasLost {
            tracing::debug!(turn_id = %self.turn.turn_id, "cancelled turn was already finalized");
        }
    }
}

fn unexpected_tool_use() -> End {
    End::Failed {
        code: UNEXPECTED_TOOL_USE,
        message: UNEXPECTED_TOOL_USE_MESSAGE,
        usage: None,
    }
}

/// Whether `last_progress_at` (last refreshed at `last`) is due for a refresh at `now`.
fn progress_due(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= PROGRESS_REFRESH
}

/// Sends the terminal events of a finalization. A lost CAS sends nothing: the relay then
/// reports `stream_interrupted`.
async fn emit(tx: &mpsc::Sender<StreamEvent>, result: FinalizeResult) {
    match result {
        FinalizeResult::Done(done, citations) => {
            if !citations.is_empty() {
                deliver(tx, StreamEvent::Citations(citations)).await;
            }
            deliver(tx, StreamEvent::Done(done)).await;
        }
        FinalizeResult::Error { code, message } => {
            deliver(tx, StreamEvent::Error { code, message }).await;
        }
        FinalizeResult::CasLost | FinalizeResult::NoEvent => {}
    }
}

/// Sends `event`; a closed channel means the client is gone and the event is dropped (the turn
/// is already finalized).
async fn deliver(tx: &mpsc::Sender<StreamEvent>, event: StreamEvent) {
    if tx.send(event).await.is_err() {
        tracing::debug!("client disconnected before the terminal event");
    }
}

/// Client citations: web citations as reported; file citations of known attachments (by
/// provider file id) with the attachment id and filename; unknown files are dropped.
fn map_citations(raw: &[RawCitation], files: &HashMap<String, (Uuid, String)>) -> Vec<CitationDto> {
    raw.iter()
        .filter_map(|citation| match citation {
            RawCitation::Web {
                url,
                title,
                snippet,
                span,
            } => Some(CitationDto {
                source: CitationSource::Web,
                title: title.clone(),
                url: Some(url.clone()),
                attachment_id: None,
                snippet: snippet.clone(),
                span: span.map(|(start, end)| TextSpan { start, end }),
            }),
            RawCitation::File {
                provider_file_id, ..
            } => files
                .get(provider_file_id)
                .map(|(id, filename)| CitationDto {
                    source: CitationSource::File,
                    title: filename.clone(),
                    url: None,
                    attachment_id: Some(*id),
                    snippet: String::new(),
                    span: None,
                }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_refreshed_at_most_every_30_seconds() {
        let last = Instant::now();
        assert!(!progress_due(last, last));
        assert!(!progress_due(last, last + Duration::from_secs(29)));
        assert!(progress_due(last, last + Duration::from_secs(30)));
        assert!(progress_due(last, last + Duration::from_secs(95)));
    }
}
