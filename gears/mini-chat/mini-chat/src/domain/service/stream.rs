//! Send-message pipeline, replay, provider task and SSE relay (OWNER: streaming core).
//!
//! DESIGN §3.3 "Streaming Contract", "Send Message with Streaming Response", "Idempotency
//! Rules", "Parallel Turn Policy", §5.7 (terminal SSE gating). The setup ([`StreamService::send`])
//! returns either a buffered replay or a live stream fed by a provider task over a bounded
//! channel; the HTTP layer turns it into an SSE response.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use mini_chat_sdk::UsageTokens;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};
use toolkit_db::DbTx;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::api::rest::dto::{
    Citation, CitationSource, CitationsData, DeltaData, DeltaKind, DoneData, ErrorData,
    MiniChatSseEvent, PingData, QuotaDecisionKind, StreamStartedData, TextSpan, ThreadSummaryInfo,
    ToolData, ToolPhase, Usage,
};
use crate::config::RagConfig;
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, reasons, resource_types, stream_codes};
use crate::domain::outbox_payloads::simple_uuid;
use crate::domain::service::Deps;
use crate::domain::service::billing::TerminalState;
use crate::domain::service::chat_access::load_chat;
use crate::domain::service::context::{
    ContextInput, ContextPlan, assemble, estimate_message_tokens, input_limit, load_history,
};
use crate::domain::service::finalization::{
    self, FinalizeOutcome, Terminal, ToolCounters, TurnContext, retry_locked, user_message,
};
use crate::domain::service::quota::{PreflightDecision, PreflightInput, QuotaService, ToolGates};
use crate::domain::service::summary::SummaryTrigger;
use crate::infra::db::entity::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment,
};
use crate::infra::llm::sanitize::sanitize_provider_message;
use crate::infra::llm::{
    ContentPart, InputMessage, InputRole, LlmCompletion, LlmEvent, LlmRequest, RawCitation,
    RequestType, ToolExchange, ToolSpec,
};

/// Minimum interval between `last_progress_at` refreshes of a running turn.
const PROGRESS_REFRESH: Duration = Duration::from_secs(30);

// ── Errors ─────────────────────────────────────────────────────────────────

#[must_use]
pub fn empty_content() -> DomainError {
    DomainError::invalid(
        resource_types::CHAT,
        "content",
        reasons::EMPTY_CONTENT,
        "Message content must not be empty",
    )
}

#[must_use]
pub fn invalid_attachment() -> DomainError {
    DomainError::invalid(
        resource_types::CHAT,
        "attachment",
        reasons::INVALID_ATTACHMENT,
        "Invalid, duplicate, foreign or not ready attachment",
    )
}

#[must_use]
pub fn request_id_conflict() -> DomainError {
    DomainError::aborted(
        reasons::REQUEST_ID_CONFLICT,
        "The request_id is already used by another turn",
    )
}

#[must_use]
pub fn turn_already_running() -> DomainError {
    DomainError::aborted(
        reasons::TURN_ALREADY_RUNNING,
        "Another turn is already running in this chat",
    )
}

// ── Public types ───────────────────────────────────────────────────────────

/// Send-message request (decoded from `StreamMessageRequest`).
#[derive(Debug, Clone, Default)]
pub struct SendInput {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Outcome of a stream setup.
pub enum StreamStart {
    /// Idempotent replay of a completed turn (buffered events).
    Replay(Vec<MiniChatSseEvent>),
    /// New generation.
    Live(LiveStream),
}

/// Receiving side of a new generation. Dropping it (or the stream made from it) cancels the
/// turn (client disconnect).
pub struct LiveStream {
    rx: mpsc::Receiver<MiniChatSseEvent>,
    guard: DropGuard,
}

impl LiveStream {
    /// SSE event stream: forwards the provider task's events and closes after the terminal
    /// event; when the task ends without one, emits `error{stream_interrupted}`.
    pub fn into_events(self) -> impl Stream<Item = MiniChatSseEvent> + Send + 'static {
        let Self { mut rx, guard } = self;
        async_stream::stream! {
            let _guard = guard;
            let mut terminal = false;
            while let Some(ev) = rx.recv().await {
                let is_terminal = ev.is_terminal();
                yield ev;
                if is_terminal {
                    terminal = true;
                    break;
                }
            }
            if !terminal {
                yield error_event(stream_codes::STREAM_INTERRUPTED, "The stream was interrupted");
            }
        }
    }
}

fn error_event(code: &str, message: &str) -> MiniChatSseEvent {
    MiniChatSseEvent::Error(ErrorData {
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

/// An image of the current user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
}

impl From<&attachment::Model> for ImageRef {
    fn from(a: &attachment::Model) -> Self {
        Self {
            attachment_id: a.id,
            provider_file_id: a.provider_file_id.clone(),
        }
    }
}

/// Chat facts gathered before the quota preflight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gathered {
    pub has_ready_documents: bool,
    pub has_ready_code_interpreter_files: bool,
    pub prior_context_tokens: i64,
}

/// Context, tools and provider request of a turn (built before the reserve).
#[derive(Debug, Clone)]
pub struct PlannedTurn {
    pub request: LlmRequest,
    pub plan: ContextPlan,
    pub tools: ToolGates,
    /// `provider_file_id → (attachment_id, filename)` (only with `file_search`).
    pub file_map: HashMap<String, (Uuid, String)>,
    pub summary_trigger: Option<SummaryTrigger>,
    /// Knowledge-search parameters when `search_knowledge` is offered for this turn.
    pub knowledge: Option<KnowledgeParams>,
}

/// Name of the knowledge-search function tool.
pub const SEARCH_KNOWLEDGE: &str = "search_knowledge";

/// Output returned to the model for `search_knowledge` calls beyond the per-message limit.
pub const SEARCH_LIMIT_REACHED: &str =
    "Search limit reached: no more knowledge searches are allowed for this message. Answer using the information you already have.";

/// Per-request knowledge-search parameters (DESIGN §4 "Knowledge Search").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeParams {
    pub provider_id: String,
    pub vector_store_id: String,
    pub max_calls_per_message: u32,
    pub top_k: usize,
    pub max_chunk_chars: usize,
}

