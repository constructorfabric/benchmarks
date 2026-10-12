//! The provider task of a turn: relays provider events to the client channel, enforces the
//! mid-turn tool limits, reacts to a client disconnect and finalizes the turn (DESIGN sections 3.3,
//! 3.6 "Streaming Cancellation", 5.7).

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt};
use mini_chat_sdk::{TokenUsage, UserLimits};
use tokio::sync::mpsc;
use toolkit_security::{AccessScope, SecurityContext};
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::facts::TurnFacts;
use super::finalize::{FinalizeError, Finalizer, Terminal};
use super::model::{
    CitationItem, CitationSource, DeltaKind, DonePayload, FinalizeOutcome, StreamEvent, ToolCounts,
    ToolPhase,
};
use super::ports::TurnRepository;
use crate::domain::llm::{
    Completion, LlmEvent, LlmProvider, LlmRequest, ProviderError, RawCitation,
};
use crate::domain::metrics::Metrics;
use crate::domain::ports::TraceIdSource;
use crate::domain::quota::QuotaService;
use crate::domain::sanitize::sanitize_provider_message;
use crate::domain::turn::TurnState;

const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";
const FILE_SEARCH: &str = "file_search";

/// Message of the `message_persistence_failed` error event.
const PERSISTENCE_FAILED_MESSAGE: &str = "The answer could not be saved";
/// Message of the `finalization_failed` error event.
const FINALIZATION_FAILED_MESSAGE: &str = "The turn could not be finalized";
/// Message of the failure a panic of the provider interaction is reported as.
const PANIC_MESSAGE: &str = "The provider stream failed unexpectedly";

/// How often a finalize transaction is attempted on the failed and cancelled paths: a transient
/// transaction error must not leave the turn `running` with its reserve held.
const FINALIZE_ATTEMPTS: u32 = 3;
/// The pause before the second attempt; it doubles for each further one.
const FINALIZE_BACKOFF: Duration = Duration::from_millis(100);

/// Settings of the provider task.
#[derive(Debug, Clone, Copy)]
pub struct RunSettings {
    /// `quota.web_search_max_calls_per_message`.
    pub web_search_max_calls: u32,
    /// `quota.code_interpreter_max_calls_per_message`.
    pub code_interpreter_max_calls: u32,
    /// The shortest interval between `last_progress_at` refreshes.
    pub progress_refresh: Duration,
}

/// Shared collaborators of the provider task.
#[derive(Clone)]
pub struct RunDeps {
    /// Turn persistence.
    pub turns: Arc<dyn TurnRepository>,
    /// The quota service (warnings after the settlement).
    pub quota: Arc<QuotaService>,
    /// The finalization transaction.
    pub finalizer: Arc<Finalizer>,
    /// The metrics sink.
    pub metrics: Arc<dyn Metrics>,
    /// The trace id of the request that starts a turn.
    pub trace: Arc<dyn TraceIdSource>,
    /// Limits and intervals.
    pub settings: RunSettings,
}

/// A turn whose reserve is committed and whose provider call is about to start.
pub struct TurnRun {
    /// Collaborators.
    pub deps: RunDeps,
    /// The caller's security context (forwarded to OAGW).
    pub ctx: SecurityContext,
    /// The caller's scope on the chat.
    pub scope: AccessScope,
    /// The resolved provider.
    pub provider: Arc<dyn LlmProvider>,
    /// The provider request.
    pub request: LlmRequest,
    /// The turn.
    pub facts: TurnFacts,
    /// The user's limits (for the quota warnings of `done`).
    pub limits: UserLimits,
    /// `provider_file_id -> (attachment id, filename)` when `file_search` is in the request.
    pub file_map: HashMap<String, (Uuid, String)>,
    /// The relay channel.
    pub tx: mpsc::Sender<StreamEvent>,
}

/// How the provider interaction ended.
enum Outcome {
    Completed {
        completion: Completion,
        response_id: Option<String>,
        usage: Option<TokenUsage>,
        citations: Vec<RawCitation>,
    },
    Failed {
        code: &'static str,
        message: String,
        usage: Option<TokenUsage>,
    },
    Disconnected,
}

