//! Send-message streaming pipeline (spec §8; DESIGN §3.3 "Streaming Contract",
//! §3.6 "Send Message with Streaming Response", §4 "Turn Lifecycle", §5.7).
//!
//! [`StreamService::send`] runs the ordered setup steps (validation → authz and
//! chat → model → idempotency / replay → parallel guard → preflight → turn
//! preparation) and returns either a replay or a live stream. The live stream is
//! fed by a spawned provider task ([`provider_task`]) through a bounded channel;
//! [`relay`] turns it into the SSE event sequence. Retry and edit (turn
//! mutations) reuse [`StreamService::preflight`], [`StreamService::prepare_turn`]
//! and [`StreamService::spawn_turn`].

mod citations;
mod preflight;
mod provider_task;
pub mod relay;
mod replay;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{ModelCatalogEntry, UserLimits};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub use preflight::{PreflightRequest, Preflighted};
pub use relay::{LiveStream, live_events};

use crate::api::rest::dto::{StreamStartedData, ThreadSummaryInfo};
use crate::api::rest::sse::SseEvent;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{AttachmentStatus, MessageRole, TurnState};
use crate::domain::sanitize::user_field;
use crate::domain::services::chat::ChatService;
use crate::domain::services::context::{
    ContextInput, ContextPlan, HistoryMessage, ToolSet, assemble, guards_for,
};
use crate::domain::services::finalization::FinalizationService;
use crate::domain::services::model_catalog::ModelCatalogService;
use crate::domain::services::quota::{PreflightDecision, QuotaService};
use crate::infra::db::entities::{attachment, chat, chat_turn, message};
use crate::infra::db::repos::turn::{NewTurn, PreflightFields};
use crate::infra::db::repos::{AttachmentRepo, ChatRepo, MessageRepo, ThreadSummaryRepo, TurnRepo};
use crate::infra::db::tx::with_tx_retry;
use crate::infra::gateways::model_policy::ModelPolicyGateway;
use crate::infra::llm::{
    KnowledgeSearch, KnowledgeTurn, LlmClient, LlmRequest, LlmTool, ProviderResolver,
    RequestMetadata, RequestType, ResolvedProvider,
};

/// A send-message request after JSON decoding.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled`.
    pub web_search: bool,
}

/// Outcome of a successful setup.
#[derive(Debug)]
pub enum StreamStart {
    /// A new generation.
    Live(LiveStream),
    /// Idempotent replay of a completed turn (complete event list).
    Replay(Vec<SseEvent>),
}

/// How the reserve transaction of [`StreamService::prepare_turn`] treats the turn.
#[allow(clippy::large_enum_variant, reason = "built once per turn")]
#[derive(Debug, Clone)]
pub enum TurnMode {
    /// Send path: the reserve transaction inserts the running turn and the user
    /// message, validates and links `attachment_ids`, and bumps the chat.
    New { attachment_ids: Vec<Uuid> },
    /// Retry/edit: the mutation commit already inserted the running turn (with
    /// NULL preflight columns) and the user message; the reserve transaction
    /// fills the preflight columns. Setup errors are returned to the caller, which
    /// marks the turn failed.
    Existing { turn: chat_turn::Model },
}

/// Input of [`StreamService::prepare_turn`].
#[derive(Debug, Clone)]
pub struct TurnPrep {
    /// The (authorized, live) chat; its `tenant_id` scopes every query.
    pub chat: chat::Model,
    pub user_id: Uuid,
    /// Text of the current user message.
    pub content: String,
    pub request_id: Uuid,
    /// `web_search.enabled` of the request (stored on a new turn).
    pub web_search_requested: bool,
    /// Result of [`StreamService::preflight`].
    pub pre: Preflighted,
    pub mode: TurnMode,
}

