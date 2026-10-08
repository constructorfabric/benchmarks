//! Provider task and SSE relay (DESIGN §3.3 SSE contract, §5.7 terminal event gating).
//!
//! Provider events are forwarded one by one through a bounded channel (no buffering). A dropped
//! receiver (client disconnect) cancels the provider stream and finalizes the turn as `cancelled`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;

use crate::clock;
use crate::domain::sanitize::sanitize_provider_message;
use crate::domain::services::AppServices;
use crate::domain::stream::events::{
    Citation, CitationSource, CitationsData, DeltaData, DeltaKind, DoneData, StreamEvent, StreamStartedData, TextSpan,
    ThreadSummaryInfo, ToolData, ToolPhase, Usage,
};
use crate::domain::stream::finalize::{self, FinalizeContext, FinalizeResult, Terminal};
use crate::domain::stream::queries;
use crate::domain::stream::setup::LiveTurn;
use crate::infra::llm::responses::{self, Annotation, ProviderEvent, ProviderFailure, StreamParser};
use crate::infra::llm::transport::StreamOutcome;
use crate::infra::metrics;

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Spawns the provider task and returns the receiving end of the SSE event channel.
#[must_use]
pub fn spawn(app: Arc<AppServices>, live: LiveTurn) -> mpsc::Receiver<StreamEvent> {
    let cap = usize::from(app.cfg.streaming.sse_channel_capacity);
    let (tx, rx) = mpsc::channel(cap);
    tokio::spawn(async move {
        run(app, live, tx).await;
    });
    rx
}

struct TurnState {
    text: String,
    content_started: bool,
    web_search_started: u32,
    code_interpreter_started: u32,
    web_search_completed: u32,
    code_interpreter_completed: u32,
    file_search_completed: u32,
    annotations: Vec<Annotation>,
    last_progress: tokio::time::Instant,
}

enum Flow {
    Continue,
    Terminal(Terminal),
    Disconnected,
}

fn fctx(live: &LiveTurn, st: &TurnState) -> FinalizeContext {
    FinalizeContext {
        tenant_id: live.ctx.subject_tenant_id(),
        user_id: live.ctx.subject_id(),
        chat_id: live.chat.id,
        turn_id: live.turn_id,
        request_id: live.request_id,
        message_id: live.message_id,
        selected_model: live.chat.model.clone(),
        effective_model: live.decision.effective_model.id.clone(),
        quota_decision: live.decision.decision.as_str().to_owned(),
        downgrade_reason: live.decision.downgrade_reason.clone(),
        periods: live.decision.periods,
        limits: live.decision.limits.clone(),
        text: st.text.clone(),
        web_search_completed: st.web_search_completed,
        code_interpreter_completed: st.code_interpreter_completed,
        file_search_completed: st.file_search_completed,
        started: live.started,
        summary_trigger: Some(live.summary_trigger),
    }
}

fn failure_terminal(f: &ProviderFailure) -> Terminal {
    Terminal::Failed { code: f.code.clone(), detail: f.message.clone(), usage: None }
}

fn labels(live: &LiveTurn) -> Vec<(&'static str, String)> {
    vec![("provider", live.provider.provider_id.clone()), ("model", live.decision.effective_model.id.clone())]
}

