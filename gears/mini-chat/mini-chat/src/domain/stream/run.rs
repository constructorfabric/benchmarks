//! Provider task: streams the provider response into the bounded client channel,
//! enforces per-turn tool limits, refreshes turn progress and finalizes the turn.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use oagw_sdk::{Body, ServerEvent, ServerEventsResponse, ServerEventsStream};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::TurnRuntime;
use super::citations::map_citations;
use super::events::{Citations, Delta, Done, QuotaWarningOut, StreamEvent, Tool, UsageOut};
use super::finalize::{FinalizeResult, Outcome, ToolCounters};
use crate::domain::app::AppServices;
use crate::domain::knowledge::{self, KnowledgeParams};
use crate::domain::quota::{self, QuotaDecision};
use crate::domain::sanitize::sanitize_provider_message;
use crate::domain::time::now;
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repo;
use crate::infra::llm::ProviderCallError;
use crate::infra::llm::responses::{ProviderEvent, ResponsesParser, build_body};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

enum Exit {
    /// Provider terminal outcome or failure: finalize and send the terminal event.
    Terminal(Outcome),
    /// Client disconnected: finalize as cancelled, send nothing.
    Cancelled,
    /// The provider request ended with `search_knowledge` calls: run them and continue.
    ToolUse,
}

/// A `search_knowledge` call requested by the model.
struct PendingCall {
    call_id: String,
    arguments: String,
}

struct Run<'a> {
    svc: &'a Arc<AppServices>,
    rt: &'a TurnRuntime,
    tx: &'a mpsc::Sender<StreamEvent>,
    text: String,
    counters: ToolCounters,
    web_search_starts: u32,
    code_interpreter_starts: u32,
    ttft_ms: Option<u64>,
    last_progress: Instant,
    pending_calls: Vec<PendingCall>,
}