/// Builds the knowledge-search parameters of a request, or `None` (knowledge search off for
/// the request). When enabled, the provider entry `knowledge_search.provider_id` must be of
/// kind `openai_responses` or `anthropic_messages`, resolve to an upstream alias for the
/// tenant and have a non-empty `api_version`, and the retriever must be configured;
/// otherwise a warning is logged and `None` is returned.
#[must_use]
pub fn knowledge_params(
    cfg: &crate::config::MiniChatConfig,
    providers: &crate::infra::llm::ProviderResolver,
    retriever_configured: bool,
    tenant_id: Uuid,
) -> Option<KnowledgeParams> {
    use crate::config::ProviderKind;
    let ks = &cfg.knowledge_search;
    if !ks.enabled {
        return None;
    }
    let off = |why: &str| {
        tracing::warn!(reason = why, "knowledge search is off for this request");
        None
    };
    let Some(provider_id) = ks.provider_id.as_deref().filter(|p| !p.is_empty()) else {
        return off("knowledge_search.provider_id is not set");
    };
    let Some(vector_store_id) = ks.vector_store_id.as_deref().filter(|v| !v.is_empty()) else {
        return off("knowledge_search.vector_store_id is not set");
    };
    let Some(entry) = providers.entry(provider_id) else {
        return off("knowledge_search.provider_id is not a configured provider");
    };
    if !matches!(
        entry.kind,
        ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
    ) {
        return off("knowledge provider kind must be openai_responses or anthropic_messages");
    }
    match providers.resolve(provider_id, tenant_id) {
        Some(rp) if !rp.alias.trim().is_empty() => {}
        _ => return off("knowledge provider has no upstream alias for the tenant"),
    }
    if entry.api_version.as_deref().is_none_or(|v| v.trim().is_empty()) {
        return off("knowledge provider has no api_version");
    }
    if !retriever_configured {
        return off("knowledge retriever is not configured");
    }
    Some(KnowledgeParams {
        provider_id: provider_id.to_owned(),
        vector_store_id: vector_store_id.to_owned(),
        max_calls_per_message: ks.max_calls_per_message,
        top_k: ks.top_k,
        max_chunk_chars: ks.max_chunk_chars,
    })
}

