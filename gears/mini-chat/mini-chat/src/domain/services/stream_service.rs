//! Send pipeline of `POST /chats/{id}/messages:stream` (S§6.1, D "Send Message
//! with Streaming Response"): validation, idempotency / replay, parallel
//! guard, preflight, context assembly, provider resolution and the reserve
//! transaction. The provider stream itself runs in [`TurnRunner`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, PolicySnapshot, UserLimits};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use crate::api::rest::dto::{Citation, DeltaKind, DoneData, StreamStartedData, ToolPhase};
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::authz;
use crate::domain::clock::Clock;
use crate::domain::context::{self, ContextInput, ContextPlan, HistoryMessage, SummaryForContext};
use crate::domain::error::{DomainError, FeatureSubject};
use crate::domain::estimation::ReserveInputs;
use crate::domain::ports::{KnowledgeRetriever, PolicyPort};
use crate::domain::services::quota_service::PreflightDecision;
use crate::domain::services::replay::{self, ReplayTurn};
use crate::domain::services::turn_runner::TurnRunner;
use crate::domain::services::{ChatService, ModelResolver, QuotaService};
use crate::domain::tools::{self, ChatToolFacts, KnowledgeParams, SelectedTools};
use crate::infra::db::entity::{attachment, chat, chat_turn, message, message_attachment};
use crate::infra::db::repos::{
    AttachmentRepo, ChatRepo, MessageAttachmentRepo, MessageRepo, ThreadSummaryRepo, TurnPreflight,
    TurnRepo, TurnTerminal, VectorStoreRepo,
};
use crate::infra::db::tx::with_retry;
use crate::infra::llm::provider_resolver::{ProviderResolver, ProviderTarget};
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::{LlmRequest, REQUEST_TYPE_CHAT, RequestMetadata, Role};

/// `VISION_INPUT` multimodal capability of a catalog model.
const VISION_INPUT: &str = "VISION_INPUT";

/// A user send (`StreamMessageRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMessage {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
}

/// One relayed SSE event after `stream_started` (pings are added by the
/// SSE writer).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Delta {
        kind: DeltaKind,
        content: String,
    },
    Tool {
        phase: ToolPhase,
        name: String,
        details: Value,
    },
    Citations(Vec<Citation>),
    /// Terminal: the turn committed as completed.
    Done(Box<DoneData>),
    /// Terminal: `{code, message}` (sanitized message).
    Error {
        code: String,
        message: String,
    },
}

impl StreamEvent {
    /// `done` / `error`.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// `delta` / `tool` (content events end the ping phase).
    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }
}

/// A live generation: its `stream_started` payload and the relayed events.
/// Cancelling `cancel` (or dropping `events`) cancels the turn.
#[derive(Debug)]
pub struct LiveTurn {
    pub started: StreamStartedData,
    pub events: mpsc::Receiver<StreamEvent>,
    /// Cancelled when the response body is dropped (client disconnect).
    pub cancel: CancellationToken,
}

/// Result of a send: a new generation or the replay of a completed turn.
#[derive(Debug)]
pub enum TurnStream {
    Live(LiveTurn),
    Replay(ReplayTurn),
}

/// Everything the provider task of one committed turn needs.
#[derive(Debug, Clone)]
pub(crate) struct TurnSetup {
    pub ctx: SecurityContext,
    pub chat: chat::Model,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub assistant_message_id: Uuid,
    pub decision: PreflightDecision,
    pub target: ProviderTarget,
    pub llm_request: LlmRequest,
    /// `provider_file_id → (attachment_id, filename)` (only with `file_search`).
    pub citation_map: HashMap<String, (Uuid, String)>,
    pub tool_flags: SelectedTools,
    pub plan: ContextPlan,
    /// `chats.model`.
    pub selected_model: String,
    pub limits: UserLimits,
    /// `chat_turns.started_at`.
    pub started_at: OffsetDateTime,
}

/// Attachment / history facts of a chat read before the preflight.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChatFacts {
    pub tools: ChatToolFacts,
    /// Ready, non-deleted attachments of the chat.
    pub ready: Vec<attachment::Model>,
    /// Provider file ids of the images among the request's attachments.
    pub image_file_ids: Vec<String>,
    /// Anthropic Files API ids of those images that have an uploaded
    /// secondary copy (sent instead to an `anthropic_messages` provider).
    pub image_secondary_file_ids: Vec<String>,
    /// Number of images among the request's attachments.
    pub image_count: u32,
    pub prior_context_tokens: i64,
}

