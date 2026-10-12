//! The provider task of a turn: calls the provider, relays its events through
//! the bounded channel without buffering, and concludes the turn (DESIGN 3.6
//! "Streaming Cancellation", 5.7 "Terminal SSE Event Emission Guard").
//!
//! The task owns the sending half of the channel. The client disconnect is the
//! closed channel, observed either through `closed()` (idle or waiting on the
//! provider) or as a failed `send` (blocked on a full channel); both cancel
//! the turn, drop the provider stream (the hard cancel) and finalize
//! `cancelled` without attempting an `error` event.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::Instant;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::finalize::{Finalization, Outcome};
use super::preflight::FileTarget;
use super::{
    Citation, CitationSource, DoneEvent, QuotaDecision, STREAM_INTERRUPTED, StreamEvent,
    StreamService, ToolPhase,
};
use crate::domain::clock::now_utc;
use crate::domain::messages::{MessageRole, NewMessage};
use crate::domain::ports::TurnRepository;
use crate::domain::provider::{
    Annotation, ChatRequest, Completion, ProviderErrorKind, ProviderEvent, ProviderFailure,
    TokenBreakdown,
};
use crate::domain::quota::{DowngradeReason, QuotaWarning};
use crate::domain::sanitize::client_message;
use crate::domain::settlement::TurnLedger;
use crate::domain::thread_summary::SummaryTrigger;
use crate::domain::turns::ToolKind;

/// `chat_turns.error_code` of a turn that started more `web_search` calls than
/// `quota.web_search_max_calls_per_message` allows.
const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";

/// `chat_turns.error_code` of a turn that started more `code_interpreter` calls
/// than `quota.code_interpreter_max_calls_per_message` allows.
const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";

/// `chat_turns.last_progress_at` is refreshed at most this often while text
/// or tool events keep arriving (DESIGN 3.7 `last_progress_at`).
const PROGRESS_REFRESH: Duration = Duration::from_secs(30);

/// Longest `provider_response_id` the column holds.
const RESPONSE_ID_MAX_CHARS: usize = 128;

/// Everything the provider task needs; owned by the task.
pub(super) struct TurnRun {
    pub(super) ctx: SecurityContext,
    /// Tenant scope for the turn's own rows.
    pub(super) scope: AccessScope,
    /// Tenant + owner scope for the caller's `quota_usage` rows.
    pub(super) quota_scope: AccessScope,
    pub(super) chat_id: Uuid,
    pub(super) turn_id: Uuid,
    pub(super) request_id: Uuid,
    pub(super) assistant_message_id: Uuid,
    pub(super) user_created_at: OffsetDateTime,
    pub(super) selected_model: String,
    pub(super) effective_model: String,
    /// Why the turn runs on `effective_model` rather than the selected one.
    pub(super) downgrade_reason: Option<DowngradeReason>,
    pub(super) request: ChatRequest,
    pub(super) files: Vec<FileTarget>,
    /// What the settlement needs, fixed at preflight.
    pub(super) ledger: TurnLedger,
    /// `token_estimate` of the thread summary in the context, when it is.
    pub(super) summary_applied: Option<u64>,
    /// What the thread-summary trigger needs at finalization.
    pub(super) trigger: SummaryTrigger,
    /// Start of the provider task (audit `latency_ms`).
    pub(super) started: Instant,
}

/// How the provider part of the turn ended.
enum Terminal {
    Completed(Completion),
    Failed {
        code: &'static str,
        message: String,
        /// Usage the provider reported with the failure, when any.
        usage: Option<(u64, u64)>,
        /// Cache / reasoning breakdown of that usage.
        breakdown: TokenBreakdown,
    },
    Cancelled,
}

impl Terminal {
    fn provider_failure(failure: &ProviderFailure) -> Self {
        Self::Failed {
            code: failure.kind.code(),
            message: client_message(&failure.message),
            usage: None,
            breakdown: TokenBreakdown::default(),
        }
    }

    /// A per-message tool limit was breached: the stream is cancelled before
    /// usage arrives, so the settlement is always estimated.
    fn tool_limit(code: &'static str, message: &str) -> Self {
        Self::Failed {
            code,
            message: message.to_owned(),
            usage: None,
            breakdown: TokenBreakdown::default(),
        }
    }

    fn provider_error(message: &str) -> Self {
        Self::provider_failure(&ProviderFailure::error(message))
    }
}