/// A turn ready to stream: the running turn row with its reserve, the provider
/// request and everything the provider task needs to finalize it.
#[derive(Debug, Clone)]
pub struct PreparedTurn {
    /// The running turn with its preflight columns set.
    pub turn: chat_turn::Model,
    /// Selected model (`chats.model`).
    pub chat_model: String,
    pub user_id: Uuid,
    pub plan: ContextPlan,
    /// `stream_started.thread_summary_applied.token_estimate`.
    pub thread_summary_applied: Option<u32>,
    /// The chat had a thread summary at context assembly (thread-summary trigger).
    pub has_summary: bool,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    /// `provider_file_id -> (attachment_id, filename)`; empty unless
    /// `file_search` is sent.
    pub citation_map: HashMap<String, (Uuid, String)>,
    /// Preflight decision (effective model, quota decision, periods).
    pub decision: PreflightDecision,
    /// Pre-allocated assistant message id (`stream_started.message_id`).
    pub message_id: Uuid,
    /// Knowledge-search parameters when `search_knowledge` is offered.
    pub knowledge: Option<KnowledgeTurn>,
    /// Start of the turn (audit `latency_ms`).
    pub started: Instant,
}

/// What the send-path reserve transaction writes besides the reserve.
struct NewTurnRows {
    user_id: Uuid,
    request_id: Uuid,
    web_search_enabled: bool,
    content: String,
    attachment_ids: Vec<Uuid>,
    fields: PreflightFields,
}

/// Infrastructure of [`StreamService`].
pub struct StreamDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<ChatAuthz>,
    pub chats: Arc<ChatService>,
    pub models: Arc<ModelCatalogService>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub quota: Arc<QuotaService>,
    pub finalization: Arc<FinalizationService>,
    pub llm: Arc<LlmClient>,
    pub providers: Arc<ProviderResolver>,
    /// Knowledge search (only when `knowledge_search.enabled`).
    pub knowledge: Option<Arc<KnowledgeSearch>>,
}

/// The send-message pipeline.
pub struct StreamService {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    chats: Arc<ChatService>,
    models: Arc<ModelCatalogService>,
    policy: Arc<dyn ModelPolicyGateway>,
    quota: Arc<QuotaService>,
    finalization: Arc<FinalizationService>,
    llm: Arc<LlmClient>,
    providers: Arc<ProviderResolver>,
    knowledge: Option<Arc<KnowledgeSearch>>,
}

/// `attachment_ids` must be unique and at most `max_documents_per_chat +
/// max_images_per_message` long (step 1).
fn validate_attachment_ids(ids: &[Uuid], cfg: &MiniChatConfig) -> DomainResult<()> {
    let mut seen = HashSet::with_capacity(ids.len());
    if !ids.iter().all(|id| seen.insert(*id)) {
        return Err(DomainError::InvalidAttachment(
            "Attachment ids must be unique".to_owned(),
        ));
    }
    let max = u64::from(cfg.rag.max_documents_per_chat) + u64::from(cfg.rag.max_images_per_message);
    if u64::try_from(ids.len()).unwrap_or(u64::MAX) > max {
        return Err(DomainError::InvalidAttachment(format!(
            "At most {max} attachments can be referenced by one message"
        )));
    }
    Ok(())
}

/// Every id of `ids` must be a live, ready attachment of the chat uploaded by
/// `user_id` (`rows` are the chat's attachments among `ids`).
fn check_attachments(ids: &[Uuid], rows: &[attachment::Model], user_id: Uuid) -> DomainResult<()> {
    for id in ids {
        let usable = rows.iter().any(|a| {
            a.id == *id
                && a.deleted_at.is_none()
                && a.uploaded_by_user_id == user_id
                && a.status == AttachmentStatus::Ready.as_str()
        });
        if !usable {
            return Err(DomainError::InvalidAttachment(format!(
                "Attachment {id} is not a ready attachment of this chat"
            )));
        }
    }
    Ok(())
}

/// Tools of the turn for the effective model (step 11); `search_knowledge`
/// only with knowledge-search parameters and without `file_search` (DESIGN §4
/// "Knowledge Search", mutual exclusion).
fn tool_set(pre: &Preflighted, knowledge: bool) -> ToolSet {
    let d = &pre.decision;
    let facts = &pre.facts;
    let file_search = facts
        .vector_store_id
        .clone()
        .filter(|_| d.send_file_search && facts.ready_docs);
    let search_knowledge = knowledge && file_search.is_none();
    ToolSet {
        file_search,
        web_search: d.send_web_search,
        code_interpreter: if d.send_code_interpreter {
            facts.xlsx_file_ids.clone()
        } else {
            Vec::new()
        },
        search_knowledge,
    }
}