#[derive(Default)]
struct Progress {
    text: String,
    content_started: bool,
    first_token: Option<Instant>,
    counts: ToolCounts,
    web_search_started: u32,
    code_interpreter_started: u32,
    last_touch: Option<Instant>,
}

impl TurnRun {
    /// Runs the turn to its terminal outcome.
    pub async fn run(self) {
        let started = Instant::now();
        let provider = self.provider.provider_id().to_owned();
        let model = self.facts.effective_model.clone();
        self.deps.metrics.active_streams(1);
        self.deps.metrics.stream_started(&provider, &model);

        let mut progress = Progress {
            last_touch: Some(started),
            ..Progress::default()
        };
        // A panic of the provider interaction (adapter, parser, gateway client) must not leave the
        // turn `running`: it is finalized like any other provider failure.
        let outcome = match AssertUnwindSafe(self.relay(&mut progress))
            .catch_unwind()
            .await
        {
            Ok(outcome) => outcome,
            Err(payload) => {
                error!(
                    turn_id = %self.facts.turn_id,
                    panic = %panic_message(payload.as_ref()),
                    "the provider task panicked; finalizing the turn as failed"
                );
                Outcome::Failed {
                    code: "provider_error",
                    message: PANIC_MESSAGE.to_owned(),
                    usage: None,
                }
            }
        };
        self.deps.metrics.active_streams(-1);
        let counts = progress.counts;
        // The conclusion (finalization, metrics, terminal events) is supervised like the relay: a
        // panic there must not leave the turn `running` with its reserve held.
        if let Err(payload) =
            AssertUnwindSafe(self.conclude(outcome, progress, started, &provider, &model))
                .catch_unwind()
                .await
        {
            error!(
                turn_id = %self.facts.turn_id,
                panic = %panic_message(payload.as_ref()),
                "the turn conclusion panicked; finalizing the turn as failed"
            );
            self.finalize_after_panic(counts, started).await;
        }
    }

    /// Finalizes a turn whose conclusion panicked: failed with the estimated settlement, through
    /// the same CAS-guarded finalizer. When the conclusion had already committed the turn the CAS
    /// is lost and nothing more happens.
    async fn finalize_after_panic(&self, counts: ToolCounts, started: Instant) {
        let failed = Terminal {
            state: TurnState::Failed,
            error_code: Some("provider_error".to_owned()),
            error_detail: Some(PANIC_MESSAGE.to_owned()),
            usage: None,
            response_id: None,
            text: String::new(),
            persist_message: false,
            tool_counts: counts,
            first_token_ms: None,
            total_ms: elapsed_ms(started),
        };
        match AssertUnwindSafe(self.finalize_with_retry(&failed))
            .catch_unwind()
            .await
        {
            Ok(result) => self.report_panic_finalization(result).await,
            Err(payload) => log_repeated_panic(self.facts.turn_id, payload.as_ref()),
        }
    }

    async fn report_panic_finalization(&self, result: Result<FinalizeOutcome, FinalizeError>) {
        match result {
            Ok(FinalizeOutcome::Lost) => {
                debug!(turn_id = %self.facts.turn_id, "the turn was finalized before the panic");
            }
            Ok(FinalizeOutcome::Committed) => {
                self.emit(StreamEvent::Error {
                    code: "provider_error".to_owned(),
                    message: PANIC_MESSAGE.to_owned(),
                })
                .await;
            }
            Err(err) => {
                error!(turn_id = %self.facts.turn_id, error = %err, "the turn could not be finalized after a panic; the orphan watchdog will finalize it");
            }
        }
    }

    async fn relay(&self, progress: &mut Progress) -> Outcome {
        let started = StreamEvent::Started {
            request_id: self.facts.request_id,
            message_id: self.facts.assistant_message_id,
            is_new_turn: true,
            thread_summary_tokens: self.facts.thread_summary_tokens,
        };
        if self.tx.send(started).await.is_err() {
            return Outcome::Disconnected;
        }
        let call_started = Instant::now();
        let opened = tokio::select! {
            biased;
            () = self.tx.closed() => return Outcome::Disconnected,
            opened = self.provider.stream(&self.ctx, &self.request) => opened,
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(err) => return provider_failure(&err, None),
        };
        loop {
            let next = tokio::select! {
                biased;
                () = self.tx.closed() => return Outcome::Disconnected,
                next = stream.next() => next,
            };
            match next {
                None => {
                    return Outcome::Failed {
                        code: "provider_error",
                        message: "The provider stream ended unexpectedly".to_owned(),
                        usage: None,
                    };
                }
                Some(Err(err)) => return provider_failure(&err, None),
                Some(Ok(event)) => {
                    if let Some(outcome) = self.handle(event, progress, call_started).await {
                        return outcome;
                    }
                }
            }
        }
    }