/// The user message a turn is generated from (a send, the original
/// message of a retry, or the edited message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnInput {
    pub content: String,
    /// Attachments of the message (a retry / edit: the non-deleted
    /// attachments copied from the original message).
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
}

/// Outcome of a passed preflight.
#[derive(Debug, Clone)]
pub(crate) struct Preflight {
    pub snap: Arc<PolicySnapshot>,
    pub limits: UserLimits,
    pub decision: PreflightDecision,
    pub facts: ChatFacts,
}

/// A retry / edit turn inserted by the mutation commit (`running`,
/// preflight columns NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NewTurn {
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// The new user message.
    pub user_message_id: Uuid,
    /// `chat_turns.started_at`.
    pub started_at: OffsetDateTime,
}

/// Persisted conversation context of a turn: the chat's thread summary and
/// the recent messages after its frontier.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextHistory {
    pub summary: Option<SummaryForContext>,
    pub recent: Vec<HistoryMessage>,
}

/// Provider request of a turn and the plan it came from.
#[derive(Debug, Clone)]
pub(crate) struct PreparedRequest {
    pub target: ProviderTarget,
    pub llm_request: LlmRequest,
    pub plan: ContextPlan,
    pub tools: SelectedTools,
    pub citation_map: HashMap<String, (Uuid, String)>,
}

/// Dependencies of [`StreamService`].
pub struct StreamDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub chats: Arc<ChatService>,
    pub models: Arc<ModelResolver>,
    pub policy: Arc<dyn PolicyPort>,
    pub quota: Arc<QuotaService>,
    pub providers: Arc<ProviderResolver>,
    pub runner: TurnRunner,
    /// Knowledge retriever (only when `knowledge_search.enabled`).
    pub knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

/// Send pipeline (validation → reserve transaction → provider task).
pub struct StreamService {
    config: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    chats: Arc<ChatService>,
    models: Arc<ModelResolver>,
    policy: Arc<dyn PolicyPort>,
    quota: Arc<QuotaService>,
    providers: Arc<ProviderResolver>,
    runner: TurnRunner,
    knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

impl StreamService {
    #[must_use]
    pub fn new(d: StreamDeps) -> Self {
        Self {
            config: d.config,
            db: d.db,
            clock: d.clock,
            chats: d.chats,
            models: d.models,
            policy: d.policy,
            quota: d.quota,
            providers: d.providers,
            runner: d.runner,
            knowledge: d.knowledge,
        }
    }

    /// Send a user message (S§6.1 order). Returns the replay of a completed
    /// turn for a known `request_id`, otherwise commits a running turn and
    /// starts its provider task.
    ///
    /// # Errors
    /// Every pre-stream rejection (JSON error, nothing persisted):
    /// `EmptyContent`, `InvalidAttachment`, authorization, `ChatNotFound`,
    /// `InvalidModel`, `RequestIdConflict`, `TurnAlreadyRunning`,
    /// `TooManyImages`, `FeatureDisabled`, `QuotaExceeded`,
    /// `VisionNotSupported`, `InputTooLong`, `ContextBudgetExceeded`,
    /// provider resolution, plugin and database failures.
    pub async fn send(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        req: SendMessage,
    ) -> Result<TurnStream, DomainError> {
        self.validate(&req)?;
        let (scope, chat) = self
            .chats
            .load_scoped(&ctx, authz::SEND_MESSAGE, chat_id)
            .await?;
        let (snap, _) = self
            .models
            .resolve_chat_model(ctx.subject_id(), &chat.model)
            .await?;

        let conn = self.db.conn()?;
        if let Some(rid) = req.request_id
            && let Some(turn) = TurnRepo
                .find_by_request(&conn, &scope, chat.id, rid)
                .await?
        {
            if turn.state == "completed" && turn.deleted_at.is_none() {
                let replay = replay::load(&conn, &scope, &chat, &turn).await?;
                return Ok(TurnStream::Replay(replay));
            }
            return Err(DomainError::RequestIdConflict);
        }
        if TurnRepo
            .find_running(&conn, &scope, chat.id)
            .await?
            .is_some()
        {
            return Err(DomainError::TurnAlreadyRunning);
        }

        let input = TurnInput {
            content: req.content.clone(),
            attachment_ids: req.attachment_ids.clone(),
            web_search_enabled: req.web_search_enabled,
        };
        let pre = self
            .preflight(&conn, &scope, &ctx, &chat, snap, &input, None)
            .await?;
        let history = self.history(&conn, &scope, chat.id, None).await?;
        let prepared = self.prepare_turn_request(
            &ctx,
            &chat,
            &pre.snap,
            &pre.decision,
            &pre.facts,
            history,
            &input.content,
            input.web_search_enabled,
        )?;
        let Preflight {
            limits, decision, ..
        } = pre;

        let turn_id = Uuid::new_v4();
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let assistant_message_id = Uuid::new_v4();
        let started_at = self
            .reserve(
                &ctx, &scope, &chat, &decision, &limits, &req, turn_id, request_id,
            )
            .await?;

        let selected_model = chat.model.clone();
        Ok(TurnStream::Live(self.run_turn(TurnSetup {
            ctx,
            chat,
            turn_id,
            request_id,
            assistant_message_id,
            decision,
            target: prepared.target,
            llm_request: prepared.llm_request,
            citation_map: prepared.citation_map,
            tool_flags: prepared.tools,
            plan: prepared.plan,
            selected_model,
            limits,
            started_at,
        })))
    }