/// Sum of the effective model's surcharges for the tools sent.
fn surcharges(entry: &ModelCatalogEntry, tools: &ToolSet) -> i64 {
    let b = &entry.estimation_budgets;
    let on = |sent: bool, tokens: u32| if sent { i64::from(tokens) } else { 0 };
    on(tools.file_search.is_some(), b.tool_surcharge_tokens)
        + on(tools.web_search, b.web_search_surcharge_tokens)
        + on(
            !tools.code_interpreter.is_empty(),
            b.code_interpreter_surcharge_tokens,
        )
}

/// Provider tools in the order `file_search`, `web_search`, `code_interpreter`,
/// `search_knowledge`.
fn llm_tools(entry: &ModelCatalogEntry, tools: &ToolSet) -> Vec<LlmTool> {
    let mut out = Vec::new();
    if let Some(vs) = &tools.file_search {
        out.push(LlmTool::FileSearch {
            vector_store_id: vs.clone(),
            max_num_results: entry.max_num_results,
        });
    }
    if tools.web_search {
        out.push(LlmTool::WebSearch {
            context_size: entry.web_search_context_size.clone(),
        });
    }
    if !tools.code_interpreter.is_empty() {
        out.push(LlmTool::CodeInterpreter {
            file_ids: tools.code_interpreter.clone(),
        });
    }
    if tools.search_knowledge {
        out.push(LlmTool::SearchKnowledge);
    }
    out
}

/// Anthropic chats send the secondary (Anthropic Files) ids of the images;
/// an image without one is dropped (DESIGN §4 "Provider after a downgrade").
fn anthropic_image_ids(request: &mut LlmRequest, secondary: &HashMap<String, String>) {
    for m in &mut request.input {
        m.image_file_ids = m
            .image_file_ids
            .iter()
            .filter_map(|id| secondary.get(id).cloned())
            .collect();
    }
}

/// Preflight columns of a turn from its decision.
fn preflight_fields(d: &PreflightDecision) -> DomainResult<PreflightFields> {
    let narrow = |v: i64, what: &str| {
        i32::try_from(v).map_err(|_| DomainError::internal(format!("{what} {v} out of range")))
    };
    Ok(PreflightFields {
        reserve_tokens: d.reserve_tokens,
        max_output_tokens_applied: narrow(d.max_output_tokens_applied, "max_output_tokens")?,
        reserved_credits_micro: d.reserved_credits_micro,
        policy_version_applied: i64::try_from(d.policy_version).map_err(|_| {
            DomainError::internal(format!("policy version {} out of range", d.policy_version))
        })?,
        effective_model: d.effective.id.clone(),
        minimal_generation_floor_applied: narrow(
            d.minimal_generation_floor_applied,
            "minimal_generation_floor",
        )?,
    })
}

/// `turn` with the preflight columns of `f` (as `fill_preflight` writes them).
fn with_preflight(mut turn: chat_turn::Model, f: &PreflightFields) -> chat_turn::Model {
    turn.reserve_tokens = turn.reserve_tokens.or(Some(f.reserve_tokens));
    turn.max_output_tokens_applied = turn
        .max_output_tokens_applied
        .or(Some(f.max_output_tokens_applied));
    turn.reserved_credits_micro = turn
        .reserved_credits_micro
        .or(Some(f.reserved_credits_micro));
    turn.policy_version_applied = turn
        .policy_version_applied
        .or(Some(f.policy_version_applied));
    turn.effective_model = turn
        .effective_model
        .take()
        .or_else(|| Some(f.effective_model.clone()));
    turn.minimal_generation_floor_applied = turn
        .minimal_generation_floor_applied
        .or(Some(f.minimal_generation_floor_applied));
    turn
}

impl StreamService {
    #[must_use]
    pub fn new(deps: StreamDeps) -> Self {
        let StreamDeps {
            config,
            db,
            authz,
            chats,
            models,
            policy,
            quota,
            finalization,
            llm,
            providers,
            knowledge,
        } = deps;
        Self {
            cfg: config,
            db,
            authz,
            chats,
            models,
            policy,
            quota,
            finalization,
            llm,
            providers,
            knowledge,
        }
    }