    /// Handles one provider event; `Some` ends the interaction.
    async fn handle(
        &self,
        event: LlmEvent,
        progress: &mut Progress,
        call_started: Instant,
    ) -> Option<Outcome> {
        match event {
            LlmEvent::TextDelta(text) => {
                if progress.first_token.is_none() {
                    progress.first_token = Some(Instant::now());
                    self.deps.metrics.ttft_provider_ms(
                        self.provider.provider_id(),
                        &self.facts.effective_model,
                        call_started.elapsed().as_secs_f64() * 1000.0,
                    );
                }
                progress.text.push_str(&text);
                self.touch(progress).await;
                self.send_content(
                    progress,
                    StreamEvent::Delta {
                        kind: DeltaKind::Text,
                        content: text,
                    },
                )
                .await
            }
            LlmEvent::ReasoningDelta(text) => {
                self.touch(progress).await;
                self.send_content(
                    progress,
                    StreamEvent::Delta {
                        kind: DeltaKind::Reasoning,
                        content: text,
                    },
                )
                .await
            }
            LlmEvent::ToolStarted { name } => {
                if let Some(code) = self.count_start(progress, &name) {
                    return Some(Outcome::Failed {
                        code,
                        message: "The model made more tool calls than allowed for one message"
                            .to_owned(),
                        usage: None,
                    });
                }
                self.touch(progress).await;
                self.send_content(
                    progress,
                    StreamEvent::Tool {
                        phase: ToolPhase::Start,
                        name,
                        details: serde_json::json!({}),
                    },
                )
                .await
            }
            LlmEvent::ToolDone { name, details } => {
                self.count_done(progress, &name).await;
                self.touch(progress).await;
                self.send_content(
                    progress,
                    StreamEvent::Tool {
                        phase: ToolPhase::Done,
                        name,
                        details,
                    },
                )
                .await
            }
            LlmEvent::Completed {
                completion,
                response_id,
                usage,
                citations,
            } => Some(Outcome::Completed {
                completion,
                response_id,
                usage,
                citations,
            }),
            LlmEvent::Failed { message, usage } => Some(Outcome::Failed {
                code: "provider_error",
                message,
                usage,
            }),
        }
    }

    async fn send_content(&self, progress: &mut Progress, event: StreamEvent) -> Option<Outcome> {
        progress.content_started = true;
        match self.tx.send(event).await {
            Ok(()) => None,
            // A failed send under backpressure is a disconnect, like the cancel token.
            Err(_) => Some(Outcome::Disconnected),
        }
    }