/// State of the relay loop.
struct Relay {
    text: String,
    content_started: bool,
    last_progress: Instant,
    /// `web_search` calls started in this turn.
    web_search_started: u32,
    /// `code_interpreter` calls started in this turn.
    code_interpreter_started: u32,
}

impl StreamService {
    pub(super) async fn run_turn(self: Arc<Self>, run: TurnRun, sender: mpsc::Sender<StreamEvent>) {
        let mut relay = Relay {
            text: String::new(),
            content_started: false,
            last_progress: Instant::now(),
            web_search_started: 0,
            code_interpreter_started: 0,
        };
        let started = StreamEvent::Started {
            request_id: run.request_id,
            message_id: run.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: run.summary_applied,
        };
        let terminal = if sender.send(started).await.is_err() {
            Terminal::Cancelled
        } else {
            self.relay(&run, &sender, &mut relay).await
        };
        self.conclude(&run, &sender, relay.text, terminal).await;
    }

    /// Calls the provider and relays its events until a terminal outcome. The
    /// provider stream is dropped when this returns, before finalization.
    async fn relay(
        &self,
        run: &TurnRun,
        sender: &mpsc::Sender<StreamEvent>,
        relay: &mut Relay,
    ) -> Terminal {
        let ping_every =
            Duration::from_secs(u64::from(self.config.streaming.sse_ping_interval_seconds));
        let mut idle_since = Instant::now();

        // The provider may keep the answer back before its first byte: the
        // pings of the contract start as soon as `stream_started` is out.
        let call = self.llm.stream_chat(&run.ctx, &run.request);
        tokio::pin!(call);
        let mut stream = loop {
            tokio::select! {
                biased;
                () = sender.closed() => return Terminal::Cancelled,
                () = tokio::time::sleep_until(idle_since + ping_every) => {
                    if sender.send(StreamEvent::Ping).await.is_err() {
                        return Terminal::Cancelled;
                    }
                    idle_since = Instant::now();
                }
                started = &mut call => match started {
                    Ok(stream) => break stream,
                    Err(failure) => return Terminal::provider_failure(&failure),
                },
            }
        };

        loop {
            let item = tokio::select! {
                biased;
                () = sender.closed() => return Terminal::Cancelled,
                () = tokio::time::sleep_until(idle_since + ping_every), if !relay.content_started => {
                    if sender.send(StreamEvent::Ping).await.is_err() {
                        return Terminal::Cancelled;
                    }
                    idle_since = Instant::now();
                    continue;
                }
                item = stream.next() => item,
            };
            let event = match item {
                None => {
                    return Terminal::provider_error("the provider stream ended unexpectedly");
                }
                Some(Err(failure)) => return Terminal::provider_failure(&failure),
                Some(Ok(event)) => event,
            };
            if let Some(terminal) = self.on_provider_event(run, sender, relay, event).await {
                return terminal;
            }
            idle_since = Instant::now();
        }
    }

    /// Translates one provider event; `Some` ends the relay loop.
    async fn on_provider_event(
        &self,
        run: &TurnRun,
        sender: &mpsc::Sender<StreamEvent>,
        relay: &mut Relay,
        event: ProviderEvent,
    ) -> Option<Terminal> {
        match event {
            ProviderEvent::TextDelta(text) => {
                if text.is_empty() {
                    return None;
                }
                relay.text.push_str(&text);
                relay.content_started = true;
                self.refresh_progress(run, relay).await;
                send_or_cancel(sender, StreamEvent::Delta(text)).await
            }
            ProviderEvent::ToolStarted { name } => {
                if let Some(breach) = self.tool_limit_breach(relay, &name) {
                    return Some(breach);
                }
                relay.content_started = true;
                self.refresh_progress(run, relay).await;
                send_or_cancel(
                    sender,
                    StreamEvent::Tool {
                        phase: ToolPhase::Start,
                        name,
                        details: serde_json::json!({}),
                    },
                )
                .await
            }
            ProviderEvent::ToolDone { name, details } => {
                relay.content_started = true;
                if let Some(tool) = ToolKind::from_name(&name) {
                    relay.last_progress = Instant::now();
                    self.record_tool_done(run, tool).await;
                }
                send_or_cancel(
                    sender,
                    StreamEvent::Tool {
                        phase: ToolPhase::Done,
                        name,
                        details,
                    },
                )
                .await
            }
            ProviderEvent::Completed(completion) => Some(Terminal::Completed(completion)),
            ProviderEvent::Failed(failure) => Some(Terminal::Failed {
                code: ProviderErrorKind::Error.code(),
                message: if failure.message.trim().is_empty() {
                    "the provider reported an error".to_owned()
                } else {
                    client_message(&failure.message)
                },
                usage: Some((failure.input_tokens, failure.output_tokens)),
                breakdown: failure.breakdown,
            }),
            ProviderEvent::Ignored => None,
        }
    }