    /// Preflight of a turn (S§6.1 steps 6–9): attachment facts, image count,
    /// kill switches, quota cascade, vision and input-size checks.
    /// `replaced_request_id` is the turn a retry / edit replaces (its answer
    /// is not prior context).
    #[allow(clippy::too_many_arguments)]
    async fn preflight(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        ctx: &SecurityContext,
        chat: &chat::Model,
        snap: Arc<PolicySnapshot>,
        input: &TurnInput,
        replaced_request_id: Option<Uuid>,
    ) -> Result<Preflight, DomainError> {
        let (tenant, user) = (ctx.subject_tenant_id(), ctx.subject_id());
        let facts = self
            .chat_facts(
                runner,
                scope,
                chat,
                &input.attachment_ids,
                replaced_request_id,
            )
            .await?;
        if facts.image_count > self.config.rag.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }
        check_kill_switches(
            snap.kill_switches,
            input.web_search_enabled,
            facts.image_count,
        )?;

        let limits = self.policy.user_limits(user, snap.policy_version).await?;
        let inputs = ReserveInputs {
            message_bytes: input.content.len(),
            prior_context_tokens: facts.prior_context_tokens,
            image_count: facts.image_count,
            chat_has_ready_docs: facts.tools.has_ready_docs,
            chat_has_ready_xlsx: !facts.tools.code_interpreter_file_ids.is_empty(),
            web_search_requested: input.web_search_enabled,
        };
        let decision = self
            .quota
            .preflight(
                tenant,
                user,
                &chat.model,
                &snap,
                &limits,
                &inputs,
                self.clock.now(),
            )
            .await?;
        check_effective_model(&decision.effective, &input.content, facts.image_count)?;
        Ok(Preflight {
            snap,
            limits,
            decision,
            facts,
        })
    }

    /// Mutation preflight of a retry / edit (D "Retry / edit variant" step
    /// 2): resolve the chat model (no enabled filter) and run the send
    /// preflight for the re-submitted message. Read-only; no PDP call (the
    /// caller authorized the mutation and passes its `scope`).
    ///
    /// # Errors
    /// `InvalidModel`, `TooManyImages`, `FeatureDisabled`, `QuotaExceeded`,
    /// `VisionNotSupported`, `InputTooLong`, plugin and database failures.
    pub(crate) async fn mutation_preflight(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat: &chat::Model,
        input: &TurnInput,
        replaced_request_id: Uuid,
    ) -> Result<Preflight, DomainError> {
        let (snap, _) = self
            .models
            .resolve_chat_model(ctx.subject_id(), &chat.model)
            .await?;
        let conn = self.db.conn()?;
        self.preflight(
            &conn,
            scope,
            ctx,
            chat,
            snap,
            input,
            Some(replaced_request_id),
        )
        .await
    }