    /// Counts a started built-in call; `Some(code)` when the per-message limit is exceeded.
    fn count_start(&self, progress: &mut Progress, name: &str) -> Option<&'static str> {
        let settings = &self.deps.settings;
        match name {
            WEB_SEARCH => {
                progress.web_search_started += 1;
                (progress.web_search_started > settings.web_search_max_calls)
                    .then_some("web_search_calls_exceeded")
            }
            CODE_INTERPRETER => {
                progress.code_interpreter_started += 1;
                (progress.code_interpreter_started > settings.code_interpreter_max_calls)
                    .then_some("code_interpreter_calls_exceeded")
            }
            _ => None,
        }
    }

    async fn count_done(&self, progress: &mut Progress, name: &str) {
        match name {
            WEB_SEARCH => progress.counts.web_search += 1,
            CODE_INTERPRETER => progress.counts.code_interpreter += 1,
            FILE_SEARCH => progress.counts.file_search += 1,
            _ => return,
        }
        if let Err(err) = self
            .deps
            .turns
            .record_tool_completed(&self.scope, self.facts.turn_id, name)
            .await
        {
            warn!(turn_id = %self.facts.turn_id, error = %err, "the tool counter could not be recorded");
        }
    }

    /// Refreshes `last_progress_at`, at most once per `progress_refresh`.
    async fn touch(&self, progress: &mut Progress) {
        let due = progress
            .last_touch
            .is_none_or(|last| last.elapsed() >= self.deps.settings.progress_refresh);
        if !due {
            return;
        }
        progress.last_touch = Some(Instant::now());
        if let Err(err) = self
            .deps
            .turns
            .touch_progress(&self.scope, self.facts.turn_id, crate::domain::clock::now())
            .await
        {
            warn!(turn_id = %self.facts.turn_id, error = %err, "the turn progress could not be refreshed");
        }
    }

    async fn conclude(
        &self,
        outcome: Outcome,
        progress: Progress,
        started: Instant,
        provider: &str,
        model: &str,
    ) {
        let total_ms = elapsed_ms(started);
        let first_token_ms = progress
            .first_token
            .map(|at| u64::try_from(at.duration_since(started).as_millis()).unwrap_or(u64::MAX));
        let terminal = |state: TurnState, text: String| Terminal {
            state,
            error_code: None,
            error_detail: None,
            usage: None,
            response_id: None,
            text,
            persist_message: false,
            tool_counts: progress.counts,
            first_token_ms,
            total_ms,
        };
        self.deps
            .metrics
            .stream_total_latency_ms(provider, model, millis_f64(total_ms));
        match outcome {
            Outcome::Completed {
                completion,
                response_id,
                usage,
                citations,
            } => {
                let mut completed = terminal(TurnState::Completed, progress.text.clone());
                completed.persist_message = true;
                completed.usage = usage;
                completed.response_id = response_id;
                self.conclude_completed(completed, completion, citations, provider, model)
                    .await;
            }
            Outcome::Failed {
                code,
                message,
                usage,
            } => {
                let sanitized = sanitize_provider_message(&message);
                let mut failed = terminal(TurnState::Failed, String::new());
                failed.error_code = Some(code.to_owned());
                failed.error_detail = Some(sanitized.clone());
                failed.usage = usage;
                self.deps.metrics.stream_failed(provider, model, code);
                self.conclude_failed(failed, code, sanitized).await;
            }
            Outcome::Disconnected => {
                let mut cancelled = terminal(TurnState::Cancelled, progress.text.clone());
                cancelled.persist_message = !progress.text.is_empty();
                self.conclude_cancelled(cancelled, progress.content_started)
                    .await;
            }
        }
    }

    /// Sends an event to the client; a closed channel means the client is gone and is not an
    /// error (the turn is already finalized by then).
    async fn emit(&self, event: StreamEvent) {
        if self.tx.send(event).await.is_err() {
            debug!(turn_id = %self.facts.turn_id, "the client is gone; an event was not delivered");
        }
    }

    async fn conclude_completed(
        &self,
        completed: Terminal,
        completion: Completion,
        citations: Vec<RawCitation>,
        provider: &str,
        model: &str,
    ) {
        let usage = completed.usage;
        match self
            .deps
            .finalizer
            .finalize(&self.scope, &self.facts, &completed)
            .await
        {
            Ok(FinalizeOutcome::Committed) => {
                self.deliver_completed(usage, &completion, citations, provider, model)
                    .await;
            }
            Ok(FinalizeOutcome::Lost) => {
                debug!(turn_id = %self.facts.turn_id, "the turn was finalized by another path");
            }
            Err(err) => {
                self.on_completed_finalize_error(err, completed, provider, model)
                    .await;
            }
        }
    }

    /// A completed turn that could not be finalized: a failed transaction downgrades the turn to
    /// `message_persistence_failed`; a settlement that cannot be computed leaves it `running` for
    /// the orphan watchdog and the client gets `finalization_failed`.
    async fn on_completed_finalize_error(
        &self,
        err: FinalizeError,
        completed: Terminal,
        provider: &str,
        model: &str,
    ) {
        match err {
            FinalizeError::Transaction(err) => {
                warn!(turn_id = %self.facts.turn_id, error = %err, "finalization of a completed turn failed; downgrading to message_persistence_failed");
                self.downgrade_to_persistence_failure(completed, provider, model)
                    .await;
            }
            FinalizeError::Settlement(err) => {
                error!(turn_id = %self.facts.turn_id, error = %err, "the completed turn cannot be settled; the orphan watchdog will finalize it");
                self.emit(StreamEvent::Error {
                    code: "finalization_failed".to_owned(),
                    message: FINALIZATION_FAILED_MESSAGE.to_owned(),
                })
                .await;
            }
        }
    }

    /// The terminal events of a committed completed turn: citations, then `done`.
    async fn deliver_completed(
        &self,
        usage: Option<TokenUsage>,
        completion: &Completion,
        citations: Vec<RawCitation>,
        provider: &str,
        model: &str,
    ) {
        self.deps.metrics.stream_completed(provider, model);
        if let Completion::Incomplete { reason } = completion {
            self.deps.metrics.stream_incomplete(provider, model, reason);
        } else {
            let items = self.map_citations(citations);
            if !items.is_empty() {
                self.emit(StreamEvent::Citations(items)).await;
            }
        }
        let done = self.done_payload(usage).await;
        self.emit(StreamEvent::Done(done)).await;
    }

    /// The completed finalization failed: finalize the turn as `failed` with
    /// `message_persistence_failed`; when even that fails the client gets `finalization_failed`
    /// and the orphan watchdog finalizes the turn.
    async fn downgrade_to_persistence_failure(
        &self,
        completed: Terminal,
        provider: &str,
        model: &str,
    ) {
        let mut failed = completed;
        failed.state = TurnState::Failed;
        failed.error_code = Some("message_persistence_failed".to_owned());
        failed.error_detail = Some("the assistant message could not be persisted".to_owned());
        failed.persist_message = false;
        self.deps
            .metrics
            .stream_failed(provider, model, "message_persistence_failed");
        match self.finalize_with_retry(&failed).await {
            Ok(FinalizeOutcome::Committed) => {
                self.emit(StreamEvent::Error {
                    code: "message_persistence_failed".to_owned(),
                    message: PERSISTENCE_FAILED_MESSAGE.to_owned(),
                })
                .await;
            }
            Ok(FinalizeOutcome::Lost) => {}
            Err(err) => {
                error!(turn_id = %self.facts.turn_id, error = %err, "the turn could not be finalized; the orphan watchdog will finalize it");
                self.emit(StreamEvent::Error {
                    code: "finalization_failed".to_owned(),
                    message: FINALIZATION_FAILED_MESSAGE.to_owned(),
                })
                .await;
            }
        }
    }

    async fn conclude_failed(&self, failed: Terminal, code: &str, message: String) {
        match self.finalize_with_retry(&failed).await {
            Ok(FinalizeOutcome::Committed) => {}
            Ok(FinalizeOutcome::Lost) => {
                debug!(turn_id = %self.facts.turn_id, "the turn was finalized by another path");
                return;
            }
            // A failed finalization still reports the original error code.
            Err(err) => {
                error!(turn_id = %self.facts.turn_id, error = %err, "finalization of a failed turn failed");
            }
        }
        self.emit(StreamEvent::Error {
            code: code.to_owned(),
            message,
        })
        .await;
    }

    /// Finalizes a cancelled turn. The partial assistant message is best effort: when persisting
    /// it fails the turn is finalized as cancelled without it.
    async fn finalize_cancelled(
        &self,
        cancelled: Terminal,
    ) -> Result<FinalizeOutcome, FinalizeError> {
        let result = self.finalize_with_retry(&cancelled).await;
        if matches!(result, Err(FinalizeError::Transaction(_))) && cancelled.persist_message {
            warn!(turn_id = %self.facts.turn_id, "the partial assistant message could not be persisted");
            let mut without_message = cancelled;
            without_message.persist_message = false;
            return self.finalize_with_retry(&without_message).await;
        }
        result
    }

    /// Finalizes the turn, retrying a failed transaction up to [`FINALIZE_ATTEMPTS`] times with a
    /// doubling pause. A settlement refusal is deterministic and is not retried.
    async fn finalize_with_retry(
        &self,
        terminal: &Terminal,
    ) -> Result<FinalizeOutcome, FinalizeError> {
        let mut attempt = 1;
        loop {
            match self
                .deps
                .finalizer
                .finalize(&self.scope, &self.facts, terminal)
                .await
            {
                Err(FinalizeError::Transaction(err)) if attempt < FINALIZE_ATTEMPTS => {
                    warn!(turn_id = %self.facts.turn_id, attempt, error = %err, "the finalize transaction failed; retrying");
                    tokio::time::sleep(FINALIZE_BACKOFF * (1 << (attempt - 1))).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    async fn conclude_cancelled(&self, cancelled: Terminal, content_started: bool) {
        let started = Instant::now();
        self.deps.metrics.cancel_requested("disconnect");
        self.deps.metrics.stream_disconnected(if content_started {
            "mid_stream"
        } else {
            "before_first_token"
        });
        match self.finalize_cancelled(cancelled).await {
            Ok(FinalizeOutcome::Committed) => {
                self.deps.metrics.cancel_effective("disconnect");
                self.deps
                    .metrics
                    .time_to_abort_ms("disconnect", started.elapsed().as_secs_f64() * 1000.0);
            }
            Ok(FinalizeOutcome::Lost) => {
                debug!(turn_id = %self.facts.turn_id, "the turn was finalized by another path");
            }
            Err(err) => {
                error!(turn_id = %self.facts.turn_id, error = %err, "finalization of a cancelled turn failed; the orphan watchdog will finalize it");
            }
        }
    }

    async fn done_payload(&self, usage: Option<TokenUsage>) -> DonePayload {
        let warnings = match self
            .deps
            .quota
            .warnings(
                &self.scope.tenant_and_owner(),
                self.facts.tenant_id,
                self.facts.user_id,
                &self.limits,
                self.facts.started_at,
            )
            .await
        {
            Ok(warnings) => warnings,
            Err(err) => {
                warn!(turn_id = %self.facts.turn_id, error = %err, "the quota warnings could not be computed");
                Vec::new()
            }
        };
        let usage = usage.unwrap_or_default();
        DonePayload {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            effective_model: self.facts.effective_model.clone(),
            selected_model: self.facts.selected_model.clone(),
            quota_decision: self.facts.quota_decision,
            downgrade_from: self.facts.downgrade_from.clone(),
            downgrade_reason: self.facts.downgrade_reason,
            quota_warnings: Some(warnings),
        }
    }

    /// Maps provider annotations to citation items: web sources as they are, file sources through
    /// the attachment map (unknown provider files are omitted), without duplicates.
    fn map_citations(&self, raw: Vec<RawCitation>) -> Vec<CitationItem> {
        let mut seen_files: HashSet<Uuid> = HashSet::new();
        let mut items = Vec::new();
        for citation in raw {
            match citation {
                RawCitation::Web {
                    url,
                    title,
                    snippet,
                    start,
                    end,
                } => items.push(CitationItem {
                    source: CitationSource::Web,
                    title,
                    url: Some(url),
                    attachment_id: None,
                    snippet,
                    span: start.zip(end),
                }),
                RawCitation::File {
                    provider_file_id, ..
                } => {
                    let Some((attachment_id, filename)) = self.file_map.get(&provider_file_id)
                    else {
                        continue;
                    };
                    if seen_files.insert(*attachment_id) {
                        items.push(CitationItem {
                            source: CitationSource::File,
                            title: filename.clone(),
                            url: None,
                            attachment_id: Some(*attachment_id),
                            snippet: String::new(),
                            span: None,
                        });
                    }
                }
            }
        }
        items
    }
}

fn log_repeated_panic(turn_id: Uuid, payload: &(dyn std::any::Any + Send)) {
    error!(
        turn_id = %turn_id,
        panic = %panic_message(payload),
        "finalization panicked again; the orphan watchdog will finalize the turn"
    );
}

/// The message of a panic payload, for the log.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

fn provider_failure(err: &ProviderError, usage: Option<TokenUsage>) -> Outcome {
    Outcome::Failed {
        code: err.kind.code(),
        message: err.message.clone(),
        usage,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "latencies are far below 2^52 milliseconds"
)]
fn millis_f64(millis: u64) -> f64 {
    millis as f64
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