#[allow(clippy::too_many_lines)]
async fn run(app: Arc<AppServices>, live: LiveTurn, tx: mpsc::Sender<StreamEvent>) {
    metrics::incr("mini_chat_stream_started", 1, &labels(&live));
    let started_evt = StreamEvent::StreamStarted(StreamStartedData {
        request_id: live.request_id,
        message_id: live.message_id,
        is_new_turn: true,
        thread_summary_applied: live
            .summary_applied
            .map(|t| ThreadSummaryInfo { token_estimate: u32::try_from(t.max(0)).unwrap_or(u32::MAX) }),
    });
    let mut st = TurnState {
        text: String::new(),
        content_started: false,
        web_search_started: 0,
        code_interpreter_started: 0,
        web_search_completed: 0,
        code_interpreter_completed: 0,
        file_search_completed: 0,
        annotations: Vec::new(),
        last_progress: tokio::time::Instant::now(),
    };
    if tx.send(started_evt).await.is_err() {
        finish(&app, &live, &st, Terminal::Cancelled, &tx).await;
        return;
    }

    let uri = live.provider.chat_uri(&live.decision.effective_model.provider_model_id);
    // A provider whose OAGW upstream is still pending (e.g. its secret just became readable):
    // retry provisioning now instead of waiting for the background backoff.
    app.provisioning.wait_ready(&live.provider.alias, Duration::from_secs(5)).await;
    let ping_every = Duration::from_secs(u64::from(app.cfg.streaming.sse_ping_interval_seconds));

    // Open the provider stream (racing the client disconnect).
    let outcome = tokio::select! {
        biased;
        () = tx.closed() => {
            finish(&app, &live, &st, Terminal::Cancelled, &tx).await;
            return;
        }
        r = app.transport.stream(&live.ctx, &uri, live.request_body.clone()) => r,
    };
    let mut events = match outcome {
        Err(e) => {
            let f = responses::map_transport_error(&e);
            finish(&app, &live, &st, failure_terminal(&f), &tx).await;
            return;
        }
        Ok(StreamOutcome::Http(resp)) => {
            let f = responses::map_http_error(&resp);
            finish(&app, &live, &st, failure_terminal(&f), &tx).await;
            return;
        }
        Ok(StreamOutcome::Events(s)) => s,
    };

    let mut parser = StreamParser::new(live.provider.kind);
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
    let terminal = loop {
        tokio::select! {
            biased;
            () = tx.closed() => break None,
            _ = ping.tick(), if !st.content_started => {
                if tx.send(StreamEvent::Ping).await.is_err() {
                    break None;
                }
            }
            next = events.next() => {
                let raw = match next {
                    None => {
                        let mut flow = Flow::Continue;
                        for pe in parser.finish() {
                            flow = handle_event(&app, &live, &mut st, pe, &tx).await;
                            if !matches!(flow, Flow::Continue) {
                                break;
                            }
                        }
                        match flow {
                            Flow::Terminal(t) => break Some(t),
                            Flow::Disconnected => break None,
                            Flow::Continue => break Some(Terminal::Failed {
                                code: "provider_error".to_owned(),
                                detail: "provider stream ended without a terminal event".to_owned(),
                                usage: None,
                            }),
                        }
                    }
                    Some(Err(e)) => break Some(failure_terminal(&responses::map_transport_error(&e))),
                    Some(Ok(raw)) => raw,
                };
                let mut flow = Flow::Continue;
                for pe in parser.on_event(&raw) {
                    flow = handle_event(&app, &live, &mut st, pe, &tx).await;
                    if !matches!(flow, Flow::Continue) {
                        break;
                    }
                }
                match flow {
                    Flow::Continue => {}
                    Flow::Disconnected => break None,
                    Flow::Terminal(t) => break Some(t),
                }
            }
        }
    };
    // Dropping the provider stream aborts the upstream request.
    drop(events);
    match terminal {
        Some(t) => finish(&app, &live, &st, t, &tx).await,
        None => {
            metrics::incr("mini_chat_cancel_requested", 1, &[("trigger", "disconnect".to_owned())]);
            finish(&app, &live, &st, Terminal::Cancelled, &tx).await;
        }
    }
}

