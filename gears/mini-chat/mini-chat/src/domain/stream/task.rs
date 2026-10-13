//! Provider task: relays provider events through the bounded channel,
//! enforces per-turn tool limits, refreshes progress and finalizes the turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{ModelTier, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::finalize::{FinalizeOutcome, Outcome};
use super::{CitationOut, StreamEvent};
use crate::domain::billing::Reserve;
use crate::domain::quota::PeriodStarts;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{attachments, chat_turns};
use crate::infra::db::now;
use crate::infra::llm::client::ProviderError;
use crate::infra::llm::resolver::{KnowledgeTarget, ResolvedProvider};
use crate::infra::llm::{LlmRequest, ProviderEvent, RawCitation, ToolExchange};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Everything the provider task and finalization need about one turn.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // reason: independent per-turn facts, not a state machine
pub struct TurnRun {
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub chat_id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub assistant_message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub tier: ModelTier,
    pub in_mult: i64,
    pub out_mult: i64,
    pub downgraded: bool,
    pub downgrade_reason: Option<String>,
    pub reserve: Reserve,
    pub periods: PeriodStarts,
    pub policy_version: u64,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    /// Knowledge-search target when `search_knowledge` is offered.
    pub knowledge: Option<KnowledgeTarget>,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    pub has_summary: bool,
    pub started: Instant,
}

/// Per-turn tool counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounters {
    pub web_search_started: u32,
    pub web_search_completed: u32,
    pub code_interpreter_started: u32,
    pub code_interpreter_completed: u32,
    pub file_search_completed: u32,
    /// In-memory `search_knowledge` call count (incremented before retrieval).
    pub knowledge_calls: u32,
}

impl ToolCounters {
    /// `file_search_calls` of the usage / audit events: the knowledge-search
    /// call count when that tool ran, else the provider `file_search` count
    /// (the two tools are never offered together).
    #[must_use]
    pub fn file_search_calls(&self) -> u32 {
        if self.knowledge_calls > 0 {
            self.knowledge_calls
        } else {
            self.file_search_completed
        }
    }
}

enum SendResult {
    Sent,
    Disconnected,
}

async fn send(tx: &mpsc::Sender<StreamEvent>, cancel: &CancellationToken, ev: StreamEvent) -> SendResult {
    tokio::select! {
        biased;
        () = cancel.cancelled() => SendResult::Disconnected,
        r = tx.send(ev) => if r.is_ok() { SendResult::Sent } else { SendResult::Disconnected },
    }
}

impl MiniChat {
    /// Resolve provider citations to client citations (unknown / deleted files omitted).
    async fn resolve_citations(&self, run: &TurnRun, raw: Vec<RawCitation>) -> Vec<CitationOut> {
        let mut out = Vec::new();
        let mut file_ids = Vec::new();
        for c in &raw {
            if let RawCitation::File { file_id } = c {
                file_ids.push(file_id.clone());
            }
        }
        // drop citations of attachments deleted while the turn ran
        let mut live: HashMap<String, (Uuid, String)> = HashMap::new();
        if !file_ids.is_empty()
            && let Ok(conn) = self.db.conn()
            && let Ok(rows) = attachments::Entity::find()
                .filter(
                    Condition::all()
                        .add(attachments::Column::ChatId.eq(run.chat_id))
                        .add(attachments::Column::ProviderFileId.is_in(file_ids))
                        .add(attachments::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&AccessScope::for_tenant(run.tenant_id))
                .all(&conn)
                .await
        {
            for r in rows {
                if let Some(pid) = r.provider_file_id {
                    live.insert(pid, (r.id, r.filename));
                }
            }
        }
        for c in raw {
            match c {
                RawCitation::Web {
                    url,
                    title,
                    snippet,
                    span,
                } => out.push(CitationOut {
                    source: "web",
                    title,
                    url: Some(url),
                    attachment_id: None,
                    snippet,
                    span,
                }),
                RawCitation::File { file_id } => {
                    if run.file_map.contains_key(&file_id)
                        && let Some((id, name)) = live.get(&file_id)
                    {
                        out.push(CitationOut {
                            source: "file",
                            title: name.clone(),
                            url: None,
                            attachment_id: Some(*id),
                            snippet: String::new(),
                            span: None,
                        });
                    }
                }
            }
        }
        out
    }

    /// Run one `search_knowledge` retrieval and format the function output.
    async fn knowledge_output(&self, kt: &KnowledgeTarget, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| "missing query".to_owned())?;
        let top_k = args
            .get("top_k")
            .and_then(serde_json::Value::as_u64)
            .and_then(|k| usize::try_from(k).ok())
            .filter(|k| *k > 0)
            .map_or(kt.top_k, |k| k.min(kt.top_k));
        let chunks = self
            .storage
            .search_knowledge(kt, query, top_k)
            .await
            .map_err(|e| e.to_string())?;
        let items: Vec<serde_json::Value> = chunks
            .into_iter()
            .map(|c| json!({ "source": c.filename, "content": c.text }))
            .collect();
        Ok(json!({ "results": items }).to_string())
    }