    /// Counts a tool start and returns the failure when it exceeds the
    /// per-message limit of its tool (DESIGN 4 "Web Search Quota Enforcement",
    /// "Code Interpreter Quota Enforcement"): the start that makes the count
    /// exceed the limit ends the relay, which drops the provider stream (the
    /// hard cancel), and the turn is finalized `failed`.
    fn tool_limit_breach(&self, relay: &mut Relay, name: &str) -> Option<Terminal> {
        let quota = &self.config.quota;
        match ToolKind::from_name(name)? {
            ToolKind::WebSearch => {
                relay.web_search_started += 1;
                (relay.web_search_started > quota.web_search_max_calls_per_message).then(|| {
                    Terminal::tool_limit(
                        WEB_SEARCH_CALLS_EXCEEDED,
                        "the turn exceeded the number of web searches allowed per message",
                    )
                })
            }
            ToolKind::CodeInterpreter => {
                relay.code_interpreter_started += 1;
                (relay.code_interpreter_started > quota.code_interpreter_max_calls_per_message)
                    .then(|| {
                        Terminal::tool_limit(
                            CODE_INTERPRETER_CALLS_EXCEEDED,
                            "the turn exceeded the number of code interpreter runs allowed per message",
                        )
                    })
            }
            ToolKind::FileSearch => None,
        }
    }