async fn progress(app: &AppServices, live: &LiveTurn, st: &mut TurnState, force: bool) {
    if !force && st.last_progress.elapsed() < PROGRESS_INTERVAL {
        return;
    }
    st.last_progress = tokio::time::Instant::now();
    let counts = (
        i32::try_from(st.web_search_completed).unwrap_or(i32::MAX),
        i32::try_from(st.code_interpreter_completed).unwrap_or(i32::MAX),
        i32::try_from(st.file_search_completed).unwrap_or(i32::MAX),
    );
    if let Ok(conn) = app.db.conn()
        && let Err(e) = queries::update_progress(&conn, live.ctx.subject_tenant_id(), live.turn_id, counts, clock::now()).await
    {
        tracing::warn!(error = %e, turn_id = %live.turn_id, "failed to refresh turn progress");
    }
}

async fn handle_event(
    app: &AppServices,
    live: &LiveTurn,
    st: &mut TurnState,
    ev: ProviderEvent,
    tx: &mpsc::Sender<StreamEvent>,
) -> Flow {
    match ev {
        ProviderEvent::TextDelta(text) => {
            st.text.push_str(&text);
            st.content_started = true;
            progress(app, live, st, false).await;
            if tx.send(StreamEvent::Delta(DeltaData { kind: DeltaKind::Text, content: text })).await.is_err() {
                return Flow::Disconnected;
            }
        }
        ProviderEvent::ReasoningDelta(text) => {
            st.content_started = true;
            if tx.send(StreamEvent::Delta(DeltaData { kind: DeltaKind::Reasoning, content: text })).await.is_err() {
                return Flow::Disconnected;
            }
        }
        ProviderEvent::ToolStart { name, details } => {
            st.content_started = true;
            if name == "web_search" {
                st.web_search_started += 1;
                if st.web_search_started > app.cfg.quota.web_search_max_calls_per_message {
                    return Flow::Terminal(Terminal::Failed {
                        code: "web_search_calls_exceeded".to_owned(),
                        detail: "web search call limit per message exceeded".to_owned(),
                        usage: None,
                    });
                }
            } else if name == "code_interpreter" {
                st.code_interpreter_started += 1;
                if st.code_interpreter_started > app.cfg.quota.code_interpreter_max_calls_per_message {
                    return Flow::Terminal(Terminal::Failed {
                        code: "code_interpreter_calls_exceeded".to_owned(),
                        detail: "code interpreter call limit per message exceeded".to_owned(),
                        usage: None,
                    });
                }
            }
            progress(app, live, st, false).await;
            if tx.send(StreamEvent::Tool(ToolData { phase: ToolPhase::Start, name, details })).await.is_err() {
                return Flow::Disconnected;
            }
        }
        ProviderEvent::ToolDone { name, details } => {
            match name.as_str() {
                "web_search" => st.web_search_completed += 1,
                "code_interpreter" => st.code_interpreter_completed += 1,
                "file_search" => st.file_search_completed += 1,
                _ => {}
            }
            progress(app, live, st, true).await;
            if tx.send(StreamEvent::Tool(ToolData { phase: ToolPhase::Done, name, details })).await.is_err() {
                return Flow::Disconnected;
            }
        }
        ProviderEvent::Annotation(a) => st.annotations.push(a),
        ProviderEvent::Completed { usage, response_id, incomplete_reason } => {
            if let Some(reason) = &incomplete_reason {
                tracing::warn!(turn_id = %live.turn_id, %reason, "stream incomplete");
                metrics::incr("mini_chat_stream_incomplete", 1, &[("reason", reason.clone())]);
            }
            return Flow::Terminal(Terminal::Completed { usage, response_id, incomplete_reason });
        }
        ProviderEvent::Failed { code, message, usage } => {
            return Flow::Terminal(Terminal::Failed { code, detail: message, usage });
        }
    }
    Flow::Continue
}