/// The `search_knowledge` function tool.
#[must_use]
pub fn search_knowledge_tool() -> ToolSpec {
    ToolSpec::Function {
        name: SEARCH_KNOWLEDGE.to_owned(),
        description: "Search the organization knowledge base and return the most relevant text chunks.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "top_k": {
                    "type": "integer",
                    "description": "Maximum number of chunks to return",
                    "minimum": 1
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

/// Trims `text` to at most `max` characters.
#[must_use]
pub fn trim_chunk(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((idx, _)) => text[..idx].to_owned(),
        None => text.to_owned(),
    }
}

/// `function_call_output` of a successful retrieval.
#[must_use]
pub fn format_knowledge_output(chunks: &[crate::infra::llm::KnowledgeChunk], max_chunk_chars: usize) -> String {
    let results: Vec<serde_json::Value> = chunks
        .iter()
        .map(|c| {
            let mut v = serde_json::json!({ "text": trim_chunk(&c.text, max_chunk_chars) });
            if let Some(f) = &c.filename {
                v["filename"] = serde_json::json!(f);
            }
            if let Some(sc) = c.score {
                v["score"] = serde_json::json!(sc);
            }
            v
        })
        .collect();
    serde_json::json!({ "results": results }).to_string()
}

/// Everything the provider task needs.
pub struct LaunchSpec {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub selected_model: String,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub decision: PreflightDecision,
    pub planned: PlannedTurn,
    pub started: Instant,
}

// ── Service ────────────────────────────────────────────────────────────────

pub struct StreamService {
    deps: Arc<Deps>,
    quota: Arc<QuotaService>,
}

impl StreamService {
    #[must_use]
    pub fn new(deps: Arc<Deps>, quota: Arc<QuotaService>) -> Self {
        Self { deps, quota }
    }

    /// `POST /v1/chats/{id}/messages:stream` setup.
    ///
    /// # Errors
    /// Every pre-stream failure (JSON Problem): 403/503/404, 400 validation, 409, 429, 500.
    pub async fn send(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        input: SendInput,
    ) -> Result<StreamStart, DomainError> {
        let started = Instant::now();
        let deps = &self.deps;
        let ac = load_chat(deps, ctx, chat_id, actions::SEND_MESSAGE).await?;
        let (chat, scope, child_scope) = (ac.chat, ac.scope, ac.child_scope);
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();

        let request_id = input.request_id.unwrap_or_else(Uuid::new_v4);

        // Idempotency check first (replay / 409), then the parallel turn guard; validation
        // and model resolution run only for new turns.
        {
            let conn = deps.db.conn()?;
            if let Some(turn) = find_turn(&conn, &child_scope, chat.id, request_id).await? {
                if turn.state == "completed" && turn.deleted_at.is_none() {
                    return replay(&conn, &child_scope, &chat, &turn)
                        .await
                        .map(StreamStart::Replay);
                }
                return Err(request_id_conflict());
            }
            if find_running_turn(&conn, &child_scope, chat.id)
                .await?
                .is_some()
            {
                return Err(turn_already_running());
            }
        }

        let (snapshot, _) = deps
            .policy
            .resolve_model(user_id, &chat.model, false)
            .await?;
        if input.content.trim().is_empty() {
            return Err(empty_content());
        }
        validate_attachment_ids(&input.attachment_ids, &deps.cfg.rag)?;

        let (images, gathered) = {
            let conn = deps.db.conn()?;
            if input.web_search && snapshot.kill_switches.disable_web_search {
                return Err(DomainError::feature_disabled("web_search"));
            }
            let images = load_images(&conn, &child_scope, chat.id, &input.attachment_ids).await?;
            let gathered = gather(&conn, &child_scope, chat.id, None).await?;
            (images, gathered)
        };

        let decision = self
            .preflight_checks(
                tenant_id,
                user_id,
                &chat,
                &input.content,
                &images,
                input.web_search,
                gathered,
            )
            .await?;
        let planned = self
            .plan_turn(
                tenant_id,
                user_id,
                &chat,
                &child_scope,
                request_id,
                &input.content,
                &images,
                &decision,
            )
            .await?;

        let r = SendReserve {
            tenant_id,
            user_id,
            chat_id: chat.id,
            scope,
            child_scope: child_scope.clone(),
            request_id,
            turn_id: Uuid::new_v4(),
            content: input.content.clone(),
            attachment_ids: input.attachment_ids.clone(),
            decision: decision.clone(),
            web_search_enabled: input.web_search,
        };
        let turn_id = r.turn_id;
        let r = Arc::new(r);
        let res = retry_locked(|| {
            let quota = Arc::clone(&self.quota);
            let r = Arc::clone(&r);
            deps.db
                .transaction(move |tx| Box::pin(async move { reserve_send(tx, &quota, &r).await }))
        })
        .await;
        if let Err(e) = res {
            if e.is_unique_violation() {
                let conn = deps.db.conn()?;
                if find_turn(&conn, &child_scope, chat.id, request_id)
                    .await?
                    .is_some()
                {
                    return Err(request_id_conflict());
                }
                return Err(turn_already_running());
            }
            return Err(e);
        }

        Ok(StreamStart::Live(self.launch(LaunchSpec {
            tenant_id,
            user_id,
            chat_id: chat.id,
            selected_model: chat.model.clone(),
            turn_id,
            request_id,
            decision,
            planned,
            started,
        })))
    }

    /// Image count limit, quota preflight, then the `INPUT_TOO_LONG` check and the image
    /// guards (`disable_images`, vision support of the effective model).
    ///
    /// # Errors
    /// 400 `TOO_MANY_IMAGES` (before the preflight), 429 from the quota service, 400 input /
    /// image guards.
    #[allow(clippy::too_many_arguments)]
    pub async fn preflight_checks(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        chat: &chat::Model,
        content: &str,
        images: &[ImageRef],
        web_search_requested: bool,
        gathered: Gathered,
    ) -> Result<PreflightDecision, DomainError> {
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        if image_count > self.deps.cfg.rag.max_images_per_message {
            return Err(DomainError::out_of_range(
                resource_types::CHAT,
                "image_count",
                reasons::TOO_MANY_IMAGES,
                format!(
                    "At most {} images per message are allowed",
                    self.deps.cfg.rag.max_images_per_message
                ),
            ));
        }
        let decision = self
            .quota
            .preflight(&PreflightInput {
                tenant_id,
                user_id,
                selected_model: chat.model.clone(),
                message_bytes: content.len(),
                image_count,
                prior_context_tokens: gathered.prior_context_tokens,
                web_search_requested,
                has_ready_documents: gathered.has_ready_documents,
                has_ready_code_interpreter_files: gathered.has_ready_code_interpreter_files,
            })
            .await?;
        let eff = &decision.effective;
        let estimate = estimate_message_tokens(&eff.estimation_budgets, content, image_count);
        if eff.max_input_tokens > 0 && estimate > i64::from(eff.max_input_tokens) {
            return Err(DomainError::out_of_range(
                resource_types::CHAT,
                "content",
                reasons::INPUT_TOO_LONG,
                "The message exceeds the model's input token limit",
            ));
        }
        if image_count > 0 && decision.snapshot.kill_switches.disable_images {
            return Err(DomainError::feature_disabled("images"));
        }
        if image_count > 0 && !decision.vision_supported {
            return Err(DomainError::invalid(
                resource_types::CHAT,
                "content_type",
                reasons::VISION_NOT_SUPPORTED,
                "The model does not support image input",
            ));
        }
        Ok(decision)
    }

    /// Tools, context assembly, provider resolution and the provider request of a turn.
    ///
    /// # Errors
    /// 400 `CONTEXT_BUDGET_EXCEEDED`, 500 when the provider cannot be resolved, DB errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn plan_turn(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        chat: &chat::Model,
        child_scope: &AccessScope,
        request_id: Uuid,
        content: &str,
        images: &[ImageRef],
        decision: &PreflightDecision,
    ) -> Result<PlannedTurn, DomainError> {
        let deps = &self.deps;
        let cfg = &deps.cfg;
        let eff = &decision.effective;
        let conn = deps.db.conn()?;

        let mut tools = Vec::new();
        let mut sent = ToolGates::default();
        if decision.tools.file_search
            && let Some(vs) = vector_store_id(&conn, child_scope, chat.id).await?
        {
            tools.push(ToolSpec::FileSearch {
                vector_store_ids: vec![vs],
                max_num_results: eff.max_num_results,
            });
            sent.file_search = true;
        }
        if decision.tools.web_search {
            tools.push(ToolSpec::WebSearch {
                search_context_size: eff.web_search_context_size.clone(),
            });
            sent.web_search = true;
        }
        // Knowledge search: never together with file_search (file_search wins).
        let knowledge = if sent.file_search {
            None
        } else {
            knowledge_params(
                cfg,
                &deps.providers,
                deps.knowledge.is_some(),
                tenant_id,
            )
        };
        if decision.tools.code_interpreter {
            let file_ids = ready_attachments(
                &conn,
                child_scope,
                chat.id,
                attachment::Column::ForCodeInterpreter,
            )
            .await?
            .into_iter()
            .filter_map(|a| a.provider_file_id)
            .collect();
            tools.push(ToolSpec::CodeInterpreter { file_ids });
            sent.code_interpreter = true;
        }
        let builtin_tools = !tools.is_empty();
        if knowledge.is_some() {
            tools.push(search_knowledge_tool());
        }

        let mut parts: Vec<&str> = Vec::new();
        if !eff.system_prompt.trim().is_empty() {
            parts.push(&eff.system_prompt);
        }
        if sent.file_search && !cfg.context.file_search_guard.trim().is_empty() {
            parts.push(&cfg.context.file_search_guard);
        }
        if sent.web_search && !cfg.context.web_search_guard.trim().is_empty() {
            parts.push(&cfg.context.web_search_guard);
        }
        if knowledge.is_some() && !cfg.knowledge_search.guard.trim().is_empty() {
            parts.push(&cfg.knowledge_search.guard);
        }
        let instructions = parts.join("\n\n");

        let history = load_history(
            &conn,
            child_scope,
            chat.id,
            request_id,
            cfg.context.recent_messages_limit,
        )
        .await?;
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        let plan = assemble(&ContextInput {
            budgets: &eff.estimation_budgets,
            context_window: eff.context_window,
            max_input_tokens: eff.max_input_tokens,
            max_output_tokens_applied: decision.max_output_tokens_applied,
            tools: sent,
            instructions: &instructions,
            current_text: content,
            image_count,
            summary: history.summary.as_ref(),
            recent: &history.recent,
        })?;

        if deps
            .providers
            .resolve(&eff.provider_id, tenant_id)
            .is_none()
        {
            return Err(DomainError::internal(format!(
                "provider '{}' of model '{}' is not configured",
                eff.provider_id, eff.id
            )));
        }

        let file_map = if sent.file_search {
            any_ready_with_file(&conn, child_scope, chat.id)
                .await?
                .into_iter()
                .filter_map(|a| a.provider_file_id.clone().map(|f| (f, (a.id, a.filename))))
                .collect()
        } else {
            HashMap::new()
        };

        let mut input = plan.messages.clone();
        let mut current = vec![ContentPart::Text(content.to_owned())];
        current.extend(
            images
                .iter()
                .filter_map(|i| i.provider_file_id.clone())
                .map(|file_id| ContentPart::Image { file_id }),
        );
        input.push(InputMessage {
            role: InputRole::User,
            content: current,
        });

        let feature = if tools.is_empty() {
            "none".to_owned()
        } else {
            tools
                .iter()
                .map(ToolSpec::name)
                .collect::<Vec<_>>()
                .join("+")
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("tenant_id".to_owned(), tenant_id.to_string());
        metadata.insert("user_id".to_owned(), user_id.to_string());
        metadata.insert("chat_id".to_owned(), chat.id.to_string());
        metadata.insert("request_type".to_owned(), "chat".to_owned());
        metadata.insert("feature".to_owned(), feature);

        let max_tool_calls = builtin_tools.then_some(eff.max_tool_calls);
        let request = LlmRequest {
            provider_id: eff.provider_id.clone(),
            tenant_id,
            model: eff.provider_model_id.clone(),
            instructions,
            input,
            tool_exchanges: Vec::new(),
            tools,
            max_output_tokens: u32::try_from(decision.max_output_tokens_applied.max(0))
                .unwrap_or(0),
            max_tool_calls,
            api_params: eff.general_config.api_params.clone(),
            user: format!("{}{}", simple_uuid(tenant_id), simple_uuid(user_id)),
            metadata,
            request_type: RequestType::Chat,
            stream: true,
        };

        let ts = &cfg.thread_summary_worker;
        let summary_trigger = ts.enabled.then(|| SummaryTrigger {
            assembled_tokens: plan.assembled_tokens,
            effective_budget: input_limit(
                eff.context_window,
                eff.max_input_tokens,
                decision.max_output_tokens_applied,
            ),
            threshold_pct: ts.compression_threshold_pct,
            messages_truncated: plan.messages_truncated,
        });

        Ok(PlannedTurn {
            request,
            plan,
            tools: sent,
            file_map,
            summary_trigger,
            knowledge,
        })
    }

    /// Opens the stream of a committed running turn: queues `stream_started` and spawns the
    /// provider task.
    #[must_use]
    pub fn launch(&self, spec: LaunchSpec) -> LiveStream {
        let deps = &self.deps;
        let cap = usize::from(deps.cfg.streaming.sse_channel_capacity).max(2);
        let (tx, rx) = mpsc::channel(cap);
        let assistant_message_id = Uuid::new_v4();
        let started =
            MiniChatSseEvent::StreamStarted(StreamStartedData {
                request_id: spec.request_id,
                message_id: assistant_message_id,
                is_new_turn: true,
                thread_summary_applied: spec.planned.plan.summary_applied.map(|t| {
                    ThreadSummaryInfo {
                        token_estimate: u32::try_from(t.max(0)).unwrap_or(0),
                    }
                }),
            });
        // The channel is empty and has capacity >= 2.
        let _ = tx.try_send(started);
        let cancel = CancellationToken::new();
        let turn = TurnContext {
            tenant_id: spec.tenant_id,
            user_id: spec.user_id,
            chat_id: spec.chat_id,
            turn_id: spec.turn_id,
            request_id: spec.request_id,
            selected_model: spec.selected_model.clone(),
            effective_model: spec.decision.effective.id.clone(),
            downgrade_reason: spec.decision.downgrade_reason,
            periods: spec.decision.periods,
            started: spec.started,
            summary_trigger: spec.planned.summary_trigger,
        };
        let task = TurnTask {
            deps: Arc::clone(deps),
            quota: Arc::clone(&self.quota),
            turn,
            request: Some(spec.planned.request),
            file_map: spec.planned.file_map,
            knowledge: spec.planned.knowledge,
            assistant_message_id,
            tx,
            cancel: cancel.clone(),
            text: String::new(),
            counters: ToolCounters::default(),
            web_search_started: 0,
            code_interpreter_started: 0,
            content_started: false,
            last_refresh: Instant::now(),
        };
        deps.tasks.spawn(task.run());
        LiveStream {
            rx,
            guard: cancel.drop_guard(),
        }
    }
}

// ── Validation and lookups ─────────────────────────────────────────────────

/// Duplicate ids or more than `max_documents_per_chat + max_images_per_message`.
///
/// # Errors
/// 400 `invalid_attachment`.
pub fn validate_attachment_ids(ids: &[Uuid], rag: &RagConfig) -> Result<(), DomainError> {
    let max = u64::from(rag.max_documents_per_chat) + u64::from(rag.max_images_per_message);
    if u64::try_from(ids.len()).unwrap_or(u64::MAX) > max {
        return Err(invalid_attachment());
    }
    let mut seen = std::collections::HashSet::new();
    if !ids.iter().all(|id| seen.insert(*id)) {
        return Err(invalid_attachment());
    }
    Ok(())
}

/// Turn by `(chat_id, request_id)` (any state, deleted or not).
///
/// # Errors
/// Database failure.
pub async fn find_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Non-deleted running turn of the chat.
///
/// # Errors
/// Database failure.
pub async fn find_running_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::State.eq("running"))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Image attachments of this chat among `ids` (non-deleted).
///
/// # Errors
/// Database failure.
pub async fn load_images(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<ImageRef>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(ids.to_vec()))
                .add(attachment::Column::AttachmentKind.eq("image"))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    // Keep the request order.
    Ok(ids
        .iter()
        .filter_map(|id| rows.iter().find(|a| a.id == *id))
        .map(ImageRef::from)
        .collect())
}