    async fn refresh_progress(&self, run: &TurnRun, relay: &mut Relay) {
        if relay.last_progress.elapsed() < PROGRESS_REFRESH {
            return;
        }
        relay.last_progress = Instant::now();
        let touched = match self.db.conn() {
            Ok(conn) => {
                conn.touch_turn_progress(&run.scope, run.turn_id, now_utc())
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = touched {
            tracing::warn!(turn_id = %run.turn_id, %error, "turn progress was not recorded");
        }
    }

    async fn record_tool_done(&self, run: &TurnRun, tool: ToolKind) {
        let counted = match self.db.conn() {
            Ok(conn) => {
                conn.count_tool_done(&run.scope, run.turn_id, tool, now_utc())
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = counted {
            tracing::warn!(turn_id = %run.turn_id, %error, "tool call was not counted");
        }
    }

    /// Finalizes the turn and, for the paths that still have a client, sends
    /// the terminal event after the finalization committed.
    async fn conclude(
        &self,
        run: &TurnRun,
        sender: &mpsc::Sender<StreamEvent>,
        text: String,
        terminal: Terminal,
    ) {
        match terminal {
            Terminal::Completed(completion) => {
                self.conclude_completed(run, sender, text, completion).await;
            }
            Terminal::Failed {
                code,
                message,
                usage,
                breakdown,
            } => {
                let finalization = self
                    .finalize(
                        run,
                        Outcome::Failed {
                            error_code: code.to_owned(),
                            usage,
                            breakdown,
                        },
                    )
                    .await;
                // A failed finalization still reports the original error; a
                // lost CAS means somebody else decided the outcome.
                let event = if matches!(finalization, Finalization::Lost) {
                    interrupted()
                } else {
                    StreamEvent::Error {
                        code: code.to_owned(),
                        message,
                    }
                };
                send_terminal(sender, event).await;
            }
            Terminal::Cancelled => {
                let message = (!text.is_empty())
                    .then(|| assistant_message(run, text, None, 0, 0, TokenBreakdown::default()));
                self.finalize(run, Outcome::Cancelled { message }).await;
            }
        }
    }

    async fn conclude_completed(
        &self,
        run: &TurnRun,
        sender: &mpsc::Sender<StreamEvent>,
        text: String,
        completion: Completion,
    ) {
        let message = assistant_message(
            run,
            text,
            completion.response_id.clone(),
            completion.input_tokens,
            completion.output_tokens,
            completion.breakdown,
        );
        let usage = (completion.input_tokens, completion.output_tokens);
        let finalization = self
            .finalize(
                run,
                Outcome::Completed {
                    message,
                    usage,
                    breakdown: completion.breakdown,
                },
            )
            .await;
        match finalization {
            Finalization::Won { warnings } => {
                if !completion.incomplete {
                    let items = citations(&completion.annotations, &run.files);
                    if !items.is_empty()
                        && sender.send(StreamEvent::Citations(items)).await.is_err()
                    {
                        return;
                    }
                }
                send_terminal(
                    sender,
                    StreamEvent::Done(done_event(run, &completion, warnings)),
                )
                .await;
            }
            Finalization::Lost => send_terminal(sender, interrupted()).await,
            Finalization::MessageNotPersisted => {
                send_terminal(
                    sender,
                    error_event(
                        "message_persistence_failed",
                        "the answer could not be stored",
                    ),
                )
                .await;
            }
            Finalization::Unavailable => {
                send_terminal(
                    sender,
                    error_event("finalization_failed", "the turn could not be finalized"),
                )
                .await;
            }
        }
    }
}

/// `Some(Cancelled)` when the receiver is gone (the disconnect seen as a
/// failed send under backpressure).
async fn send_or_cancel(
    sender: &mpsc::Sender<StreamEvent>,
    event: StreamEvent,
) -> Option<Terminal> {
    sender
        .send(event)
        .await
        .is_err()
        .then_some(Terminal::Cancelled)
}

/// Sends the terminal event; a disconnect after finalization is a no-op.
async fn send_terminal(sender: &mpsc::Sender<StreamEvent>, event: StreamEvent) {
    if sender.send(event).await.is_err() {
        tracing::debug!("client disconnected before the terminal event");
    }
}

fn error_event(code: &str, message: &str) -> StreamEvent {
    StreamEvent::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn interrupted() -> StreamEvent {
    error_event(
        STREAM_INTERRUPTED,
        "the turn was finalized by another process",
    )
}

fn done_event(run: &TurnRun, completion: &Completion, warnings: Vec<QuotaWarning>) -> DoneEvent {
    let downgraded = run.effective_model != run.selected_model || run.downgrade_reason.is_some();
    DoneEvent {
        input_tokens: completion.input_tokens,
        output_tokens: completion.output_tokens,
        effective_model: run.effective_model.clone(),
        selected_model: run.selected_model.clone(),
        quota_decision: if downgraded {
            QuotaDecision::Downgrade
        } else {
            QuotaDecision::Allow
        },
        downgrade_from: downgraded.then(|| run.selected_model.clone()),
        downgrade_reason: run
            .downgrade_reason
            .map(|reason| reason.as_str().to_owned()),
        quota_warnings: Some(warnings),
    }
}

/// The assistant message of the turn; strictly newer than the user message.
fn assistant_message(
    run: &TurnRun,
    content: String,
    provider_response_id: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    breakdown: TokenBreakdown,
) -> NewMessage {
    let newest = now_utc().max(run.user_created_at + time::Duration::microseconds(1));
    NewMessage {
        id: run.assistant_message_id,
        tenant_id: run.request.tenant_id,
        chat_id: run.chat_id,
        request_id: run.request_id,
        role: MessageRole::Assistant,
        content,
        model: Some(run.effective_model.clone()),
        provider_response_id: provider_response_id
            .map(|id| id.chars().take(RESPONSE_ID_MAX_CHARS).collect()),
        input_tokens: i64::try_from(input_tokens).unwrap_or(i64::MAX),
        output_tokens: i64::try_from(output_tokens).unwrap_or(i64::MAX),
        breakdown,
        created_at: newest,
    }
}

/// Maps the provider annotations to client citations: web annotations as they
/// are, file annotations only when the provider file belongs to a live ready
/// document of this chat (the provider file id never leaves the gear).
fn citations(annotations: &[Annotation], files: &[FileTarget]) -> Vec<Citation> {
    annotations
        .iter()
        .filter_map(|annotation| match annotation {
            Annotation::Web {
                url,
                title,
                snippet,
                span,
            } => Some(Citation {
                source: CitationSource::Web,
                title: title.clone(),
                url: Some(url.clone()),
                attachment_id: None,
                snippet: snippet.clone(),
                span: *span,
            }),
            Annotation::File { file_id, .. } => files
                .iter()
                .find(|file| file.provider_file_id == *file_id)
                .map(|file| Citation {
                    source: CitationSource::File,
                    title: file.filename.clone(),
                    url: None,
                    attachment_id: Some(file.attachment_id),
                    snippet: String::new(),
                    span: None,
                }),
        })
        .collect()
}
