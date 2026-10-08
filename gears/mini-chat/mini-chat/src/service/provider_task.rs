//! Provider task of a live turn: relays provider events to the SSE channel,
//! enforces per-turn tool limits, runs the knowledge-search agentic loop and
//! finalizes the turn before emitting the terminal event (DESIGN §5.7).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::UsageTokens;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::AppState;
use super::finalize::{FinalizeOutcome, Outcome, TurnCounts, bump_counter, touch_progress};
use super::stream::{CitationOut, DoneOut, StreamEvent, TurnPlan};
use crate::infra::db::entity::chat_turn;
use crate::infra::llm::{CitationSource, LlmEvent, ProviderError, RawCitation};

/// `last_progress_at` is refreshed at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Knowledge search function tool name.
const KNOWLEDGE_TOOL: &str = "search_knowledge";

/// How one provider iteration ended.
enum IterEnd {
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        citations: Vec<RawCitation>,
        incomplete_reason: Option<String>,
    },
    FunctionCalls(Vec<(String, String, String)>),
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
    Cancelled,
}

struct Runner {
    state: Arc<AppState>,
    plan: TurnPlan,
    tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
    text: String,
    counts: TurnCounts,
    web_search_started: u32,
    ci_started: u32,
    knowledge_calls: u32,
    last_progress: Instant,
    /// Usage reported by the last provider iteration that ended in tool use.
    last_usage: Option<UsageTokens>,
}