async fn ready_attachments(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    purpose: attachment::Column,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Status.eq("ready"))
                .add(attachment::Column::DeletedAt.is_null())
                .add(purpose.eq(true)),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Ready, non-deleted attachments with a provider file id (citation map source).
async fn any_ready_with_file(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Status.eq("ready"))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::ProviderFileId.is_not_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

async fn vector_store_id(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<String>, DomainError> {
    Ok(chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?
        .and_then(|v| v.vector_store_id)
        .filter(|v| !v.is_empty()))
}

/// Documents / code-interpreter files / prior context tokens of the chat. Messages of
/// `exclude_request_id` (the mutated turn) do not count as prior context.
///
/// # Errors
/// Database failure.
pub async fn gather(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    exclude_request_id: Option<Uuid>,
) -> Result<Gathered, DomainError> {
    let has_docs = !ready_attachments(runner, scope, chat_id, attachment::Column::ForFileSearch)
        .await?
        .is_empty();
    let has_ci = !ready_attachments(
        runner,
        scope,
        chat_id,
        attachment::Column::ForCodeInterpreter,
    )
    .await?
    .is_empty();
    let mut cond = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::Role.eq("assistant"))
        .add(message::Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(message::Column::InputTokens.gt(0))
                .add(message::Column::OutputTokens.gt(0)),
        );
    if let Some(rid) = exclude_request_id {
        cond = cond.add(message::Column::RequestId.ne(rid));
    }
    let prior = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(cond)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?
        .map_or(0, |m| m.input_tokens + m.output_tokens);
    Ok(Gathered {
        has_ready_documents: has_docs,
        has_ready_code_interpreter_files: has_ci,
        prior_context_tokens: prior,
    })
}

