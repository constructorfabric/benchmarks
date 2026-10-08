//! The provider task of a live turn: relays provider events to the SSE
//! channel as they arrive (no buffering), enforces the per-turn tool limits,
//! runs the knowledge-search loop, observes client disconnects and finalizes
//! the turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::finalize::{Counts, FinalizeCtx, Finalized, Outcome};
use super::plan::KnowledgeParams;
use super::{CitationView, DeltaKind, DoneView, StreamEvent};
use crate::domain::service::Services;
use crate::infra::db::repo::turns;
use crate::infra::db::{now_ts, tenant_scope};
use crate::infra::llm::registry::ResolvedProvider;
use crate::infra::llm::types::{InputItem, LlmRequest, ProviderEvent, RawCitation};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// A live turn's provider task.
pub struct TurnRun {
    pub svc: Arc<Services>,
    pub fc: FinalizeCtx,
    pub request: LlmRequest,
    pub target: ResolvedProvider,
    pub knowledge: Option<KnowledgeParams>,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub tx: mpsc::Sender<StreamEvent>,
    pub cancel: CancellationToken,
}

enum DriveResult {
    Completed {
        text: String,
        usage: Option<mini_chat_sdk::UsageTokens>,
        response_id: Option<String>,
        incomplete: Option<String>,
        citations: Vec<RawCitation>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<mini_chat_sdk::UsageTokens>,
    },
    Cancelled {
        text: String,
        before_first_token: bool,
    },
}

struct State {
    text: String,
    counts: Counts,
    ws_started: u32,
    ci_started: u32,
    knowledge_calls: u32,
    first_token: Option<Instant>,
    last_progress: Instant,
}