    /// Steps 4–5 of a retry / edit after the mutation commit: context
    /// assembly, provider resolution, the quota reserve with the re-check
    /// (filling the turn's preflight columns), then the provider task.
    ///
    /// A failure marks the committed turn `failed` with a plain CAS
    /// (`context_length_exceeded`, `quota_exceeded` or `turn_setup_failed`;
    /// no reserve exists, so no settlement and no outbox event) and is
    /// returned as is (D§5.7 "Unstarted retry/edit turn").
    ///
    /// # Errors
    /// `ContextBudgetExceeded`, `QuotaExceeded`, provider resolution and
    /// database failures.
    pub(crate) async fn start_committed_turn(
        &self,
        ctx: SecurityContext,
        scope: AccessScope,
        chat: chat::Model,
        pre: Preflight,
        input: &TurnInput,
        turn: NewTurn,
    ) -> Result<LiveTurn, DomainError> {
        let turn_id = turn.turn_id;
        match self
            .setup_committed(ctx, &scope, chat, pre, input, turn)
            .await
        {
            Ok(setup) => Ok(self.run_turn(setup)),
            Err(e) => {
                self.fail_unstarted(&scope, turn_id, &e).await;
                Err(e)
            }
        }
    }

    async fn setup_committed(
        &self,
        ctx: SecurityContext,
        scope: &AccessScope,
        chat: chat::Model,
        pre: Preflight,
        input: &TurnInput,
        turn: NewTurn,
    ) -> Result<TurnSetup, DomainError> {
        let conn = self.db.conn()?;
        let history = self
            .history(&conn, scope, chat.id, Some(turn.user_message_id))
            .await?;
        let prepared = self.prepare_turn_request(
            &ctx,
            &chat,
            &pre.snap,
            &pre.decision,
            &pre.facts,
            history,
            &input.content,
            input.web_search_enabled,
        )?;
        self.reserve_committed(&ctx, scope, &pre.decision, &pre.limits, turn.turn_id)
            .await?;
        let selected_model = chat.model.clone();
        Ok(TurnSetup {
            ctx,
            chat,
            turn_id: turn.turn_id,
            request_id: turn.request_id,
            assistant_message_id: Uuid::new_v4(),
            decision: pre.decision,
            target: prepared.target,
            llm_request: prepared.llm_request,
            citation_map: prepared.citation_map,
            tool_flags: prepared.tools,
            plan: prepared.plan,
            selected_model,
            limits: pre.limits,
            started_at: turn.started_at,
        })
    }

    /// Reserve transaction of a committed retry / edit turn: quota reserve
    /// + re-check and the turn's preflight columns.
    async fn reserve_committed(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        decision: &PreflightDecision,
        limits: &UserLimits,
        turn_id: Uuid,
    ) -> Result<(), DomainError> {
        let now = self.clock.now();
        let (tenant, user) = (ctx.subject_tenant_id(), ctx.subject_id());
        let columns = preflight_columns(decision);
        let quota = Arc::clone(&self.quota);
        let (scope, decision, limits) = (scope.clone(), decision.clone(), *limits);
        with_retry(&self.db, move |tx| {
            let (quota, scope, decision, columns) = (
                Arc::clone(&quota),
                scope.clone(),
                decision.clone(),
                columns.clone(),
            );
            Box::pin(async move {
                quota.reserve(tx, tenant, user, &decision, &limits).await?;
                let rows = TurnRepo
                    .fill_preflight(tx, &scope, turn_id, &columns, now)
                    .await?;
                if rows == 0 {
                    return Err(DomainError::Internal(format!(
                        "turn {turn_id} is no longer an unstarted running turn"
                    )));
                }
                Ok(())
            })
        })
        .await
    }

    /// Plain CAS `running → failed` of a committed turn whose setup failed.
    async fn fail_unstarted(&self, scope: &AccessScope, turn_id: Uuid, err: &DomainError) {
        let terminal = TurnTerminal {
            state: "failed",
            error_code: Some(setup_error_code(err).to_owned()),
            error_detail: Some(err.to_string()),
            assistant_message_id: None,
            provider_response_id: None,
            now: self.clock.now(),
        };
        let result = match self.db.conn() {
            Ok(conn) => TurnRepo
                .finalize_cas(&conn, scope, turn_id, &terminal)
                .await
                .map_err(DomainError::from),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            // The orphan watchdog finalizes the turn later.
            warn!(%turn_id, error = %e, "failed to mark an unstarted turn failed");
        }
    }