// ── Replay ─────────────────────────────────────────────────────────────────

/// Replay events of a completed turn (no provider call, no writes).
#[must_use]
pub fn replay_events(
    chat: &chat::Model,
    turn: &chat_turn::Model,
    msg: &message::Model,
) -> Vec<MiniChatSseEvent> {
    let effective = msg
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_default();
    let downgrade = effective != chat.model;
    vec![
        MiniChatSseEvent::StreamStarted(StreamStartedData {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        MiniChatSseEvent::Delta(DeltaData {
            kind: DeltaKind::Text,
            content: msg.content.clone(),
        }),
        MiniChatSseEvent::Done(DoneData {
            usage: Usage {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
            },
            effective_model: effective,
            selected_model: chat.model.clone(),
            quota_decision: if downgrade {
                QuotaDecisionKind::Downgrade
            } else {
                QuotaDecisionKind::Allow
            },
            downgrade_from: downgrade.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        }),
    ]
}

async fn replay(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat: &chat::Model,
    turn: &chat_turn::Model,
) -> Result<Vec<MiniChatSseEvent>, DomainError> {
    let mid = turn
        .assistant_message_id
        .ok_or_else(|| DomainError::internal("completed turn without assistant message"))?;
    let msg = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat.id))
        .secure()
        .scope_with(scope)
        .and_id(mid)?
        .one(runner)
        .await?
        .ok_or_else(|| DomainError::internal("assistant message of a completed turn is missing"))?;
    Ok(replay_events(chat, turn, &msg))
}

// ── Reserve transaction (send) ─────────────────────────────────────────────

struct SendReserve {
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    scope: AccessScope,
    child_scope: AccessScope,
    request_id: Uuid,
    turn_id: Uuid,
    content: String,
    attachment_ids: Vec<Uuid>,
    decision: PreflightDecision,
    web_search_enabled: bool,
}