impl Runner {
    /// Relay a non-terminal event; `false` when the client is gone.
    async fn relay(&mut self, ev: StreamEvent) -> bool {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => false,
            r = self.tx.send(ev) => r.is_ok(),
        }
    }

    /// Send the terminal event; a gone client is not an error here.
    async fn send_terminal(&self, ev: StreamEvent) {
        if self.tx.send(ev).await.is_err() {
            tracing::debug!(request_id = %self.plan.request_id, "client gone before the terminal event");
        }
    }

    async fn progress(&mut self) {
        if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.last_progress = Instant::now();
            if let Ok(conn) = self.state.conn() {
                touch_progress(&conn, self.plan.tenant_id, self.plan.turn_id).await;
            }
        }
    }

    async fn bump(&self, col: chat_turn::Column) {
        if let Ok(conn) = self.state.conn() {
            bump_counter(&conn, self.plan.tenant_id, self.plan.turn_id, col).await;
        }
    }

    /// Run one provider request until its terminal event.
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    async fn iterate(&mut self) -> IterEnd {
        let stream = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return IterEnd::Cancelled,
            s = self.state.llm.stream_chat(&self.plan.ctx, &self.plan.provider, &self.plan.request) => s,
        };
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => return failed(&e),
        };
        let mut calls = Vec::new();
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return IterEnd::Cancelled,
                n = stream.next() => n,
            };
            let Some(ev) = next else {
                if !calls.is_empty() {
                    return IterEnd::FunctionCalls(calls);
                }
                return IterEnd::Failed {
                    code: "provider_error".into(),
                    message: "Provider stream ended unexpectedly".into(),
                    usage: None,
                };
            };
            match ev {
                LlmEvent::TextDelta(t) => {
                    if t.is_empty() {
                        continue;
                    }
                    self.text.push_str(&t);
                    if !self
                        .relay(StreamEvent::Delta {
                            kind: "text",
                            content: t,
                        })
                        .await
                    {
                        return IterEnd::Cancelled;
                    }
                    self.progress().await;
                }
                LlmEvent::ReasoningDelta(t) => {
                    if t.is_empty() {
                        continue;
                    }
                    if !self
                        .relay(StreamEvent::Delta {
                            kind: "reasoning",
                            content: t,
                        })
                        .await
                    {
                        return IterEnd::Cancelled;
                    }
                    self.progress().await;
                }
                LlmEvent::ToolStart { name, details } => {
                    match name.as_str() {
                        "web_search" => {
                            self.web_search_started += 1;
                            if self.web_search_started
                                > self.state.cfg.quota.web_search_max_calls_per_message
                            {
                                return IterEnd::Failed {
                                    code: "web_search_calls_exceeded".into(),
                                    message:
                                        "The web search call limit for this message was exceeded"
                                            .into(),
                                    usage: None,
                                };
                            }
                        }
                        "code_interpreter" => {
                            self.ci_started += 1;
                            if self.ci_started
                                > self.state.cfg.quota.code_interpreter_max_calls_per_message
                            {
                                return IterEnd::Failed {
                                    code: "code_interpreter_calls_exceeded".into(),
                                    message: "The code interpreter call limit for this message was exceeded".into(),
                                    usage: None,
                                };
                            }
                        }
                        _ => {}
                    }
                    if !self
                        .relay(StreamEvent::Tool {
                            phase: "start",
                            name,
                            details,
                        })
                        .await
                    {
                        return IterEnd::Cancelled;
                    }
                    self.progress().await;
                }
                LlmEvent::ToolDone { name, details } => {
                    match name.as_str() {
                        "web_search" => {
                            self.counts.web_search += 1;
                            self.bump(chat_turn::Column::WebSearchCompletedCount).await;
                        }
                        "code_interpreter" => {
                            self.counts.code_interpreter += 1;
                            self.bump(chat_turn::Column::CodeInterpreterCompletedCount)
                                .await;
                        }
                        "file_search" => {
                            self.counts.file_search += 1;
                            self.bump(chat_turn::Column::FileSearchCompletedCount).await;
                        }
                        _ => {}
                    }
                    if !self
                        .relay(StreamEvent::Tool {
                            phase: "done",
                            name,
                            details,
                        })
                        .await
                    {
                        return IterEnd::Cancelled;
                    }
                    self.progress().await;
                }
                LlmEvent::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    calls.push((call_id, name, arguments));
                }
                LlmEvent::Completed {
                    usage,
                    response_id,
                    citations,
                } => {
                    if !calls.is_empty() {
                        self.last_usage = usage;
                        return IterEnd::FunctionCalls(calls);
                    }
                    return IterEnd::Completed {
                        usage,
                        response_id,
                        citations,
                        incomplete_reason: None,
                    };
                }
                LlmEvent::Incomplete {
                    usage,
                    response_id,
                    reason,
                } => {
                    if !calls.is_empty() {
                        self.last_usage = usage;
                        return IterEnd::FunctionCalls(calls);
                    }
                    return IterEnd::Completed {
                        usage,
                        response_id,
                        citations: vec![],
                        incomplete_reason: Some(reason),
                    };
                }
                LlmEvent::Failed(e) => return failed(&e),
            }
        }
    }

    /// Handle the function calls of one agentic iteration.
    ///
    /// Returns `Err((code, message))` when the turn must fail.
    async fn handle_calls(
        &mut self,
        calls: Vec<(String, String, String)>,
    ) -> Result<(), (String, String)> {
        let Some(k) = self.plan.knowledge.clone() else {
            return Err(unexpected_tool());
        };
        for (call_id, name, arguments) in calls {
            if name != KNOWLEDGE_TOOL {
                return Err(unexpected_tool());
            }
            self.plan.request.extra_input.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }));
            let output = if self.knowledge_calls >= k.max_calls {
                "search limit reached; answer with the information you already have".to_owned()
            } else {
                self.knowledge_calls += 1;
                let args: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let top_k = args
                    .get("top_k")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(k.top_k)
                    .clamp(1, k.top_k);
                let res = self
                    .state
                    .llm
                    .knowledge_search(
                        &self.plan.ctx,
                        &k.storage,
                        &k.vector_store_id,
                        &query,
                        top_k,
                    )
                    .await;
                match res {
                    Ok(chunks) => {
                        self.bump(chat_turn::Column::FileSearchCompletedCount).await;
                        let trimmed: Vec<String> = chunks
                            .into_iter()
                            .map(|c| c.chars().take(k.max_chunk_chars).collect())
                            .collect();
                        json!({ "results": trimmed }).to_string()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "knowledge search failed");
                        "knowledge search failed".to_owned()
                    }
                }
            };
            self.plan.request.extra_input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
            self.progress().await;
        }
        Ok(())
    }
}

fn unexpected_tool() -> (String, String) {
    (
        "unexpected_tool_use".into(),
        "The model requested a tool that is not available".into(),
    )
}

fn failed(e: &ProviderError) -> IterEnd {
    IterEnd::Failed {
        code: e.kind.code().to_owned(),
        message: e.message.clone(),
        usage: e.usage,
    }
}

/// Map raw citations to the public shape (unmapped file ids are dropped).
#[must_use]
pub fn map_citations(plan: &TurnPlan, raw: Vec<RawCitation>) -> Vec<CitationOut> {
    let mut out = Vec::new();
    for c in raw {
        match c.source {
            CitationSource::Web { url, title } => out.push(CitationOut {
                source: "web",
                title,
                url: Some(url),
                attachment_id: None,
                snippet: c.snippet,
                span: c.span,
            }),
            CitationSource::File { file_id, .. } => {
                if let Some((att, filename)) = plan.citation_map.get(&file_id) {
                    out.push(CitationOut {
                        source: "file",
                        title: filename.clone(),
                        url: None,
                        attachment_id: Some(*att),
                        snippet: String::new(),
                        span: None,
                    });
                }
            }
        }
    }
    out
}