    /// Start the provider task of a committed turn (also used by retry /
    /// edit).
    pub(crate) fn run_turn(&self, setup: TurnSetup) -> LiveTurn {
        self.runner.run(setup)
    }

    /// Body checks that need no query (S§6.1 step 1).
    fn validate(&self, req: &SendMessage) -> Result<(), DomainError> {
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let rag = &self.config.rag;
        let max = rag
            .max_documents_per_chat
            .saturating_add(rag.max_images_per_message) as usize;
        let unique: HashSet<&Uuid> = req.attachment_ids.iter().collect();
        if unique.len() != req.attachment_ids.len() || req.attachment_ids.len() > max {
            return Err(DomainError::InvalidAttachment);
        }
        Ok(())
    }

    /// Ready attachments, vector store, images of the request and the prior
    /// context tokens (snapshot before the reserve; the answer of the turn
    /// `replaced_request_id` replaced by a retry / edit is not counted).
    pub(crate) async fn chat_facts(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat: &chat::Model,
        attachment_ids: &[Uuid],
        replaced_request_id: Option<Uuid>,
    ) -> Result<ChatFacts, DomainError> {
        let ready = AttachmentRepo.list_ready(runner, scope, chat.id).await?;
        let vector_store_id = VectorStoreRepo
            .find_by_chat(runner, scope, chat.id)
            .await?
            .and_then(|v| v.vector_store_id);
        let images: Vec<attachment::Model> = AttachmentRepo
            .find_in_chat(runner, scope, chat.id, attachment_ids)
            .await?
            .into_iter()
            .filter(|a| a.attachment_kind == "image" && a.deleted_at.is_none())
            .collect();
        let prior = MessageRepo
            .latest_assistant_usage(runner, scope, chat.id, replaced_request_id)
            .await?
            .map_or(0, |(i, o)| i.saturating_add(o));
        let tools = ChatToolFacts {
            vector_store_id,
            has_ready_docs: ready
                .iter()
                .any(|a| a.attachment_kind == "document" && a.for_file_search),
            code_interpreter_file_ids: ready
                .iter()
                .filter(|a| a.for_code_interpreter)
                .filter_map(|a| a.provider_file_id.clone())
                .collect(),
        };
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        let image_secondary_file_ids = images
            .iter()
            .filter(|a| a.secondary_status == "uploaded")
            .filter_map(|a| a.secondary_file_id.clone())
            .collect();
        Ok(ChatFacts {
            tools,
            ready,
            image_file_ids: images
                .into_iter()
                .filter_map(|a| a.provider_file_id)
                .collect(),
            image_secondary_file_ids,
            image_count,
            prior_context_tokens: prior,
        })
    }

    /// Thread summary and recent-history window (messages after the
    /// summary frontier, not compressed; oldest first is restored by the
    /// assembly). `exclude` is the current user message of a committed
    /// retry / edit turn (sent separately as the current message).
    pub(crate) async fn history(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        exclude: Option<Uuid>,
    ) -> Result<ContextHistory, DomainError> {
        let summary = ThreadSummaryRepo
            .find_by_chat(runner, scope, chat_id)
            .await?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let limit = u64::from(self.config.context.recent_messages_limit);
        let fetch = if exclude.is_some() { limit + 1 } else { limit };
        let rows = MessageRepo
            .recent_for_context(
                runner,
                scope,
                chat_id,
                fetch,
                frontier,
                self.db.db().backend(),
            )
            .await?;
        let recent = rows
            .into_iter()
            .filter(|m| Some(m.id) != exclude)
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .filter_map(history_message)
            .collect();
        Ok(ContextHistory {
            summary: summary.map(|s| SummaryForContext {
                text: s.summary_text,
                token_estimate: i64::from(s.token_estimate),
            }),
            recent,
        })
    }