impl Run<'_> {
    fn outcome(&self, state: &'static str, code: Option<&str>, detail: Option<String>) -> Outcome {
        Outcome {
            state,
            error_code: code.map(str::to_owned),
            error_detail: detail,
            text: self.text.clone(),
            usage: None,
            response_id: None,
            counters: self.counters,
            ttft_ms: self.ttft_ms,
        }
    }

    async fn send(&mut self, ev: StreamEvent) -> bool {
        if self.ttft_ms.is_none() && ev.is_content() {
            self.ttft_ms = u64::try_from(self.rt.started.elapsed().as_millis()).ok();
        }
        self.tx.send(ev).await.is_ok()
    }

    async fn progress(&mut self, force: bool) {
        if !force && self.last_progress.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last_progress = Instant::now();
        let Ok(conn) = self.svc.db.conn() else { return };
        let c = self.counters;
        // Best-effort heartbeat: a failed write is retried on the next progress tick.
        if let Err(e) = repo::update_turn_where(
            &conn,
            self.rt.tenant_id,
            self.rt.turn_id,
            Condition::all().add(chat_turn::Column::State.eq("running")),
            vec![
                (chat_turn::Column::LastProgressAt, Expr::value(now())),
                (
                    chat_turn::Column::WebSearchCompletedCount,
                    Expr::value(i32::try_from(c.web_search_done).unwrap_or(i32::MAX)),
                ),
                (
                    chat_turn::Column::CodeInterpreterCompletedCount,
                    Expr::value(i32::try_from(c.code_interpreter_done).unwrap_or(i32::MAX)),
                ),
                (
                    chat_turn::Column::FileSearchCompletedCount,
                    Expr::value(i32::try_from(c.file_search_done).unwrap_or(i32::MAX)),
                ),
            ],
        )
        .await
        {
            tracing::debug!(error = %e, turn_id = %self.rt.turn_id, "turn progress update failed");
        }
    }

    /// Runs one `search_knowledge` call and returns its `function_call_output`.
    async fn knowledge_output(&mut self, kp: &KnowledgeParams, arguments: &str) -> String {
        if self.counters.knowledge_calls >= kp.max_calls {
            return knowledge::limit_output();
        }
        let Some(args) = knowledge::parse_args(arguments) else {
            return knowledge::invalid_args_output();
        };
        self.counters.knowledge_calls += 1;
        let started = Instant::now();
        let res = knowledge::search(&self.svc.llm, kp, &args).await;
        #[allow(clippy::cast_precision_loss)] // latency metric
        self.svc.metrics.record(
            "knowledge_search_latency_ms",
            started.elapsed().as_secs_f64() * 1000.0,
            &[],
        );
        match res {
            Ok(chunks) => {
                self.counters.file_search_done += 1;
                self.svc
                    .metrics
                    .inc("knowledge_search", &[("result", "ok")]);
                #[allow(clippy::cast_precision_loss)] // chunk count metric
                self.svc
                    .metrics
                    .record("knowledge_search_chunks", chunks.len() as f64, &[]);
                knowledge::results_output(&chunks)
            }
            Err(e) => {
                tracing::warn!(error = %e, turn_id = %self.rt.turn_id, "knowledge search failed");
                self.svc
                    .metrics
                    .inc("knowledge_search", &[("result", "error")]);
                knowledge::failure_output()
            }
        }
    }

    /// Handles one provider event; `Some` ends the stream.
    // One arm per provider event kind; the dispatch reads best as a single match.
    #[allow(clippy::cognitive_complexity)]
    async fn on_event(&mut self, ev: ProviderEvent) -> Option<Exit> {
        match ev {
            ProviderEvent::TextDelta(d) => {
                self.text.push_str(&d);
                if !self
                    .send(StreamEvent::Delta(Delta {
                        kind: "text",
                        content: d,
                    }))
                    .await
                {
                    return Some(Exit::Cancelled);
                }
                self.progress(false).await;
                None
            }
            ProviderEvent::ToolStart { name, details } => {
                let q = &self.svc.cfg.quota;
                if name == "web_search" {
                    self.web_search_starts += 1;
                    if self.web_search_starts > q.web_search_max_calls_per_message {
                        return Some(Exit::Terminal(self.outcome(
                            "failed",
                            Some("web_search_calls_exceeded"),
                            Some("web search call limit exceeded".to_owned()),
                        )));
                    }
                }
                if name == "code_interpreter" {
                    self.code_interpreter_starts += 1;
                    if self.code_interpreter_starts > q.code_interpreter_max_calls_per_message {
                        return Some(Exit::Terminal(self.outcome(
                            "failed",
                            Some("code_interpreter_calls_exceeded"),
                            Some("code interpreter call limit exceeded".to_owned()),
                        )));
                    }
                }
                let ev = StreamEvent::Tool(Tool {
                    phase: "start",
                    name: name.to_owned(),
                    details,
                });
                if !self.send(ev).await {
                    return Some(Exit::Cancelled);
                }
                self.progress(true).await;
                None
            }
            ProviderEvent::ToolDone { name, details } => {
                match name {
                    "web_search" => self.counters.web_search_done += 1,
                    "code_interpreter" => self.counters.code_interpreter_done += 1,
                    "file_search" => self.counters.file_search_done += 1,
                    _ => {}
                }
                let ev = StreamEvent::Tool(Tool {
                    phase: "done",
                    name: name.to_owned(),
                    details,
                });
                if !self.send(ev).await {
                    return Some(Exit::Cancelled);
                }
                self.progress(true).await;
                None
            }
            ProviderEvent::FunctionCall {
                name,
                call_id,
                arguments,
            } => {
                if self.rt.knowledge.is_some() && name == knowledge::TOOL_NAME {
                    self.pending_calls.push(PendingCall { call_id, arguments });
                    return None;
                }
                Some(Exit::Terminal(self.outcome(
                    "failed",
                    Some("unexpected_tool_use"),
                    Some(format!("unexpected tool use: {name}")),
                )))
            }
            ProviderEvent::Completed(_) if !self.pending_calls.is_empty() => Some(Exit::ToolUse),
            ProviderEvent::Completed(c) => {
                if c.incomplete_reason.is_none() {
                    let items = map_citations(&c.parts, &self.rt.files);
                    if !items.is_empty()
                        && !self.send(StreamEvent::Citations(Citations { items })).await
                    {
                        return Some(Exit::Cancelled);
                    }
                }
                if let Some(reason) = &c.incomplete_reason {
                    tracing::warn!(reason = %reason, turn_id = %self.rt.turn_id, "stream incomplete");
                    self.svc.metrics.inc(
                        "stream_incomplete",
                        &[
                            ("provider", &self.rt.provider.provider_id),
                            ("model", &self.rt.decision.effective.id),
                            ("reason", reason),
                        ],
                    );
                }
                let mut o = self.outcome("completed", None, None);
                o.usage = c.usage;
                o.response_id = c.response_id;
                Some(Exit::Terminal(o))
            }
            ProviderEvent::Failed {
                code,
                message,
                usage,
            } => {
                let mut o = self.outcome(
                    "failed",
                    Some("provider_error"),
                    Some(format!("{}: {message}", code.unwrap_or_default())),
                );
                o.usage = usage;
                o.error_detail = Some(sanitize_provider_message(&message));
                Some(Exit::Terminal(o))
            }
        }
    }
}