/// Entry point spawned by `launch`.
#[allow(
    clippy::cognitive_complexity,
    reason = "one branch per terminal outcome of the turn"
)]
pub async fn run(
    state: Arc<AppState>,
    plan: TurnPlan,
    tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
) {
    let mut r = Runner {
        state: Arc::clone(&state),
        plan,
        tx,
        cancel,
        text: String::new(),
        counts: TurnCounts::default(),
        web_search_started: 0,
        ci_started: 0,
        knowledge_calls: 0,
        last_progress: Instant::now(),
        last_usage: None,
    };
    let max_iterations = r.plan.knowledge.as_ref().map_or(1, |k| k.max_calls + 2);
    let mut iteration = 0u32;
    let end = loop {
        iteration += 1;
        if iteration > max_iterations {
            break IterEnd::Failed {
                code: "agentic_iterations_exceeded".into(),
                message: "The tool iteration limit for this message was exceeded".into(),
                usage: r.last_usage,
            };
        }
        match r.iterate().await {
            IterEnd::FunctionCalls(calls) => {
                if let Err((code, message)) = r.handle_calls(calls).await {
                    break IterEnd::Failed {
                        code,
                        message,
                        usage: r.last_usage,
                    };
                }
            }
            other => break other,
        }
    };
    let counts = TurnCounts {
        file_search: r.counts.file_search + r.knowledge_calls,
        ..r.counts
    };
    let plan = r.plan.clone();
    match end {
        IterEnd::Completed {
            usage,
            response_id,
            citations,
            incomplete_reason,
        } => {
            let outcome = Outcome::Completed {
                text: std::mem::take(&mut r.text),
                usage,
                response_id,
                incomplete_reason: incomplete_reason.clone(),
            };
            match state.finalize_turn(&plan, outcome, counts).await {
                FinalizeOutcome::Committed {
                    state: "completed",
                    quota_warnings,
                } => {
                    let items = if incomplete_reason.is_none() {
                        map_citations(&plan, citations)
                    } else {
                        vec![]
                    };
                    if !items.is_empty() && !r.relay(StreamEvent::Citations(items)).await {
                        return;
                    }
                    let u = usage.unwrap_or_default();
                    r.send_terminal(StreamEvent::Done(DoneOut {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        effective_model: plan.effective.id.clone(),
                        selected_model: plan.selected_model.clone(),
                        downgrade: plan.downgrade,
                        downgrade_from: plan.downgrade.then(|| plan.selected_model.clone()),
                        downgrade_reason: if plan.downgrade {
                            plan.downgrade_reason.clone()
                        } else {
                            None
                        },
                        quota_warnings: Some(quota_warnings),
                    }))
                    .await;
                }
                FinalizeOutcome::Committed { .. } => {
                    r.send_terminal(StreamEvent::Error {
                        code: "message_persistence_failed".into(),
                        message: "The response could not be saved".into(),
                    })
                    .await;
                }
                FinalizeOutcome::CasLost => {}
                FinalizeOutcome::Error(_) => {
                    r.send_terminal(StreamEvent::Error {
                        code: "finalization_failed".into(),
                        message: "The turn could not be finalized".into(),
                    })
                    .await;
                }
            }
        }
        IterEnd::Failed {
            code,
            message,
            usage,
        } => {
            let outcome = Outcome::Failed {
                code: code.clone(),
                message: message.clone(),
                usage,
            };
            match state.finalize_turn(&plan, outcome, counts).await {
                FinalizeOutcome::CasLost => {}
                FinalizeOutcome::Committed { .. } | FinalizeOutcome::Error(_) => {
                    r.send_terminal(StreamEvent::Error { code, message }).await;
                }
            }
        }
        IterEnd::Cancelled => {
            let outcome = Outcome::Cancelled {
                text: std::mem::take(&mut r.text),
            };
            // No terminal event: the client is gone.
            if let FinalizeOutcome::Error(e) = state.finalize_turn(&plan, outcome, counts).await {
                tracing::warn!(error = %e, request_id = %plan.request_id, "cancelled turn finalization failed");
            }
        }
        IterEnd::FunctionCalls(_) => unreachable!("handled in the loop"),
    }
}