    /// Tools, context assembly and provider resolution of a turn
    /// (S§6.1 step 10; retry / edit reuse it).
    ///
    /// # Errors
    /// `ContextBudgetExceeded`; `ProviderResolution`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_turn_request(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        snap: &PolicySnapshot,
        decision: &PreflightDecision,
        facts: &ChatFacts,
        history: ContextHistory,
        content: &str,
        web_search_enabled: bool,
    ) -> Result<PreparedRequest, DomainError> {
        let eff = &decision.effective;
        let (tenant, user) = (ctx.subject_tenant_id(), ctx.subject_id());
        let target = self.providers.resolve(&eff.provider_id, tenant)?;
        let knowledge = self.knowledge_params(tenant);
        let sel = tools::select_tools(
            eff,
            &snap.kill_switches,
            &facts.tools,
            web_search_enabled,
            knowledge.as_ref(),
        );
        let budgets = &eff.estimation_budgets;
        let surcharge = |on: bool, tokens: u32| if on { i64::from(tokens) } else { 0 };
        let surcharges = surcharge(sel.file_search, budgets.tool_surcharge_tokens)
            + surcharge(sel.web_search, budgets.web_search_surcharge_tokens)
            + surcharge(
                sel.code_interpreter,
                budgets.code_interpreter_surcharge_tokens,
            );
        let max_out = u32::try_from(decision.plan.max_output_tokens_applied).unwrap_or(u32::MAX);
        let plan = context::assemble(ContextInput {
            eff,
            max_output_tokens_applied: max_out,
            system_prompt: &eff.system_prompt,
            guards: tools::guards(&sel, &self.config.context, knowledge.as_ref()),
            summary: history.summary,
            recent: history.recent,
            current_text: content,
            current_images: current_images(&target, facts),
            surcharges,
        })?;
        let citation_map = if sel.file_search {
            facts
                .ready
                .iter()
                .filter_map(|a| {
                    a.provider_file_id
                        .clone()
                        .map(|f| (f, (a.id, a.filename.clone())))
                })
                .collect()
        } else {
            HashMap::new()
        };
        let llm_request = LlmRequest {
            model: eff.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input: plan.input.clone(),
            max_output_tokens: max_out,
            tools: sel.specs.clone(),
            max_tool_calls: eff.max_tool_calls,
            api_params: eff.general_config.api_params.clone(),
            user: provider_user_field(tenant, user),
            metadata: RequestMetadata {
                tenant_id: tenant,
                user_id: user,
                chat_id: chat.id,
                request_type: REQUEST_TYPE_CHAT,
                feature: sel.feature_label.clone(),
            },
            stream: true,
        };
        Ok(PreparedRequest {
            target,
            llm_request,
            plan,
            tools: sel,
            citation_map,
        })
    }

    /// Knowledge search parameters of a request for `tenant`, or `None`
    /// (with a warning when enabled but not usable): the retriever must be
    /// configured and the `knowledge_search.provider_id` entry must be of
    /// kind `openai_responses` / `anthropic_messages` with an alias and a
    /// non-empty `api_version` (D§4 "Knowledge Search").
    fn knowledge_params(&self, tenant: Uuid) -> Option<KnowledgeParams> {
        let k = &self.config.knowledge_search;
        if !k.enabled {
            return None;
        }
        let problem = match (&self.knowledge, k.provider_id.as_deref()) {
            (None, _) => Some("the knowledge retriever is not configured"),
            (_, None) => Some("knowledge_search.provider_id is not set"),
            (Some(_), Some(id)) => match self.providers.knowledge_target(id, tenant) {
                Err(_) => Some("knowledge_search.provider_id names no provider entry"),
                Ok((kind, _))
                    if !matches!(
                        kind,
                        ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
                    ) =>
                {
                    Some(
                        "the knowledge provider kind is not openai_responses or anthropic_messages",
                    )
                }
                Ok((_, t)) if t.alias.is_empty() => Some("the knowledge provider has no alias"),
                Ok((_, t)) if t.api_version.as_deref().is_none_or(str::is_empty) => {
                    Some("the knowledge provider has no api_version")
                }
                Ok(_) => None,
            },
        };
        if let Some(reason) = problem {
            warn!(%tenant, reason, "knowledge search is off for this request");
            return None;
        }
        Some(KnowledgeParams {
            guard: k.guard.clone(),
            tool: tools::search_knowledge_tool(),
        })
    }

    /// Reserve transaction (S§6.1 step 11): quota reserve + re-check, user
    /// message (+ chat `updated_at`), attachment validation and links, the
    /// running turn. Returns the turn's `started_at`.
    #[allow(clippy::too_many_arguments)]
    async fn reserve(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat: &chat::Model,
        decision: &PreflightDecision,
        limits: &UserLimits,
        req: &SendMessage,
        turn_id: Uuid,
        request_id: Uuid,
    ) -> Result<OffsetDateTime, DomainError> {
        let now = self.clock.now();
        let (tenant, user) = (ctx.subject_tenant_id(), ctx.subject_id());
        let user_msg = user_message(chat, request_id, &req.content, now);
        let turn = running_turn(
            chat,
            user,
            turn_id,
            request_id,
            Some(decision),
            req.web_search_enabled,
            now,
        );
        let quota = Arc::clone(&self.quota);
        let (scope_tx, decision, limits) = (scope.clone(), decision.clone(), *limits);
        let (chat_id, attachment_ids) = (chat.id, req.attachment_ids.clone());
        let result = with_retry(&self.db, move |tx| {
            let (quota, scope_tx, decision, user_msg, turn, attachment_ids) = (
                Arc::clone(&quota),
                scope_tx.clone(),
                decision.clone(),
                user_msg.clone(),
                turn.clone(),
                attachment_ids.clone(),
            );
            Box::pin(async move {
                let scope = scope_tx;
                quota.reserve(tx, tenant, user, &decision, &limits).await?;
                let msg = MessageRepo.insert(tx, &scope, user_msg).await?;
                ChatRepo.touch(tx, &scope, chat_id, now).await?;
                link_attachments(tx, &scope, &msg, user, &attachment_ids, now).await?;
                TurnRepo.insert(tx, &scope, turn).await?;
                Ok(())
            })
        })
        .await;
        match result {
            Ok(()) => Ok(now),
            Err(e) if e.is_unique_violation() => {
                // Lost an insert race: the request id or the running slot.
                let conn = self.db.conn()?;
                let taken = TurnRepo
                    .find_by_request(&conn, scope, chat.id, request_id)
                    .await?
                    .is_some();
                Err(if taken {
                    DomainError::RequestIdConflict
                } else {
                    DomainError::TurnAlreadyRunning
                })
            }
            Err(e) => Err(e),
        }
    }
}