impl TurnRun {
    fn labels(&self) -> [(&'static str, String); 2] {
        [
            ("provider", self.target.provider_id.clone()),
            ("model", self.fc.effective_model.clone()),
        ]
    }

    fn metric(&self, name: &str, extra: &[(&'static str, &str)]) {
        let l = self.labels();
        let mut labels: Vec<(&'static str, &str)> =
            vec![(l[0].0, l[0].1.as_str()), (l[1].0, l[1].1.as_str())];
        labels.extend_from_slice(extra);
        self.svc.metrics.inc(name, &labels);
    }

    async fn send(&self, ev: StreamEvent) -> bool {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => false,
            r = self.tx.send(ev) => r.is_ok(),
        }
    }

    /// Refresh `last_progress_at` at most every `PROGRESS_INTERVAL`.
    async fn progress(&self, st: &mut State) {
        if st.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.touch(st).await;
        }
    }

    /// Refresh `last_progress_at` now.
    async fn touch(&self, st: &mut State) {
        st.last_progress = Instant::now();
        let Ok(conn) = self.svc.db.conn() else {
            return;
        };
        if let Err(e) = turns::touch_progress(
            &conn,
            &tenant_scope(self.fc.tenant_id),
            self.fc.turn_id,
            now_ts(),
        )
        .await
        {
            tracing::warn!(error = %e, "failed to refresh turn progress");
        }
    }

    async fn count_file_search(&self) {
        if let Ok(conn) = self.svc.db.conn()
            && let Err(e) =
                turns::inc_file_search(&conn, &tenant_scope(self.fc.tenant_id), self.fc.turn_id)
                    .await
        {
            tracing::warn!(error = %e, "failed to count a file search call");
        }
    }

    async fn knowledge_output(
        &self,
        params: &KnowledgeParams,
        arguments: &str,
        st: &State,
    ) -> String {
        let cfg = &self.svc.cfg.knowledge_search;
        if st.knowledge_calls > cfg.max_calls_per_message {
            return "Search limit reached for this message. Answer with the information already retrieved.".to_owned();
        }
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let top_k = args
            .get("top_k")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(cfg.top_k)
            .clamp(1, cfg.top_k);
        let started = Instant::now();
        let res = self
            .svc
            .llm
            .search_vector_store(
                &params.alias,
                &params.api_version,
                &params.vector_store_id,
                &query,
                top_k,
            )
            .await;
        #[allow(clippy::cast_precision_loss)]
        self.svc.metrics.record(
            "knowledge_search_latency_ms",
            started.elapsed().as_millis() as f64,
            &[],
        );
        match res {
            Ok(chunks) => {
                self.svc
                    .metrics
                    .inc("knowledge_search", &[("result", "ok")]);
                #[allow(clippy::cast_precision_loss)]
                self.svc
                    .metrics
                    .record("knowledge_search_chunks", chunks.len() as f64, &[]);
                self.count_file_search().await;
                let parts: Vec<String> = chunks
                    .into_iter()
                    .map(|(name, text)| {
                        let t: String = text.chars().take(cfg.max_chunk_chars).collect();
                        format!("[{name}]\n{t}")
                    })
                    .collect();
                if parts.is_empty() {
                    "No relevant results found.".to_owned()
                } else {
                    parts.join("\n\n")
                }
            }
            Err(e) => {
                self.svc
                    .metrics
                    .inc("knowledge_search", &[("result", "error")]);
                tracing::warn!(error = %e, "knowledge search failed");
                "Knowledge search is temporarily unavailable.".to_owned()
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn drive(&self, st: &mut State) -> DriveResult {
        let cfg = &self.svc.cfg;
        let max_iterations = cfg.knowledge_search.max_calls_per_message + 2;
        let mut input = self.request.input.clone();
        let mut iterations = 0u32;
        loop {
            iterations += 1;
            if iterations > max_iterations {
                return DriveResult::Failed {
                    code: "agentic_iterations_exceeded".into(),
                    message: "Tool-use iteration limit exceeded".into(),
                    usage: None,
                };
            }
            let mut req = self.request.clone();
            req.input = input.clone();
            let s2s = self.svc.llm.s2s();
            let stream = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    return DriveResult::Cancelled { text: st.text.clone(), before_first_token: st.first_token.is_none() };
                }
                r = self.svc.llm.stream_chat(s2s.as_ref(), &self.target, &req) => r,
            };
            let mut stream = match stream {
                Ok(s) => s,
                Err(f) => {
                    return DriveResult::Failed {
                        code: f.code.as_str().to_owned(),
                        message: f.message,
                        usage: f.usage,
                    };
                }
            };
            loop {
                let ev = tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => {
                        return DriveResult::Cancelled { text: st.text.clone(), before_first_token: st.first_token.is_none() };
                    }
                    ev = stream.next() => ev,
                };
                let Some(ev) = ev else {
                    return DriveResult::Failed {
                        code: "provider_error".into(),
                        message: "Provider stream ended unexpectedly".into(),
                        usage: None,
                    };
                };
                match ev {
                    ProviderEvent::TextDelta(t) => {
                        if st.first_token.is_none() {
                            st.first_token = Some(Instant::now());
                        }
                        st.text.push_str(&t);
                        if !self
                            .send(StreamEvent::Delta {
                                kind: DeltaKind::Text,
                                content: t,
                            })
                            .await
                        {
                            return DriveResult::Cancelled {
                                text: st.text.clone(),
                                before_first_token: false,
                            };
                        }
                        self.progress(st).await;
                    }
                    ProviderEvent::ReasoningDelta(t) => {
                        if !self
                            .send(StreamEvent::Delta {
                                kind: DeltaKind::Reasoning,
                                content: t,
                            })
                            .await
                        {
                            return DriveResult::Cancelled {
                                text: st.text.clone(),
                                before_first_token: st.first_token.is_none(),
                            };
                        }
                    }
                    ProviderEvent::ToolStart { name, details } => {
                        if name == "web_search" {
                            st.ws_started += 1;
                            if st.ws_started > cfg.quota.web_search_max_calls_per_message {
                                return DriveResult::Failed {
                                    code: "web_search_calls_exceeded".into(),
                                    message: "Web search call limit exceeded for this message"
                                        .into(),
                                    usage: None,
                                };
                            }
                        }
                        if name == "code_interpreter" {
                            st.ci_started += 1;
                            if st.ci_started > cfg.quota.code_interpreter_max_calls_per_message {
                                return DriveResult::Failed {
                                    code: "code_interpreter_calls_exceeded".into(),
                                    message:
                                        "Code interpreter call limit exceeded for this message"
                                            .into(),
                                    usage: None,
                                };
                            }
                        }
                        if !self
                            .send(StreamEvent::Tool {
                                phase: "start",
                                name,
                                details,
                            })
                            .await
                        {
                            return DriveResult::Cancelled {
                                text: st.text.clone(),
                                before_first_token: st.first_token.is_none(),
                            };
                        }
                        self.touch(st).await;
                    }
                    ProviderEvent::ToolDone { name, details } => {
                        match name.as_str() {
                            "web_search" => st.counts.web_search += 1,
                            "code_interpreter" => st.counts.code_interpreter += 1,
                            "file_search" => {
                                st.counts.file_search += 1;
                                self.count_file_search().await;
                            }
                            _ => {}
                        }
                        if !self
                            .send(StreamEvent::Tool {
                                phase: "done",
                                name,
                                details,
                            })
                            .await
                        {
                            return DriveResult::Cancelled {
                                text: st.text.clone(),
                                before_first_token: st.first_token.is_none(),
                            };
                        }
                        self.progress(st).await;
                    }
                    ProviderEvent::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    } => {
                        let Some(params) = &self.knowledge else {
                            return DriveResult::Failed {
                                code: "unexpected_tool_use".into(),
                                message: format!("The model requested an unsupported tool: {name}"),
                                usage: None,
                            };
                        };
                        if name != "search_knowledge" {
                            return DriveResult::Failed {
                                code: "unexpected_tool_use".into(),
                                message: format!("The model requested an unsupported tool: {name}"),
                                usage: None,
                            };
                        }
                        st.knowledge_calls += 1;
                        st.counts.file_search += 1;
                        let output = self.knowledge_output(params, &arguments, st).await;
                        input.push(InputItem::FunctionCall {
                            call_id: call_id.clone(),
                            name,
                            arguments,
                        });
                        input.push(InputItem::FunctionCallOutput { call_id, output });
                        break;
                    }
                    ProviderEvent::Completed {
                        response_id,
                        usage,
                        citations,
                        incomplete_reason,
                    } => {
                        return DriveResult::Completed {
                            text: st.text.clone(),
                            usage,
                            response_id,
                            incomplete: incomplete_reason,
                            citations,
                        };
                    }
                    ProviderEvent::Failed(f) => {
                        return DriveResult::Failed {
                            code: f.code.as_str().to_owned(),
                            message: f.message,
                            usage: f.usage,
                        };
                    }
                }
            }
        }
    }

    fn map_citations(&self, raw: Vec<RawCitation>) -> Vec<CitationView> {
        raw.into_iter()
            .filter_map(|c| match c {
                RawCitation::Web {
                    url,
                    title,
                    snippet,
                    span,
                } => Some(CitationView {
                    source: "web",
                    title,
                    url: Some(url),
                    attachment_id: None,
                    snippet,
                    span,
                }),
                RawCitation::File { file_id, span, .. } => {
                    self.file_map.get(&file_id).map(|(id, name)| CitationView {
                        source: "file",
                        title: name.clone(),
                        url: None,
                        attachment_id: Some(*id),
                        snippet: String::new(),
                        span,
                    })
                }
            })
            .collect()
    }

    /// Run the turn to its terminal event.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub async fn run(self) {
        let svc = self.svc.clone();
        svc.metrics.gauge_add("active_streams", 1);
        self.metric("stream_started", &[]);
        let mut st = State {
            text: String::new(),
            counts: Counts::default(),
            ws_started: 0,
            ci_started: 0,
            knowledge_calls: 0,
            first_token: None,
            last_progress: Instant::now(),
        };
        let provider_started = Instant::now();
        let result = self.drive(&mut st).await;
        if let Some(t) = st.first_token {
            #[allow(clippy::cast_precision_loss)]
            {
                let l = self.labels();
                let labels = [(l[0].0, l[0].1.as_str()), (l[1].0, l[1].1.as_str())];
                svc.metrics.record(
                    "ttft_provider_ms",
                    t.duration_since(provider_started).as_millis() as f64,
                    &labels,
                );
                svc.metrics.record("ttft_overhead_ms", 0.0, &labels);
            }
        }
        let mut counts = st.counts;
        counts.ttft_ms = st
            .first_token
            .map(|t| u64::try_from(t.duration_since(provider_started).as_millis()).unwrap_or(0));
        match result {
            DriveResult::Completed {
                text,
                usage,
                response_id,
                incomplete,
                citations,
            } => {
                let outcome = Outcome::Completed {
                    text,
                    usage,
                    response_id,
                };
                match svc.finalize_turn(&self.fc, &outcome, counts).await {
                    Finalized::Committed { .. } => {
                        self.metric("stream_completed", &[]);
                        if let Some(reason) = &incomplete {
                            tracing::warn!(reason = %reason, turn_id = %self.fc.turn_id, "stream incomplete");
                            self.metric("stream_incomplete", &[("reason", reason)]);
                        } else {
                            let views = self.map_citations(citations);
                            if !views.is_empty() {
                                let _ = self.send(StreamEvent::Citations(views)).await;
                            }
                        }
                        if counts.code_interpreter > 0 {
                            svc.metrics.add(
                                "code_interpreter_calls",
                                u64::try_from(counts.code_interpreter).unwrap_or(0),
                                &[("model", &self.fc.effective_model)],
                            );
                        }
                        let u = usage.unwrap_or_default();
                        let warnings = svc
                            .quota_warnings(self.fc.tenant_id, self.fc.user_id, &self.fc.limits)
                            .await;
                        let done = DoneView {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            effective_model: self.fc.effective_model.clone(),
                            selected_model: self.fc.selected_model.clone(),
                            downgrade: self.fc.downgrade,
                            downgrade_from: self
                                .fc
                                .downgrade
                                .then(|| self.fc.selected_model.clone()),
                            downgrade_reason: if self.fc.downgrade {
                                self.fc.downgrade_reason.clone()
                            } else {
                                None
                            },
                            quota_warnings: Some(warnings),
                        };
                        let _ = self.send(StreamEvent::Done(done)).await;
                    }
                    Finalized::PersistFailed => {
                        self.metric(
                            "stream_failed",
                            &[("error_code", "message_persistence_failed")],
                        );
                        let _ = self
                            .send(StreamEvent::error(
                                "message_persistence_failed",
                                "The answer could not be saved",
                            ))
                            .await;
                    }
                    Finalized::Lost => {}
                    Finalized::TxFailed(e) => {
                        tracing::warn!(error = %e, turn_id = %self.fc.turn_id, "finalization failed");
                        self.metric("stream_failed", &[("error_code", "finalization_failed")]);
                        let _ = self
                            .send(StreamEvent::error(
                                "finalization_failed",
                                "The turn could not be finalized",
                            ))
                            .await;
                    }
                }
            }
            DriveResult::Failed {
                code,
                message,
                usage,
            } => {
                let outcome = Outcome::Failed {
                    code: code.clone(),
                    message: message.clone(),
                    usage,
                    response_id: None,
                };
                let fin = svc.finalize_turn(&self.fc, &outcome, counts).await;
                if let Finalized::TxFailed(e) = &fin {
                    tracing::warn!(error = %e, turn_id = %self.fc.turn_id, "finalization of a failed turn failed");
                }
                if fin != Finalized::Lost {
                    self.metric("stream_failed", &[("error_code", &code)]);
                    let _ = self.send(StreamEvent::error(&code, message)).await;
                }
            }
            DriveResult::Cancelled {
                text,
                before_first_token,
            } => {
                let observed = Instant::now();
                svc.metrics
                    .inc("cancel_requested", &[("trigger", "disconnect")]);
                svc.metrics.inc(
                    "stream_disconnected",
                    &[(
                        "stage",
                        if before_first_token {
                            "before_first_token"
                        } else {
                            "mid_stream"
                        },
                    )],
                );
                #[allow(clippy::cast_precision_loss)]
                svc.metrics.record(
                    "time_to_abort_ms",
                    observed.elapsed().as_millis() as f64,
                    &[("trigger", "disconnect")],
                );
                let outcome = Outcome::Cancelled { text };
                if let Finalized::Committed { .. } =
                    svc.finalize_turn(&self.fc, &outcome, counts).await
                {
                    svc.metrics
                        .inc("cancel_effective", &[("trigger", "disconnect")]);
                    svc.metrics
                        .inc("streams_aborted", &[("trigger", "client_disconnect")]);
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        {
            let l = self.labels();
            svc.metrics.record(
                "stream_total_latency_ms",
                self.fc.started.elapsed().as_millis() as f64,
                &[(l[0].0, l[0].1.as_str()), (l[1].0, l[1].1.as_str())],
            );
        }
        svc.metrics.gauge_add("active_streams", -1);
    }
}