    async fn touch_progress(&self, run: &TurnRun, counters: &ToolCounters) {
        let Ok(conn) = self.db.conn() else { return };
        let ts = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(Some(ts)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
            .col_expr(
                chat_turns::Column::WebSearchCompletedCount,
                Expr::value(i32::try_from(counters.web_search_completed).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turns::Column::CodeInterpreterCompletedCount,
                Expr::value(i32::try_from(counters.code_interpreter_completed).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turns::Column::FileSearchCompletedCount,
                Expr::value(i32::try_from(counters.file_search_completed).unwrap_or(i32::MAX)),
            )
            .filter(
                Condition::all()
                    .add(chat_turns::Column::Id.eq(run.turn_id))
                    .add(chat_turns::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(run.tenant_id))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "turn progress update failed");
        }
    }

    /// Run the provider stream for a committed turn.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub async fn run_provider_task(
        self: Arc<Self>,
        run: TurnRun,
        tx: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let mut counters = ToolCounters::default();
        let mut text = String::new();
        let mut last_progress = Instant::now();
        let mut pending_citations: Vec<CitationOut> = Vec::new();

        let mut req = run.request.clone();
        let mut pending_calls: Vec<(String, String, String)> = Vec::new();
        let mut iterations: u32 = 1;
        let open = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            r = self.llm.stream(&run.provider, &req) => Some(r),
        };
        let mut stream = match open {
            None => {
                self.finish(&run, Outcome::Cancelled { text }, counters, &tx, None).await;
                return;
            }
            Some(Err(e)) => {
                self.finish(&run, Outcome::from_provider_error(&e, None), counters, &tx, None).await;
                return;
            }
            Some(Ok(s)) => s,
        };

        let ws_limit = self.cfg.quota.web_search_max_calls_per_message;
        let ci_limit = self.cfg.quota.code_interpreter_max_calls_per_message;
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    drop(stream);
                    self.finish(&run, Outcome::Cancelled { text }, counters, &tx, None).await;
                    return;
                }
                ev = stream.next_event() => ev,
            };
            let ev = match next {
                None => {
                    self.finish(
                        &run,
                        Outcome::from_provider_error(
                            &ProviderError::Provider("Provider stream ended unexpectedly".into()),
                            None,
                        ),
                        counters,
                        &tx,
                        None,
                    )
                    .await;
                    return;
                }
                Some(Err(e)) => {
                    self.finish(&run, Outcome::from_provider_error(&e, None), counters, &tx, None).await;
                    return;
                }
                Some(Ok(ev)) => ev,
            };
            let mut progress = false;
            let out_ev = match ev {
                ProviderEvent::Delta { kind, text: t } => {
                    if kind == crate::infra::llm::DeltaKind::Text {
                        text.push_str(&t);
                    }
                    progress = true;
                    Some(StreamEvent::Delta { kind, content: t })
                }
                ProviderEvent::ToolStart { name, details } => {
                    progress = true;
                    if name == "web_search" {
                        counters.web_search_started += 1;
                        if counters.web_search_started > ws_limit {
                            drop(stream);
                            self.finish(&run, Outcome::failed("web_search_calls_exceeded", "Too many web search calls in one message"), counters, &tx, None).await;
                            return;
                        }
                    } else if name == "code_interpreter" {
                        counters.code_interpreter_started += 1;
                        if counters.code_interpreter_started > ci_limit {
                            drop(stream);
                            self.finish(&run, Outcome::failed("code_interpreter_calls_exceeded", "Too many code interpreter calls in one message"), counters, &tx, None).await;
                            return;
                        }
                    }
                    Some(StreamEvent::Tool { phase: "start", name, details })
                }
                ProviderEvent::ToolDone { name, details } => {
                    match name.as_str() {
                        "web_search" => counters.web_search_completed += 1,
                        "code_interpreter" => counters.code_interpreter_completed += 1,
                        "file_search" => counters.file_search_completed += 1,
                        _ => {}
                    }
                    self.touch_progress(&run, &counters).await;
                    last_progress = Instant::now();
                    Some(StreamEvent::Tool { phase: "done", name, details })
                }
                ProviderEvent::Citations(raw) => {
                    pending_citations.extend(self.resolve_citations(&run, raw).await);
                    None
                }
                ProviderEvent::FunctionCall { call_id, name, arguments } => {
                    progress = true;
                    if name == "search_knowledge" && run.knowledge.is_some() {
                        pending_calls.push((call_id, name, arguments));
                        None
                    } else {
                        drop(stream);
                        self.finish(
                            &run,
                            Outcome::failed("unexpected_tool_use", "The model requested a tool that is not available"),
                            counters,
                            &tx,
                            None,
                        )
                        .await;
                        return;
                    }
                }
                ProviderEvent::Completed { usage, .. }
                    if !pending_calls.is_empty() && let Some(kt) = run.knowledge.as_ref() =>
                {
                    // agentic loop: run the retrievals, replay call + output, ask again
                    drop(stream);
                    if iterations >= kt.max_calls.saturating_add(2) {
                        self.finish(
                            &run,
                            Outcome::Failed {
                                code: "agentic_iterations_exceeded".into(),
                                message: "Too many knowledge search iterations in one message".into(),
                                usage,
                            },
                            counters,
                            &tx,
                            None,
                        )
                        .await;
                        return;
                    }
                    for (call_id, name, arguments) in std::mem::take(&mut pending_calls) {
                        counters.knowledge_calls += 1;
                        let output = if counters.knowledge_calls > kt.max_calls {
                            "search limit reached".to_owned()
                        } else {
                            match self.knowledge_output(kt, &arguments).await {
                                Ok(o) => {
                                    counters.file_search_completed += 1;
                                    o
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, turn_id = %run.turn_id, "knowledge search failed");
                                    "knowledge search failed".to_owned()
                                }
                            }
                        };
                        req.tool_exchanges.push(ToolExchange { call_id, name, arguments, output });
                    }
                    self.touch_progress(&run, &counters).await;
                    last_progress = Instant::now();
                    iterations += 1;
                    let open = tokio::select! {
                        biased;
                        () = cancel.cancelled() => None,
                        r = self.llm.stream(&run.provider, &req) => Some(r),
                    };
                    stream = match open {
                        None => {
                            self.finish(&run, Outcome::Cancelled { text }, counters, &tx, None).await;
                            return;
                        }
                        Some(Err(e)) => {
                            self.finish(&run, Outcome::from_provider_error(&e, None), counters, &tx, None).await;
                            return;
                        }
                        Some(Ok(s)) => s,
                    };
                    continue;
                }
                ProviderEvent::Completed {
                    response_id,
                    usage,
                    incomplete_reason,
                } => {
                    if incomplete_reason.is_some() {
                        pending_citations.clear();
                    }
                    let citations = std::mem::take(&mut pending_citations);
                    self.finish(
                        &run,
                        Outcome::Completed {
                            text,
                            usage,
                            response_id,
                            incomplete_reason,
                        },
                        counters,
                        &tx,
                        Some(citations),
                    )
                    .await;
                    return;
                }
                ProviderEvent::Failed { code, message, usage } => {
                    let _ = code;
                    self.finish(
                        &run,
                        Outcome::Failed {
                            code: "provider_error".into(),
                            message,
                            usage,
                        },
                        counters,
                        &tx,
                        None,
                    )
                    .await;
                    return;
                }
            };
            if progress && last_progress.elapsed() >= PROGRESS_INTERVAL {
                self.touch_progress(&run, &counters).await;
                last_progress = Instant::now();
            }
            if let Some(ev) = out_ev
                && matches!(send(&tx, &cancel, ev).await, SendResult::Disconnected)
            {
                drop(stream);
                self.finish(&run, Outcome::Cancelled { text }, counters, &tx, None).await;
                return;
            }
        }
    }

    /// Finalize and emit the terminal event (only after the commit).
    async fn finish(
        &self,
        run: &TurnRun,
        outcome: Outcome,
        counters: ToolCounters,
        tx: &mpsc::Sender<StreamEvent>,
        citations: Option<Vec<CitationOut>>,
    ) {
        let is_cancel = matches!(outcome, Outcome::Cancelled { .. });
        let original_error = match &outcome {
            Outcome::Failed { code, message, .. } => Some((code.clone(), message.clone())),
            _ => None,
        };
        let result = self.finalize_turn(run, outcome, counters).await;
        if is_cancel {
            return;
        }
        let terminal = match result {
            FinalizeOutcome::Completed(done) => {
                if let Some(c) = citations.filter(|c| !c.is_empty()) {
                    tx.send(StreamEvent::Citations(c)).await.ok();
                }
                StreamEvent::Done(done)
            }
            FinalizeOutcome::Failed { code, message } => StreamEvent::Error { code, message },
            FinalizeOutcome::Lost => return,
            FinalizeOutcome::Error(e) => {
                tracing::error!(error = %e, turn_id = %run.turn_id, "turn finalization failed");
                match original_error {
                    Some((code, message)) => StreamEvent::Error {
                        code,
                        message: crate::domain::sanitize::sanitize_provider_message(&message),
                    },
                    None => StreamEvent::Error {
                        code: "finalization_failed".into(),
                        message: "The response could not be finalized".into(),
                    },
                }
            }
        };
        tx.send(terminal).await.ok();
    }
}

impl Outcome {
    #[must_use]
    pub fn failed(code: &str, message: &str) -> Self {
        Self::Failed {
            code: code.to_owned(),
            message: message.to_owned(),
            usage: None,
        }
    }

    #[must_use]
    pub fn from_provider_error(e: &ProviderError, usage: Option<UsageTokens>) -> Self {
        Self::Failed {
            code: e.code().to_owned(),
            message: e.message(),
            usage,
        }
    }
}

/// Details of a tool event payload (empty object).
#[must_use]
pub fn empty_details() -> serde_json::Value {
    json!({})
}