fn map_citations(live: &LiveTurn, st: &TurnState) -> Vec<Citation> {
    let mut out = Vec::new();
    for a in &st.annotations {
        match a {
            Annotation::Url { url, title, snippet, start, end } => out.push(Citation {
                source: CitationSource::Web,
                title: title.clone(),
                url: Some(url.clone()),
                attachment_id: None,
                snippet: snippet.clone(),
                span: match (start, end) {
                    (Some(s), Some(e)) => Some(TextSpan { start: *s, end: *e }),
                    _ => None,
                },
                score: None,
            }),
            Annotation::File { file_id, .. } => {
                if let Some((attachment_id, filename)) = live.file_map.get(file_id) {
                    out.push(Citation {
                        source: CitationSource::File,
                        title: filename.clone(),
                        url: None,
                        attachment_id: Some(*attachment_id),
                        snippet: String::new(),
                        span: None,
                        score: None,
                    });
                }
            }
        }
    }
    out
}

/// Finalizes the turn and emits the gated terminal event (nothing after a disconnect).
async fn finish(app: &Arc<AppServices>, live: &LiveTurn, st: &TurnState, terminal: Terminal, tx: &mpsc::Sender<StreamEvent>) {
    let result = finalize::finalize(app, &fctx(live, st), &terminal).await;
    let disconnected = matches!(terminal, Terminal::Cancelled);
    if disconnected {
        if matches!(result, FinalizeResult::Committed { .. }) {
            metrics::incr("mini_chat_cancel_effective", 1, &[("trigger", "disconnect".to_owned())]);
            metrics::incr("mini_chat_streams_aborted", 1, &[("trigger", "client_disconnect".to_owned())]);
        }
        return;
    }
    let event = match (&terminal, result) {
        (_, FinalizeResult::CasLost) => {
            tracing::warn!(turn_id = %live.turn_id, "finalization CAS lost; ending stream without terminal event");
            return;
        }
        (Terminal::Completed { usage, .. }, FinalizeResult::Committed { warnings }) => {
            metrics::incr("mini_chat_stream_completed", 1, &labels(live));
            let citations = map_citations(live, st);
            if !citations.is_empty() && tx.send(StreamEvent::Citations(CitationsData { items: citations })).await.is_err() {
                return;
            }
            let u = usage.unwrap_or_default();
            let downgrade = live.decision.decision.as_str() == "downgrade";
            StreamEvent::Done(DoneData {
                usage: Usage { input_tokens: u.input_tokens, output_tokens: u.output_tokens },
                effective_model: live.decision.effective_model.id.clone(),
                selected_model: live.chat.model.clone(),
                quota_decision: live.decision.decision.as_str().to_owned(),
                downgrade_from: downgrade.then(|| live.chat.model.clone()),
                downgrade_reason: if downgrade { live.decision.downgrade_reason.clone() } else { None },
                quota_warnings: Some(warnings),
            })
        }
        (Terminal::Completed { .. }, FinalizeResult::MessagePersistenceFailed) => {
            StreamEvent::error("message_persistence_failed", "The response could not be saved")
        }
        (Terminal::Completed { .. }, FinalizeResult::Failed(_)) => {
            StreamEvent::error("finalization_failed", "The response could not be finalized")
        }
        (Terminal::Failed { code, detail, .. }, _) => {
            metrics::incr("mini_chat_stream_failed", 1, &[("error_code", code.clone())]);
            StreamEvent::error(code, sanitize_provider_message(&user_message(code, detail)))
        }
        (Terminal::Cancelled, _) => return,
    };
    let _ = tx.send(event).await;
}

fn user_message(code: &str, detail: &str) -> String {
    match code {
        "provider_timeout" => "The model provider timed out".to_owned(),
        "web_search_calls_exceeded" => "Web search call limit per message exceeded".to_owned(),
        "code_interpreter_calls_exceeded" => "Code interpreter call limit per message exceeded".to_owned(),
        _ if detail.trim().is_empty() => "The model provider returned an error".to_owned(),
        _ => detail.to_owned(),
    }
}

/// Synthesized terminal event when the provider task ended without one (ADR-0010).
#[must_use]
pub fn stream_interrupted() -> StreamEvent {
    StreamEvent::error("stream_interrupted", "The stream ended unexpectedly")
}