/// Image file ids sent with the current message: the Anthropic secondary
/// copies for an `anthropic_messages` provider (images without one are not
/// sent), the provider file ids otherwise.
fn current_images(target: &ProviderTarget, facts: &ChatFacts) -> Vec<String> {
    if target.kind == ProviderKind::AnthropicMessages {
        if facts.image_secondary_file_ids.len() < facts.image_file_ids.len() {
            warn!(
                provider = %target.provider_id,
                "images without an Anthropic file copy are not sent"
            );
        }
        facts.image_secondary_file_ids.clone()
    } else {
        facts.image_file_ids.clone()
    }
}

/// Kill switches checked before the quota cascade (S§6.1 step 7).
fn check_kill_switches(
    ks: KillSwitches,
    web_search_enabled: bool,
    image_count: u32,
) -> Result<(), DomainError> {
    if web_search_enabled && ks.disable_web_search {
        return Err(DomainError::FeatureDisabled(FeatureSubject::WebSearch));
    }
    if image_count > 0 && ks.disable_images {
        return Err(DomainError::FeatureDisabled(FeatureSubject::Images));
    }
    Ok(())
}

/// Vision and input-size checks on the effective model (S§6.1 step 9).
fn check_effective_model(
    eff: &ModelCatalogEntry,
    content: &str,
    image_count: u32,
) -> Result<(), DomainError> {
    if image_count > 0
        && !eff
            .multimodal_capabilities
            .iter()
            .any(|c| c == VISION_INPUT)
    {
        return Err(DomainError::VisionNotSupported);
    }
    if context::input_too_long(eff, content) {
        return Err(DomainError::InputTooLong);
    }
    Ok(())
}