/// New `running` turn row (preflight columns from `decision`, or NULL for retry/edit).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn new_running_turn(
    tenant_id: Uuid,
    chat_id: Uuid,
    turn_id: Uuid,
    request_id: Uuid,
    user_id: Uuid,
    decision: Option<&PreflightDecision>,
    web_search_enabled: bool,
    now: OffsetDateTime,
) -> chat_turn::ActiveModel {
    chat_turn::ActiveModel {
        id: Set(turn_id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(user_id)),
        state: Set("running".to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(decision.map(|d| d.reserve_tokens)),
        max_output_tokens_applied: Set(decision.map(|d| d.max_output_tokens_applied)),
        reserved_credits_micro: Set(decision.map(|d| d.reserved_credits_micro)),
        policy_version_applied: Set(
            decision.map(|d| i64::try_from(d.policy_version).unwrap_or(i64::MAX))
        ),
        effective_model: Set(decision.map(|d| d.effective.id.clone())),
        minimal_generation_floor_applied: Set(decision.map(|d| d.minimal_generation_floor_applied)),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(now),
        last_progress_at: Set(Some(now)),
        web_search_enabled: Set(web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(now),
    }
}

/// Bumps `chats.updated_at` (owner scope).
///
/// # Errors
/// Database failure.
pub async fn touch_chat(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Inserts a `message_attachments` link.
///
/// # Errors
/// Database failure.
pub async fn link_attachment(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = message_attachment::ActiveModel {
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(now),
    };
    secure_insert::<message_attachment::Entity>(am, scope, tx).await?;
    Ok(())
}

async fn reserve_send(
    tx: &DbTx<'_>,
    quota: &QuotaService,
    r: &SendReserve,
) -> Result<(), DomainError> {
    let now = OffsetDateTime::now_utc();
    quota
        .reserve_in_tx(tx, r.tenant_id, r.user_id, &r.decision)
        .await?;
    let user_message_id = Uuid::new_v4();
    secure_insert::<message::Entity>(
        user_message(
            r.tenant_id,
            r.chat_id,
            user_message_id,
            r.request_id,
            r.content.clone(),
            now,
        ),
        &r.child_scope,
        tx,
    )
    .await?;
    touch_chat(tx, &r.scope, r.chat_id, now).await?;
    for id in &r.attachment_ids {
        let a = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(r.chat_id))
                    .add(attachment::Column::UploadedByUserId.eq(r.user_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::Status.eq("ready")),
            )
            .secure()
            .scope_with(&r.child_scope)
            .and_id(*id)?
            .one(tx)
            .await?;
        if a.is_none() {
            return Err(invalid_attachment());
        }
        link_attachment(
            tx,
            &r.child_scope,
            r.tenant_id,
            r.chat_id,
            user_message_id,
            *id,
            now,
        )
        .await?;
    }
    secure_insert::<chat_turn::Entity>(
        new_running_turn(
            r.tenant_id,
            r.chat_id,
            r.turn_id,
            r.request_id,
            r.user_id,
            Some(&r.decision),
            r.web_search_enabled,
            now,
        ),
        &r.child_scope,
        tx,
    )
    .await?;
    Ok(())
}

// ── Citations ──────────────────────────────────────────────────────────────

/// Maps provider citations to client citations (unknown / deleted files omitted).
#[must_use]
pub fn map_citations(
    raw: &[RawCitation],
    file_map: &HashMap<String, (Uuid, String)>,
) -> Vec<Citation> {
    raw.iter()
        .filter_map(|c| match c {
            RawCitation::Web {
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
                span: span.map(|(start, end)| TextSpan { start, end }),
            }),
            RawCitation::File { file_id, .. } => {
                file_map.get(file_id).map(|(id, filename)| Citation {
                    source: CitationSource::File,
                    title: filename.clone(),
                    url: None,
                    attachment_id: Some(*id),
                    snippet: String::new(),
                    span: None,
                })
            }
        })
        .collect()
}

// ── Provider task ──────────────────────────────────────────────────────────

enum Step {
    Cancelled,
    Ping,
    Event(Option<LlmEvent>),
}

/// Result of opening a provider stream.
enum Opened {
    Stream(futures::stream::BoxStream<'static, LlmEvent>),
    Cancelled,
    Failed(crate::infra::llm::LlmFailure),
}

/// Result of handling the function calls of a provider request.
enum CallsOutcome {
    /// Retrievals done: issue the next provider request with these exchanges appended.
    Continue(Vec<ToolExchange>),
    /// Finalize the turn as failed with `(code, message)`.
    Fail(&'static str, &'static str),
    Cancelled,
}

struct TurnTask {
    deps: Arc<Deps>,
    quota: Arc<QuotaService>,
    turn: TurnContext,
    request: Option<LlmRequest>,
    file_map: HashMap<String, (Uuid, String)>,
    knowledge: Option<KnowledgeParams>,
    assistant_message_id: Uuid,
    tx: mpsc::Sender<MiniChatSseEvent>,
    cancel: CancellationToken,
    text: String,
    counters: ToolCounters,
    web_search_started: u32,
    code_interpreter_started: u32,
    content_started: bool,
    last_refresh: Instant,
}

impl TurnTask {
    fn ping_every(&self) -> Duration {
        Duration::from_secs(u64::from(
            self.deps.cfg.streaming.sse_ping_interval_seconds.max(1),
        ))
    }

    /// Opens a provider stream; sends `ping` while waiting (before the first delta / tool).
    async fn open(&self, req: LlmRequest) -> Opened {
        let llm = Arc::clone(&self.deps.llm);
        let cancel = self.cancel.clone();
        let ping_every = self.ping_every();
        let fut = llm.stream(req, cancel.clone());
        tokio::pin!(fut);
        loop {
            let res = if self.content_started {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Opened::Cancelled,
                    r = &mut fut => r,
                }
            } else {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Opened::Cancelled,
                    r = &mut fut => r,
                    () = tokio::time::sleep(ping_every) => {
                        if !self.emit(MiniChatSseEvent::Ping(PingData {})).await {
                            return Opened::Cancelled;
                        }
                        continue;
                    }
                }
            };
            return match res {
                Ok(s) => Opened::Stream(s),
                Err(f) => Opened::Failed(f),
            };
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run(mut self) {
        let cancel = self.cancel.clone();
        let Some(base) = self.request.take() else {
            return;
        };
        let ping_every = self.ping_every();
        let quota_cfg = self.deps.cfg.quota.clone();
        let mut exchanges: Vec<ToolExchange> = Vec::new();
        let mut iterations: u32 = 0;
        loop {
            iterations += 1;
            let mut req = base.clone();
            req.tool_exchanges.clone_from(&exchanges);
            let mut stream = match self.open(req).await {
                Opened::Stream(s) => s,
                Opened::Cancelled => return self.finish_cancelled().await,
                Opened::Failed(f) => {
                    return self
                        .finish_failed(f.code, &f.message, f.usage, f.response_id)
                        .await;
                }
            };
            let mut calls: Vec<(String, String, String)> = Vec::new();
            let completion = loop {
                let step = if self.content_started {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => Step::Cancelled,
                        ev = stream.next() => Step::Event(ev),
                    }
                } else {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => Step::Cancelled,
                        ev = stream.next() => Step::Event(ev),
                        () = tokio::time::sleep(ping_every) => Step::Ping,
                    }
                };
                let ev = match step {
                    Step::Cancelled => {
                        drop(stream);
                        return self.finish_cancelled().await;
                    }
                    Step::Ping => {
                        if !self.emit(MiniChatSseEvent::Ping(PingData {})).await {
                            drop(stream);
                            return self.finish_cancelled().await;
                        }
                        continue;
                    }
                    Step::Event(None) => {
                        drop(stream);
                        if cancel.is_cancelled() {
                            return self.finish_cancelled().await;
                        }
                        return self
                            .finish_failed(
                                stream_codes::PROVIDER_ERROR,
                                "Provider stream ended without a terminal event",
                                None,
                                None,
                            )
                            .await;
                    }
                    Step::Event(Some(ev)) => ev,
                };
                let delivered = match ev {
                    LlmEvent::TextDelta(s) => {
                        self.content_started = true;
                        self.text.push_str(&s);
                        self.emit(MiniChatSseEvent::Delta(DeltaData {
                            kind: DeltaKind::Text,
                            content: s,
                        }))
                        .await
                    }
                    LlmEvent::ReasoningDelta(s) => {
                        self.content_started = true;
                        self.emit(MiniChatSseEvent::Delta(DeltaData {
                            kind: DeltaKind::Reasoning,
                            content: s,
                        }))
                        .await
                    }
                    LlmEvent::ToolStart { name, details } => {
                        self.content_started = true;
                        if name == "web_search" {
                            self.web_search_started += 1;
                            if self.web_search_started > quota_cfg.web_search_max_calls_per_message
                            {
                                cancel.cancel();
                                drop(stream);
                                return self
                                    .finish_failed(
                                        stream_codes::WEB_SEARCH_CALLS_EXCEEDED,
                                        "The web search call limit per message was exceeded",
                                        None,
                                        None,
                                    )
                                    .await;
                            }
                        } else if name == "code_interpreter" {
                            self.code_interpreter_started += 1;
                            if self.code_interpreter_started
                                > quota_cfg.code_interpreter_max_calls_per_message
                            {
                                cancel.cancel();
                                drop(stream);
                                return self
                                    .finish_failed(
                                        stream_codes::CODE_INTERPRETER_CALLS_EXCEEDED,
                                        "The code interpreter call limit per message was exceeded",
                                        None,
                                        None,
                                    )
                                    .await;
                            }
                        }
                        self.emit(MiniChatSseEvent::Tool(ToolData {
                            phase: ToolPhase::Start,
                            name,
                            details,
                        }))
                        .await
                    }
                    LlmEvent::ToolDone { name, details } => {
                        self.content_started = true;
                        match name.as_str() {
                            "web_search" => self.counters.web_search += 1,
                            "code_interpreter" => self.counters.code_interpreter += 1,
                            "file_search" => self.counters.file_search += 1,
                            _ => {}
                        }
                        self.emit(MiniChatSseEvent::Tool(ToolData {
                            phase: ToolPhase::Done,
                            name,
                            details,
                        }))
                        .await
                    }
                    LlmEvent::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    } => {
                        calls.push((call_id, name, arguments));
                        true
                    }
                    LlmEvent::Completed(c) => break c,
                    LlmEvent::Failed(f) => {
                        drop(stream);
                        return self
                            .finish_failed(f.code, &f.message, f.usage, f.response_id)
                            .await;
                    }
                };
                if !delivered {
                    // Failed channel send: the client disconnected.
                    cancel.cancel();
                    drop(stream);
                    return self.finish_cancelled().await;
                }
                self.maybe_refresh_progress().await;
            };
            drop(stream);
            if calls.is_empty() {
                return self.finish_completed(completion).await;
            }
            match self.handle_function_calls(calls, iterations).await {
                CallsOutcome::Continue(new) => exchanges.extend(new),
                CallsOutcome::Fail(code, message) => {
                    return self
                        .finish_failed(code, message, completion.usage, completion.response_id)
                        .await;
                }
                CallsOutcome::Cancelled => return self.finish_cancelled().await,
            }
        }
    }

    /// Function calls that ended a provider request: `search_knowledge` retrievals (agentic
    /// loop), or `unexpected_tool_use` / `agentic_iterations_exceeded`.
    async fn handle_function_calls(
        &mut self,
        calls: Vec<(String, String, String)>,
        iterations: u32,
    ) -> CallsOutcome {
        let Some(k) = self.knowledge.clone() else {
            tracing::warn!(turn_id = %self.turn.turn_id, "function call while knowledge search is off");
            return CallsOutcome::Fail(
                stream_codes::UNEXPECTED_TOOL_USE,
                "The model requested an unsupported tool",
            );
        };
        if calls.iter().any(|(_, name, _)| name != SEARCH_KNOWLEDGE) {
            tracing::warn!(turn_id = %self.turn.turn_id, "model requested an unknown function tool");
            return CallsOutcome::Fail(
                stream_codes::UNEXPECTED_TOOL_USE,
                "The model requested an unsupported tool",
            );
        }
        if iterations >= k.max_calls_per_message.saturating_add(2) {
            return CallsOutcome::Fail(
                stream_codes::AGENTIC_ITERATIONS_EXCEEDED,
                "The knowledge search iteration limit was exceeded",
            );
        }
        let mut out = Vec::with_capacity(calls.len());
        for (call_id, name, arguments) in calls {
            let output = if self.counters.knowledge_search_calls >= k.max_calls_per_message {
                SEARCH_LIMIT_REACHED.to_owned()
            } else {
                self.counters.knowledge_search_calls += 1;
                let cancel = self.cancel.clone();
                let res = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return CallsOutcome::Cancelled,
                    r = self.retrieve(&k, &arguments) => r,
                };
                match res {
                    Ok(text) => {
                        self.counters.file_search += 1;
                        text
                    }
                    Err(text) => text,
                }
            };
            out.push(ToolExchange {
                call_id,
                name,
                arguments,
                output,
            });
        }
        self.last_refresh = Instant::now();
        if let Err(e) = self.refresh_progress().await {
            tracing::warn!(turn_id = %self.turn.turn_id, error = %e, "progress refresh failed");
        }
        CallsOutcome::Continue(out)
    }

    /// Runs one knowledge retrieval; `Err` carries the output returned to the model.
    async fn retrieve(&self, k: &KnowledgeParams, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
        let Some(query) = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
        else {
            return Err("Invalid search_knowledge arguments: a non-empty `query` is required.".to_owned());
        };
        let top_k = args
            .get("top_k")
            .and_then(serde_json::Value::as_u64)
            .map_or(k.top_k, |n| usize::try_from(n).unwrap_or(usize::MAX))
            .clamp(1, k.top_k.max(1));
        let Some(retriever) = self.deps.knowledge.clone() else {
            return Err("Knowledge search is unavailable.".to_owned());
        };
        match retriever
            .search(
                &k.provider_id,
                self.turn.tenant_id,
                &k.vector_store_id,
                query,
                top_k,
            )
            .await
        {
            Ok(chunks) => {
                tracing::debug!(turn_id = %self.turn.turn_id, chunks = chunks.len(), "knowledge search completed");
                Ok(format_knowledge_output(&chunks, k.max_chunk_chars))
            }
            Err(e) => {
                tracing::warn!(turn_id = %self.turn.turn_id, error = %e, "knowledge search failed");
                Err("Knowledge search failed. Answer without the knowledge base.".to_owned())
            }
        }
    }

    async fn emit(&self, ev: MiniChatSseEvent) -> bool {
        self.tx.send(ev).await.is_ok()
    }

    async fn maybe_refresh_progress(&mut self) {
        if self.last_refresh.elapsed() < PROGRESS_REFRESH {
            return;
        }
        self.last_refresh = Instant::now();
        if let Err(e) = self.refresh_progress().await {
            tracing::warn!(turn_id = %self.turn.turn_id, error = %e, "progress refresh failed");
        }
    }

    async fn refresh_progress(&self) -> Result<(), DomainError> {
        let now = OffsetDateTime::now_utc();
        let conn = self.deps.db.conn()?;
        let c = self.counters;
        chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                chat_turn::Column::WebSearchCompletedCount,
                Expr::value(i32::try_from(c.web_search).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turn::Column::CodeInterpreterCompletedCount,
                Expr::value(i32::try_from(c.code_interpreter).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turn::Column::FileSearchCompletedCount,
                Expr::value(i32::try_from(c.file_search).unwrap_or(i32::MAX)),
            )
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(self.turn.turn_id))
                    .add(chat_turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(self.turn.tenant_id))
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn finish_cancelled(self) {
        self.cancel.cancel();
        let terminal = Terminal::Cancelled {
            assistant_message_id: self.assistant_message_id,
            text: self.text.clone(),
        };
        match finalization::finalize(&self.deps, &self.quota, &self.turn, self.counters, terminal)
            .await
        {
            Ok(o) => tracing::debug!(turn_id = %self.turn.turn_id, outcome = ?o, "turn cancelled"),
            Err(e) => {
                tracing::error!(turn_id = %self.turn.turn_id, error = %e, "cancel finalization failed")
            }
        }
    }

    async fn finish_failed(
        self,
        code: &str,
        message: &str,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
    ) {
        let message = sanitize_provider_message(message);
        let terminal = Terminal::Failed {
            code: code.to_owned(),
            detail: message.clone(),
            usage,
            response_id,
        };
        let ev = match finalization::finalize(
            &self.deps,
            &self.quota,
            &self.turn,
            self.counters,
            terminal,
        )
        .await
        {
            Ok(FinalizeOutcome::Committed { .. }) => error_event(code, &message),
            Ok(FinalizeOutcome::Lost) => error_event(
                stream_codes::STREAM_INTERRUPTED,
                "The stream was interrupted",
            ),
            Err(e) => {
                tracing::error!(turn_id = %self.turn.turn_id, error = %e, "failure finalization failed");
                error_event(code, &message)
            }
        };
        let _ = self.tx.send(ev).await;
    }

    async fn finish_completed(self, c: LlmCompletion) {
        // The stored assistant content is the accumulated delta text only.
        let text = self.text.clone();
        let usage = c.usage.unwrap_or_default();
        let incomplete = c.incomplete_reason.is_some();
        let citations = map_citations(&c.citations, &self.file_map);
        let terminal = Terminal::Completed {
            assistant_message_id: self.assistant_message_id,
            text,
            completion: c,
        };
        let res =
            finalization::finalize(&self.deps, &self.quota, &self.turn, self.counters, terminal)
                .await;
        match res {
            Ok(FinalizeOutcome::Committed {
                state: TerminalState::Completed,
                ..
            }) => {
                if !incomplete
                    && !citations.is_empty()
                    && self
                        .tx
                        .send(MiniChatSseEvent::Citations(CitationsData {
                            items: citations,
                        }))
                        .await
                        .is_err()
                {
                    return;
                }
                let warnings = match self
                    .quota
                    .quota_warnings(self.turn.tenant_id, self.turn.user_id)
                    .await
                {
                    Ok(w) => Some(w),
                    Err(e) => {
                        tracing::warn!(error = %e, "quota warnings unavailable");
                        None
                    }
                };
                let downgrade = self.turn.downgrade_reason;
                let done = DoneData {
                    usage: Usage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    },
                    effective_model: self.turn.effective_model.clone(),
                    selected_model: self.turn.selected_model.clone(),
                    quota_decision: if downgrade.is_some() {
                        QuotaDecisionKind::Downgrade
                    } else {
                        QuotaDecisionKind::Allow
                    },
                    downgrade_from: downgrade.map(|_| self.turn.selected_model.clone()),
                    downgrade_reason: downgrade.map(|r| r.as_str().to_owned()),
                    quota_warnings: warnings,
                };
                let _ = self.tx.send(MiniChatSseEvent::Done(done)).await;
            }
            Ok(FinalizeOutcome::Committed { error_code, .. }) => {
                let code = error_code
                    .unwrap_or_else(|| stream_codes::MESSAGE_PERSISTENCE_FAILED.to_owned());
                let _ = self
                    .tx
                    .send(error_event(&code, "The response could not be saved"))
                    .await;
            }
            Ok(FinalizeOutcome::Lost) => {
                let _ = self
                    .tx
                    .send(error_event(
                        stream_codes::STREAM_INTERRUPTED,
                        "The stream was interrupted",
                    ))
                    .await;
            }
            Err(e) => {
                tracing::error!(turn_id = %self.turn.turn_id, error = %e, "finalization failed");
                let _ = self
                    .tx
                    .send(error_event(
                        stream_codes::FINALIZATION_FAILED,
                        "The response could not be finalized",
                    ))
                    .await;
            }
        }
    }
}

#[cfg(test)]
#[path = "stream_test_helpers.rs"]
pub(crate) mod test_helpers;

#[cfg(test)]
#[path = "stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "stream_knowledge_tests.rs"]
mod stream_knowledge_tests;