    /// `streaming.sse_ping_interval_seconds`.
    #[must_use]
    pub fn ping_interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds))
    }

    /// Run the setup of `POST /v1/chats/{id}/messages:stream` (spec §8 steps
    /// 1–15). Every failure before the stream opens is returned as an error and
    /// leaves no turn, message or reserve behind.
    ///
    /// # Errors
    /// Validation, authorization, idempotency (`RequestIdConflict`), parallel
    /// turn (`TurnAlreadyRunning`), preflight and setup errors.
    pub async fn send(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> DomainResult<StreamStart> {
        // 1. Request validation.
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        validate_attachment_ids(&req.attachment_ids, &self.cfg)?;

        // 2. Authorization and chat.
        let scope = self
            .authz
            .chat_scope(&ctx, actions::SEND_MESSAGE, Some(chat_id))
            .await?;
        let chat = self.chats.load_chat(&scope, chat_id).await?;
        let user_id = ctx.subject_id();

        // 4. Idempotency: replay a completed turn, reject any other reuse. A
        // replay reads only persisted rows: it does not resolve the model.
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let conn = self.db.conn()?;
        if let Some(turn) =
            TurnRepo::find_by_request(&conn, chat.tenant_id, chat.id, request_id).await?
        {
            if turn.deleted_at.is_none() && turn.state == TurnState::Completed.as_str() {
                let events = replay::replay_events(&conn, &chat, &turn).await?;
                return Ok(StreamStart::Replay(events));
            }
            return Err(DomainError::RequestIdConflict);
        }

        // 3. The chat's model, without the enabled filter (new turns only).
        let model_id = chat.model.clone().ok_or(DomainError::InvalidModel)?;
        let model = self.models.resolve_chat_model(user_id, &model_id).await?;

        // 5. Parallel turn guard.
        if TurnRepo::running_in_chat(&conn, chat.tenant_id, chat.id)
            .await?
            .is_some()
        {
            return Err(DomainError::TurnAlreadyRunning);
        }

        // 6–10. Kill switches, chat facts, quota preflight, input and image guards.
        let pre = self
            .preflight(PreflightRequest {
                chat: &chat,
                user_id,
                model: &model,
                content: &req.content,
                attachment_ids: &req.attachment_ids,
                web_search: req.web_search,
            })
            .await?;

        // 11–14. Tools, context, provider and the reserve transaction.
        let prepared = self
            .prepare_turn(TurnPrep {
                chat,
                user_id,
                content: req.content,
                request_id,
                web_search_requested: req.web_search,
                pre,
                mode: TurnMode::New {
                    attachment_ids: req.attachment_ids,
                },
            })
            .await?;

        // 15. Open the stream.
        Ok(StreamStart::Live(self.spawn_turn(prepared)))
    }

    /// Steps 11–14: tool selection, context assembly, provider resolution and
    /// the reserve transaction (see [`TurnMode`]).
    ///
    /// # Errors
    /// `ContextBudgetExceeded`, provider resolution failures (`Internal`),
    /// `QuotaExceeded { Tokens }` from the reserve re-check, `InvalidAttachment`,
    /// `TurnAlreadyRunning` / `RequestIdConflict` (insert race), database failures.
    pub(crate) async fn prepare_turn(&self, p: TurnPrep) -> DomainResult<PreparedTurn> {
        let started = Instant::now();
        let tenant_id = p.chat.tenant_id;
        let chat_id = p.chat.id;
        let entry = p.pre.decision.effective.clone();

        // 11. Tools.
        let knowledge = self
            .knowledge
            .as_ref()
            .and_then(|k| k.for_tenant(tenant_id));
        let tools = tool_set(&p.pre, knowledge.is_some());
        let knowledge = knowledge.filter(|_| tools.search_knowledge);

        // 12. Context assembly.
        let conn = self.db.conn()?;
        let summary_row = ThreadSummaryRepo::get_for_chat(&conn, tenant_id, chat_id).await?;
        let frontier = summary_row
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let summary = summary_row.as_ref().and_then(|s| {
            s.summary_text
                .clone()
                .filter(|t| !t.trim().is_empty())
                .map(|t| (t, i64::from(s.token_estimate.unwrap_or(0))))
        });
        let history = match p.pre.boundary {
            Some(boundary) => {
                let mut rows = MessageRepo::recent_for_context(
                    &conn,
                    tenant_id,
                    chat_id,
                    boundary,
                    frontier,
                    self.cfg.context.recent_messages_limit,
                )
                .await?;
                rows.reverse();
                rows.into_iter()
                    .filter_map(|m| {
                        MessageRole::parse(&m.role).map(|role| HistoryMessage {
                            role,
                            content: m.content,
                        })
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        let plan = assemble(&ContextInput {
            entry: entry.clone(),
            max_output_tokens_applied: p.pre.decision.max_output_tokens_applied,
            system_prompt_extra: guards_for(&tools, &self.cfg),
            summary,
            history,
            user_text: p.content.clone(),
            image_file_ids: p.pre.image_file_ids.clone(),
            surcharges: surcharges(&entry, &tools),
        })?;
        let thread_summary_applied = plan.summary_applied.map(|estimate| {
            let stored = summary_row
                .as_ref()
                .and_then(|s| s.token_estimate)
                .map_or(estimate, i64::from);
            u32::try_from(stored.max(0)).unwrap_or(u32::MAX)
        });

        // 13. Provider of the effective model.
        let provider = self.providers.resolve(&entry.provider_id, tenant_id)?;
        let llm_tools = llm_tools(&entry, &tools);
        let metadata =
            RequestMetadata::new(tenant_id, p.user_id, chat_id, RequestType::Chat, &llm_tools);
        let mut request = LlmRequest {
            model: entry.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input: plan.messages.clone(),
            max_output_tokens: u32::try_from(p.pre.decision.max_output_tokens_applied)
                .unwrap_or(u32::MAX),
            tools: llm_tools,
            max_tool_calls: Some(entry.max_tool_calls),
            api_params: entry.general_config.api_params.clone(),
            user: user_field(&tenant_id.to_string(), &p.user_id.to_string()),
            metadata,
            stream: true,
            tool_rounds: Vec::new(),
        };
        if provider.kind == ProviderKind::AnthropicMessages {
            anthropic_image_ids(&mut request, &p.pre.secondary_image_ids);
        }
        let citation_map = if tools.file_search.is_some() {
            p.pre.facts.citation_map.clone()
        } else {
            HashMap::new()
        };

        // 14. Reserve transaction.
        let fields = preflight_fields(&p.pre.decision)?;
        let turn = match p.mode {
            TurnMode::New { attachment_ids } => {
                self.reserve_new_turn(
                    &p.chat,
                    &p.pre,
                    NewTurnRows {
                        user_id: p.user_id,
                        request_id: p.request_id,
                        web_search_enabled: p.web_search_requested,
                        content: p.content,
                        attachment_ids,
                        fields,
                    },
                )
                .await?
            }
            TurnMode::Existing { turn } => self.reserve_existing_turn(turn, &p.pre, fields).await?,
        };

        Ok(PreparedTurn {
            turn,
            chat_model: p.chat.model.clone().unwrap_or_default(),
            user_id: p.user_id,
            plan,
            thread_summary_applied,
            has_summary: summary_row.is_some(),
            provider,
            request,
            citation_map,
            decision: p.pre.decision,
            message_id: Uuid::new_v4(),
            knowledge,
            started,
        })
    }

    /// Send path reserve transaction: reserve + re-check, running turn, user
    /// message, chat `updated_at`, attachment validation and links.
    async fn reserve_new_turn(
        &self,
        chat: &chat::Model,
        pre: &Preflighted,
        rows: NewTurnRows,
    ) -> DomainResult<chat_turn::Model> {
        let quota = Arc::clone(&self.quota);
        let decision = pre.decision.clone();
        let limits: UserLimits = pre.user_limits;
        let (tenant_id, chat_id) = (chat.tenant_id, chat.id);
        let NewTurnRows {
            user_id,
            request_id,
            web_search_enabled,
            content,
            attachment_ids,
            fields,
        } = rows;
        // Allocated once: every transaction attempt writes the same rows.
        let (turn_id, message_id) = (Uuid::new_v4(), Uuid::new_v4());
        with_tx_retry(&self.db, "send reserve", move |tx| {
            let quota = Arc::clone(&quota);
            let decision = decision.clone();
            let (content, attachment_ids, fields) =
                (content.clone(), attachment_ids.clone(), fields.clone());
            Box::pin(async move {
                let now = now_utc();
                quota
                    .reserve_in_tx(tx, tenant_id, user_id, &decision, &limits)
                    .await?;
                // The turn first: a request-id race then reports `request_id_conflict`.
                let turn = TurnRepo::insert_running(
                    tx,
                    NewTurn {
                        id: turn_id,
                        tenant_id,
                        chat_id,
                        request_id,
                        requester_user_id: user_id,
                        web_search_enabled,
                        preflight: Some(fields),
                        now,
                    },
                )
                .await?;
                let inserted = MessageRepo::insert_if_absent(
                    tx,
                    user_message(&turn, message_id, content, now),
                )
                .await?;
                if !inserted {
                    return Err(DomainError::internal(format!(
                        "user message id {message_id} already exists"
                    )));
                }
                ChatRepo::touch(tx, tenant_id, chat_id, now).await?;
                let rows =
                    AttachmentRepo::find_in_chat(tx, tenant_id, chat_id, &attachment_ids).await?;
                check_attachments(&attachment_ids, &rows, user_id)?;
                AttachmentRepo::link_to_message(
                    tx,
                    tenant_id,
                    chat_id,
                    message_id,
                    &attachment_ids,
                    now,
                )
                .await?;
                Ok(turn)
            })
        })
        .await
    }

    /// Retry/edit reserve transaction: reserve + re-check and the turn's
    /// preflight columns.
    async fn reserve_existing_turn(
        &self,
        turn: chat_turn::Model,
        pre: &Preflighted,
        fields: PreflightFields,
    ) -> DomainResult<chat_turn::Model> {
        let quota = Arc::clone(&self.quota);
        let decision = pre.decision.clone();
        let limits = pre.user_limits;
        let (tenant_id, turn_id) = (turn.tenant_id, turn.id);
        let user_id = turn.requester_user_id.ok_or_else(|| {
            DomainError::internal(format!("turn {turn_id} has no requester user"))
        })?;
        let stored = fields.clone();
        with_tx_retry(&self.db, "retry/edit reserve", move |tx| {
            let quota = Arc::clone(&quota);
            let (decision, stored) = (decision.clone(), stored.clone());
            Box::pin(async move {
                quota
                    .reserve_in_tx(tx, tenant_id, user_id, &decision, &limits)
                    .await?;
                TurnRepo::fill_preflight(tx, turn_id, &stored).await
            })
        })
        .await?;
        Ok(with_preflight(turn, &fields))
    }

    /// Spawn the provider task of a prepared turn and return its live stream
    /// (first event `stream_started`).
    #[must_use]
    pub(crate) fn spawn_turn(&self, t: PreparedTurn) -> LiveStream {
        let capacity = usize::from(self.cfg.streaming.sse_channel_capacity).max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let cancel = CancellationToken::new();
        let first = SseEvent::StreamStarted(StreamStartedData {
            request_id: t.turn.request_id,
            message_id: t.message_id,
            is_new_turn: true,
            thread_summary_applied: t
                .thread_summary_applied
                .map(|token_estimate| ThreadSummaryInfo { token_estimate }),
        });
        let deps = provider_task::TaskDeps {
            cfg: Arc::clone(&self.cfg),
            db: Arc::clone(&self.db),
            llm: Arc::clone(&self.llm),
            finalization: Arc::clone(&self.finalization),
        };
        tokio::spawn(provider_task::run(deps, t, tx, cancel.clone()));
        LiveStream {
            events: rx,
            cancel,
            first,
        }
    }
}

/// The user message row of a new turn (send path and the retry/edit commit).
pub(crate) fn user_message(
    turn: &chat_turn::Model,
    id: Uuid,
    content: String,
    now: chrono::DateTime<chrono::Utc>,
) -> message::Model {
    message::Model {
        id,
        tenant_id: turn.tenant_id,
        chat_id: turn.chat_id,
        request_id: Some(turn.request_id),
        role: MessageRole::User.as_str().to_owned(),
        content,
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: None,
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: None,
        is_compressed: false,
        created_at: now,
        deleted_at: None,
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod stream_tests;