/// Validate the request's attachments inside the reserve transaction and
/// link them to the user message (D "Attachment Preflight Validation").
async fn link_attachments(
    runner: &impl DBRunner,
    scope: &AccessScope,
    msg: &message::Model,
    user: Uuid,
    ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if ids.is_empty() {
        return Ok(());
    }
    let found = AttachmentRepo
        .find_in_chat(runner, scope, msg.chat_id, ids)
        .await?;
    for id in ids {
        let valid = found.iter().any(|a| {
            a.id == *id
                && a.tenant_id == msg.tenant_id
                && a.uploaded_by_user_id == user
                && a.status == "ready"
                && a.deleted_at.is_none()
        });
        if !valid {
            return Err(DomainError::InvalidAttachment);
        }
        MessageAttachmentRepo
            .insert(
                runner,
                scope,
                message_attachment::Model {
                    tenant_id: msg.tenant_id,
                    chat_id: msg.chat_id,
                    message_id: msg.id,
                    attachment_id: *id,
                    created_at: now,
                },
            )
            .await?;
    }
    Ok(())
}

/// A persisted user / assistant message as context history (other roles
/// are not sent to the model).
pub(crate) fn history_message(m: message::Model) -> Option<HistoryMessage> {
    let role = match m.role.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    Some(HistoryMessage {
        id: m.id,
        role,
        content: m.content,
        created_at: m.created_at,
    })
}

/// The user message row of a send / retry / edit.
pub(crate) fn user_message(
    chat: &chat::Model,
    request_id: Uuid,
    content: &str,
    now: OffsetDateTime,
) -> message::Model {
    message::Model {
        id: Uuid::new_v4(),
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        request_id: Some(request_id),
        role: "user".to_owned(),
        content: content.to_owned(),
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

/// Preflight columns of a turn from its preflight decision.
fn preflight_columns(d: &PreflightDecision) -> TurnPreflight {
    let to_i32 = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
    TurnPreflight {
        reserve_tokens: d.plan.reserve_tokens,
        max_output_tokens_applied: to_i32(d.plan.max_output_tokens_applied),
        reserved_credits_micro: d.plan.reserved_credits_micro,
        policy_version_applied: i64::try_from(d.policy_version).unwrap_or(i64::MAX),
        effective_model: d.effective.id.clone(),
        minimal_generation_floor_applied: to_i32(d.plan.minimal_generation_floor_applied),
    }
}

/// `error_code` of a retry / edit turn whose setup failed after the
/// mutation commit (D "Retry / edit variant").
fn setup_error_code(err: &DomainError) -> &'static str {
    match err {
        DomainError::ContextBudgetExceeded => "context_length_exceeded",
        DomainError::QuotaExceeded(_) => "quota_exceeded",
        _ => "turn_setup_failed",
    }
}

/// The `running` turn row. A send sets every preflight column on insert
/// (`decision`); a retry / edit turn starts with them NULL and fills them
/// in its reserve transaction.
pub(crate) fn running_turn(
    chat: &chat::Model,
    user: Uuid,
    turn_id: Uuid,
    request_id: Uuid,
    decision: Option<&PreflightDecision>,
    web_search_enabled: bool,
    now: OffsetDateTime,
) -> chat_turn::Model {
    let p = decision.map(preflight_columns);
    chat_turn::Model {
        id: turn_id,
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        request_id,
        requester_type: "user".to_owned(),
        requester_user_id: Some(user),
        state: "running".to_owned(),
        provider_name: None,
        provider_response_id: None,
        assistant_message_id: None,
        error_code: None,
        reserve_tokens: p.as_ref().map(|p| p.reserve_tokens),
        max_output_tokens_applied: p.as_ref().map(|p| p.max_output_tokens_applied),
        reserved_credits_micro: p.as_ref().map(|p| p.reserved_credits_micro),
        policy_version_applied: p.as_ref().map(|p| p.policy_version_applied),
        effective_model: p.as_ref().map(|p| p.effective_model.clone()),
        minimal_generation_floor_applied: p.as_ref().map(|p| p.minimal_generation_floor_applied),
        error_detail: None,
        deleted_at: None,
        replaced_by_request_id: None,
        started_at: now,
        last_progress_at: Some(now),
        web_search_enabled,
        web_search_completed_count: 0,
        code_interpreter_completed_count: 0,
        file_search_completed_count: 0,
        completed_at: None,
        updated_at: now,
    }
}