fn error_message(code: &str, detail: Option<&str>) -> String {
    match code {
        "provider_error" => detail
            .map(sanitize_provider_message)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "Provider is currently unavailable".to_owned()),
        "web_search_calls_exceeded" => {
            "The web search call limit for this message was exceeded".to_owned()
        }
        "code_interpreter_calls_exceeded" => {
            "The code interpreter call limit for this message was exceeded".to_owned()
        }
        "unexpected_tool_use" => "The model requested a tool that is not available".to_owned(),
        "agentic_iterations_exceeded" => "The tool-use iteration limit was exceeded".to_owned(),
        "message_persistence_failed" => "The response could not be saved".to_owned(),
        _ => detail.map_or_else(
            || "The request failed".to_owned(),
            sanitize_provider_message,
        ),
    }
}

/// Runs a turn to its terminal state.
// Provider loop + finalization + terminal event emission of one turn.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub async fn run_turn(
    svc: Arc<AppServices>,
    rt: TurnRuntime,
    tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
) {
    let provider_label = rt.provider.provider_id.clone();
    let model_label = rt.decision.effective.id.clone();
    svc.metrics.inc(
        "stream_started",
        &[("provider", &provider_label), ("model", &model_label)],
    );
    svc.metrics.updown("active_streams", 1);
    let mut run = Run {
        svc: &svc,
        rt: &rt,
        tx: &tx,
        text: String::new(),
        counters: ToolCounters::default(),
        web_search_starts: 0,
        code_interpreter_starts: 0,
        ttft_ms: None,
        last_progress: Instant::now(),
        pending_calls: Vec::new(),
    };
    let mut provider_error: Option<ProviderCallError> = None;
    let mut request = rt.request.clone();
    let mut iteration: u32 = 0;

    let exit = 'run: loop {
        iteration += 1;
        let body = build_body(&request);
        let req = http::Request::builder()
            .method(http::Method::POST)
            .uri(rt.provider.chat_uri(&request.model))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "text/event-stream")
            .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()));
        let req = match req {
            Ok(r) => r,
            Err(e) => {
                break 'run Exit::Terminal(run.outcome(
                    "failed",
                    Some("provider_error"),
                    Some(e.to_string()),
                ));
            }
        };
        let resp = tokio::select! {
            () = cancel.cancelled() => break 'run Exit::Cancelled,
            () = tx.closed() => break 'run Exit::Cancelled,
            r = svc.llm.send(req) => r,
        };
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let code = e.code();
                let o = run.outcome("failed", Some(code), Some(e.to_string()));
                provider_error = Some(e);
                break 'run Exit::Terminal(o);
            }
        };
        let mut events = match ServerEventsStream::from_response::<ServerEvent>(resp) {
            ServerEventsResponse::Events(s) => s,
            ServerEventsResponse::Response(_) => {
                break 'run Exit::Terminal(run.outcome(
                    "failed",
                    Some("provider_error"),
                    Some("provider returned a non-streaming response".to_owned()),
                ));
            }
        };
        let mut parser = ResponsesParser::default();
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => break 'run Exit::Cancelled,
                () = tx.closed() => break 'run Exit::Cancelled,
                n = events.next() => n,
            };
            match next {
                None => {
                    break 'run Exit::Terminal(run.outcome(
                        "failed",
                        Some("provider_error"),
                        Some("provider stream ended without a terminal event".to_owned()),
                    ));
                }
                Some(Err(e)) => {
                    let msg = e.to_string();
                    let code = if msg.to_ascii_lowercase().contains("timeout")
                        || msg.contains("deadline")
                    {
                        "provider_timeout"
                    } else {
                        "provider_error"
                    };
                    break 'run Exit::Terminal(run.outcome("failed", Some(code), Some(msg)));
                }
                Some(Ok(ev)) => {
                    let mut tool_use = false;
                    for pe in parser.on_event(ev.event.as_deref(), &ev.data) {
                        match run.on_event(pe).await {
                            Some(Exit::ToolUse) => tool_use = true,
                            Some(exit) => break 'run exit,
                            None => {}
                        }
                    }
                    if tool_use {
                        break;
                    }
                }
            }
        }
        // Agentic loop: run the requested retrievals and issue the next request.
        let Some(kp) = rt.knowledge.as_ref() else {
            break 'run Exit::Terminal(run.outcome("failed", Some("unexpected_tool_use"), None));
        };
        if iteration >= kp.max_calls.saturating_add(2) {
            break 'run Exit::Terminal(run.outcome(
                "failed",
                Some("agentic_iterations_exceeded"),
                Some("knowledge search iteration limit exceeded".to_owned()),
            ));
        }
        let calls = std::mem::take(&mut run.pending_calls);
        for call in calls {
            let output = tokio::select! {
                () = cancel.cancelled() => break 'run Exit::Cancelled,
                () = tx.closed() => break 'run Exit::Cancelled,
                o = run.knowledge_output(kp, &call.arguments) => o,
            };
            request.extra_input.push(json!({
                "type": "function_call",
                "call_id": call.call_id,
                "name": knowledge::TOOL_NAME,
                "arguments": call.arguments,
            }));
            request.extra_input.push(json!({
                "type": "function_call_output",
                "call_id": call.call_id,
                "output": output,
            }));
        }
        run.progress(true).await;
    };
    let total_ms = rt.started.elapsed().as_secs_f64() * 1000.0;
    svc.metrics.updown("active_streams", -1);
    // `ToolUse` is consumed by the agentic loop and never leaves it.
    let exit = match exit {
        Exit::ToolUse => Exit::Terminal(run.outcome("failed", Some("unexpected_tool_use"), None)),
        other => other,
    };
    match exit {
        Exit::ToolUse => {}
        Exit::Cancelled => {
            svc.metrics
                .inc("cancel_requested", &[("trigger", "disconnect")]);
            svc.metrics.inc(
                "stream_disconnected",
                &[(
                    "stage",
                    if run.ttft_ms.is_some() {
                        "mid_stream"
                    } else {
                        "before_first_token"
                    },
                )],
            );
            let o = run.outcome("cancelled", None, None);
            if let FinalizeResult::Committed { .. } = svc.finalize_turn(&rt, o).await {
                svc.metrics
                    .inc("cancel_effective", &[("trigger", "disconnect")]);
                svc.metrics
                    .inc("streams_aborted", &[("trigger", "client_disconnect")]);
            }
        }
        Exit::Terminal(o) => {
            let original_code = o.error_code.clone();
            let detail = o.error_detail.clone();
            let completed = o.state == "completed";
            let usage = o.usage.unwrap_or_default();
            match svc.finalize_turn(&rt, o).await {
                FinalizeResult::Committed { state, error_code } => {
                    if state == "completed" {
                        svc.metrics.inc(
                            "stream_completed",
                            &[("provider", &provider_label), ("model", &model_label)],
                        );
                        svc.metrics.record(
                            "stream_total_latency_ms",
                            total_ms,
                            &[("provider", &provider_label), ("model", &model_label)],
                        );
                        let warnings = quota_warnings(&svc, &rt).await;
                        let d = &rt.decision;
                        let downgrade = d.decision == QuotaDecision::Downgrade;
                        let done = Done {
                            usage: UsageOut {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                            },
                            effective_model: d.effective.id.clone(),
                            selected_model: rt.selected_model.clone(),
                            quota_decision: if downgrade { "downgrade" } else { "allow" },
                            downgrade_from: downgrade.then(|| rt.selected_model.clone()),
                            downgrade_reason: if downgrade {
                                d.downgrade_reason.clone()
                            } else {
                                None
                            },
                            quota_warnings: warnings,
                        };
                        tx.send(StreamEvent::Done(done)).await.ok();
                    } else {
                        let code = error_code.unwrap_or_else(|| "provider_error".to_owned());
                        svc.metrics.inc(
                            "stream_failed",
                            &[
                                ("provider", &provider_label),
                                ("model", &model_label),
                                ("error_code", &code),
                            ],
                        );
                        let message = match (&provider_error, code.as_str()) {
                            (Some(e), _) if original_code.as_deref() == Some(code.as_str()) => {
                                e.client_message()
                            }
                            _ => error_message(&code, detail.as_deref()),
                        };
                        tx.send(StreamEvent::error(&code, message)).await.ok();
                    }
                }
                FinalizeResult::Lost => {}
                FinalizeResult::Failed(_) => {
                    let code = if completed {
                        "finalization_failed".to_owned()
                    } else {
                        original_code.unwrap_or_else(|| "provider_error".to_owned())
                    };
                    let message = if completed {
                        "The response could not be finalized".to_owned()
                    } else {
                        match &provider_error {
                            Some(e) => e.client_message(),
                            None => error_message(&code, detail.as_deref()),
                        }
                    };
                    tx.send(StreamEvent::error(&code, message)).await.ok();
                }
            }
        }
    }
}

/// `quota_warnings` of the `done` event (current bucket state).
async fn quota_warnings(svc: &AppServices, rt: &TurnRuntime) -> Option<Vec<QuotaWarningOut>> {
    let conn = svc.db.conn().ok()?;
    let periods = quota::Periods::at(now());
    let usage = quota::load_usage(&conn, rt.tenant_id, rt.user_id, &periods)
        .await
        .ok()?;
    let status = quota::compute_status(
        &rt.decision.limits,
        &usage,
        now(),
        svc.cfg.quota.warning_threshold_pct,
    );
    let mut out = Vec::new();
    for t in status {
        for p in t.periods {
            out.push(QuotaWarningOut {
                tier: t.tier,
                period: p.period,
                remaining_percentage: p.remaining_pct,
                warning: p.warning,
                exhausted: p.exhausted,
                next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
            });
        }
    }
    Some(out)
}
