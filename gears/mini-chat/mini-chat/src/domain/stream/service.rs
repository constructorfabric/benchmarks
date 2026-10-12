//! Stream service: setup of a new turn and idempotent replay (DESIGN section
//! 3.3 "Streaming Contract", "Attachment Preflight Validation Invariant";
//! section 3.6 "Send Message with Streaming Response"; "Context Plan
//! Assembly and Truncation", "`ContextPlan` Determinism and Snapshot
//! Boundary"; section 4 "Idempotency Rules", "Parallel Turn Policy (P1)";
//! sections 5.4.1-5.4.3; ADR-0010).
//!
//! [`StreamService::send`] runs, in this order (every failure before the
//! reserve transaction is a JSON error and leaves nothing behind):
//!
//! 1. request checks: non-blank content (`EMPTY_CONTENT`), distinct
//!    `attachment_ids`, at most `rag.max_documents_per_chat +
//!    rag.max_images_per_message` of them (`invalid_attachment`);
//! 2. PEP `send_message` on the chat, scoped chat load (404);
//! 3. idempotency on `(chat_id, request_id)` (section 4 "Check Priority
//!    Order"): a completed, live turn is replayed immediately through a
//!    separate, read-only path that depends neither on the current catalog
//!    nor on the policy plugin; any other state or a soft-deleted turn is
//!    `409 request_id_conflict`;
//! 4. parallel turn guard: any running turn is `409 turn_already_running`;
//! 5. the user's policy (version, snapshot, limits) and the chat model looked
//!    up without the `enabled` filter (`INVALID_MODEL` when it left the
//!    catalog; plugin failures are 500);
//! 6. snapshot boundary, `prior_context_tokens`, thread summary, recent
//!    messages, the chat's ready attachments and vector store, the requested
//!    attachments (`TOO_MANY_IMAGES` above `rag.max_images_per_message`);
//! 7. quota preflight (web search kill switch, cascade, tool quotas);
//! 8. image guards on the effective model (`disable_images`:
//!    `FEATURE_DISABLED`, no `VISION_INPUT`: `VISION_NOT_SUPPORTED`), then
//!    `INPUT_TOO_LONG`;
//! 9. context assembly (`CONTEXT_BUDGET_EXCEEDED`) and the summary trigger;
//! 10. provider resolution for the effective model's `provider_id` (500);
//!     the `search_knowledge` tool and `knowledge_search.guard` are added in
//!     step 9 when knowledge search is on for the request and `file_search`
//!     is not sent (DESIGN section 4 "Knowledge Search");
//! 11. one reserve transaction (the reserve increment is its first
//!     statement): reserve + limit re-check, user message, chat
//!     `updated_at` (a chat soft-deleted since the load is `404`, the
//!     transaction rolls back), attachment validation and links, the
//!     `running` turn. A unique violation on the running-turn index is `turn_already_running`,
//!     on the request key `request_id_conflict` (the key is re-read after
//!     the rollback and an existing turn always reports
//!     `request_id_conflict`);
//! 12. the provider task is spawned and [`StreamStart::Live`] returned.
//!
//! Retry / edit (DESIGN section 3.6 "Retry / edit variant", section 3.9) use
//! the same pipeline split around the mutation commit of the turn service:
//!
//! - [`StreamService::mutation_preflight`] (step 2, before the commit): the
//!   policy, the chat model without the `enabled` filter (`INVALID_MODEL`),
//!   `TOO_MANY_IMAGES`, the quota preflight with the original turn's
//!   `web_search_enabled`, the image guards for the original user message's
//!   live images and `INPUT_TOO_LONG` — steps 5 to 8 above, read-only; a
//!   rejection changes nothing. `prior_context_tokens` skips the replaced
//!   turn (its own usage is not part of the history that will be re-sent);
//! - [`StreamService::start_mutation`] (step 4, after the commit): context
//!   assembly over the history before the replacement user message, provider
//!   resolution and one reserve transaction (the reserve increment first,
//!   the limit re-check, then the one-time write of the turn's preflight
//!   columns), then the provider task as above. The preflight decision of
//!   step 2 is reused, no PDP call is made. Any failure moves the new turn
//!   `running → failed` with a plain CAS (`context_length_exceeded` for the
//!   context budget, `quota_exceeded` for the reserve re-check,
//!   `turn_setup_failed` otherwise; no settlement, no outbox event), retried
//!   up to [`FAIL_UNSTARTED_ATTEMPTS`] times with a short backoff so the
//!   chat is not left with a blocking `running` turn, and the setup error is
//!   returned as the JSON error.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::ModelCatalogEntry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::Db;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::events::{Delta, DeltaKind, Done, DoneUsage, StreamEvent, StreamStarted};
use super::finalize::{Finalizer, TurnContext};
use super::task::{CitedFile, PROGRESS_REFRESH_INTERVAL, ProviderTask, TaskLimits};
use crate::config::{ContextConfig, KnowledgeSearchConfig, MiniChatConfig};
use crate::domain::authz::{Action, AuthzService};
use crate::domain::billing::{CONTEXT_LENGTH_EXCEEDED, QUOTA_EXCEEDED, TURN_SETUP_FAILED};
use crate::domain::chat_service::chat_not_found;
use crate::domain::context::{
    ContextInput, ContextPlan, ContextRole, HistoryMessage, ModelLimits, SummaryForContext,
    ToolSurcharges, assemble, check_input_length, select_guards, summary_trigger,
};
use crate::domain::error::{
    AbortReason, DomainError, ExhaustedLimit, InvalidArgumentReason, OutOfRangeReason,
    PreconditionSubject,
};
use crate::domain::knowledge::{KnowledgeRetriever, KnowledgeTurn, search_knowledge_tool};
use crate::domain::llm::{
    LlmInputItem, LlmMetadata, LlmProvider, LlmProviderResolver, LlmRequest, LlmRequestType,
    LlmTool, feature_label, user_field,
};
use crate::domain::ports::{
    AttachmentKind, AttachmentStatus, ChatAttachmentRecord, ChatRecord, MessageAttachmentLinks,
    MessageRepository, MessageRole, NewRunningTurn, NewUserMessage, QuotaOwner, Repositories,
    ThreadSummaryRecord, TurnKeyRecord, TurnPreflightColumns, TurnState, storage_now,
};
use crate::domain::quota_service::{
    PolicyContext, PreflightDecision, PreflightInput, PreflightRequest, QuotaDecisionKind,
    QuotaService,
};

/// Attempts of the `running → failed` CAS of an unstarted retry / edit turn
/// whose setup failed.
pub const FAIL_UNSTARTED_ATTEMPTS: u32 = 3;

/// Backoff before the second attempt of that CAS (doubled for each later
/// attempt).
const FAIL_UNSTARTED_BACKOFF: Duration = Duration::from_millis(50);

// ── Settings and requests ────────────────────────────────────────────────

/// The gear configuration the stream service reads (Appendix B).
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_field_names,
    reason = "field names follow the configuration keys"
)]
pub struct StreamSettings {
    /// `streaming.sse_channel_capacity`: bound of the event channel.
    pub sse_channel_capacity: usize,
    /// `rag.max_documents_per_chat`.
    pub max_documents_per_chat: u32,
    /// `rag.max_images_per_message`.
    pub max_images_per_message: u32,
    /// `quota.web_search_max_calls_per_message`.
    pub web_search_max_calls_per_message: u32,
    /// `quota.code_interpreter_max_calls_per_message`.
    pub code_interpreter_max_calls_per_message: u32,
    /// `context` (recent messages limit, tool guards).
    pub context: ContextConfig,
    /// `knowledge_search` (enablement, limits, guard).
    pub knowledge_search: KnowledgeSearchConfig,
    /// `thread_summary_worker.enabled`.
    pub summary_enabled: bool,
    /// `thread_summary_worker.compression_threshold_pct`.
    pub compression_threshold_pct: u32,
    /// Minimum interval between `last_progress_at` refreshes (30 s).
    pub progress_interval: Duration,
}

impl StreamSettings {
    /// The stream settings of the gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            sse_channel_capacity: usize::from(config.streaming.sse_channel_capacity.max(1)),
            max_documents_per_chat: config.rag.max_documents_per_chat,
            max_images_per_message: config.rag.max_images_per_message,
            web_search_max_calls_per_message: config.quota.web_search_max_calls_per_message,
            code_interpreter_max_calls_per_message: config
                .quota
                .code_interpreter_max_calls_per_message,
            context: config.context.clone(),
            knowledge_search: config.knowledge_search.clone(),
            summary_enabled: config.thread_summary_worker.enabled,
            compression_threshold_pct: config.thread_summary_worker.compression_threshold_pct,
            progress_interval: PROGRESS_REFRESH_INTERVAL,
        }
    }
}

/// `POST /v1/chats/{id}/messages:stream` body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendRequest {
    pub content: String,
    /// Client idempotency key; a UUID v4 is generated when absent.
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled`.
    pub web_search_enabled: bool,
}

/// Request-scoped options of a send.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendOptions {
    /// Trace id of the HTTP request, recorded in the turn audit event.
    pub trace_id: Option<String>,
}

/// The live part of a new turn, handed to the SSE relay.
///
/// The relay sends `first`, then relays `rx` until it yields a terminal
/// event or closes. Dropping the relay must cancel `cancel` (and drop `rx`):
/// the provider task then finalizes the turn as `cancelled` and sends
/// nothing more. When `rx` closes without a terminal event while the client
/// is still connected, the relay sends `stream_interrupted`.
#[derive(Debug)]
pub struct LiveStream {
    pub first: StreamStarted,
    /// Bounded by `streaming.sse_channel_capacity`.
    pub rx: mpsc::Receiver<StreamEvent>,
    pub cancel: CancellationToken,
}

/// Outcome of a successful send setup.
#[derive(Debug)]
pub enum StreamStart {
    /// Idempotent replay of a completed turn: `stream_started` (`is_new_turn
    /// = false`), one text `delta`, `done`.
    Replay(Vec<StreamEvent>),
    /// A new turn is running.
    Live(LiveStream),
}

/// Inputs of the retry / edit preflight (DESIGN section 3.6 "Retry / edit
/// variant", step 2).
#[derive(Debug, Clone, Copy)]
pub struct MutationPreflightInput<'a> {
    /// The scope of the mutation preview (no further PDP call).
    pub scope: &'a AccessScope,
    pub chat: &'a ChatRecord,
    /// The content of the replacement user message.
    pub content: &'a str,
    /// `web_search_enabled` of the replaced turn.
    pub web_search_enabled: bool,
    /// `request_id` of the replaced turn, left out of
    /// `prior_context_tokens` (and of the snapshot boundary).
    pub replaced_request_id: Uuid,
    /// The replaced turn's user message, whose live images are re-sent.
    pub source_message_id: Option<Uuid>,
}

/// The decision of the retry / edit preflight, reused after the mutation
/// commit (no second cascade).
#[derive(Debug, Clone)]
pub struct MutationPreflight {
    pub decision: PreflightDecision,
}

/// The `running` replacement turn committed by the mutation (NULL preflight
/// columns until the reserve).
#[derive(Debug, Clone)]
pub struct MutationTurn {
    /// The scope of the mutation preview.
    pub scope: AccessScope,
    pub chat: ChatRecord,
    pub turn_id: Uuid,
    /// Server-generated UUID v4.
    pub request_id: Uuid,
    /// The replacement user message (its attachment links are the copied
    /// ones).
    pub user_message_id: Uuid,
    pub content: String,
}

// ── Service ──────────────────────────────────────────────────────────────

/// Stream domain service.
pub struct StreamService {
    db: Db,
    repos: Repositories,
    quota: Arc<QuotaService>,
    authz: Arc<AuthzService>,
    resolver: Arc<dyn LlmProviderResolver>,
    provider: Arc<dyn LlmProvider>,
    settings: StreamSettings,
    /// Knowledge retriever, set only when knowledge search is enabled.
    knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

impl std::fmt::Debug for StreamService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamService")
            .field("settings", &self.settings)
            .field("knowledge_retriever", &self.knowledge.is_some())
            .finish_non_exhaustive()
    }
}

/// Facts read before the preflight.
struct ChatFacts {
    prior_context_tokens: u64,
    summary: Option<ThreadSummaryRecord>,
    recent: Vec<HistoryMessage>,
    ready: Vec<ChatAttachmentRecord>,
    vector_store_id: Option<String>,
    /// Valid image attachments of the request, in request order.
    image_file_ids: Vec<String>,
    /// Secondary (Anthropic Files API) copies of those images, by primary
    /// file id.
    secondary_file_ids: BTreeMap<String, String>,
    image_count: u32,
}

impl ChatFacts {
    /// Sets the images of the current user message, in order.
    fn with_images<'a>(mut self, images: impl Iterator<Item = &'a ChatAttachmentRecord>) -> Self {
        let images: Vec<&ChatAttachmentRecord> = images.collect();
        self.image_file_ids = images
            .iter()
            .filter_map(|a| a.provider_file_id.clone())
            .collect();
        self.secondary_file_ids = images
            .iter()
            .filter_map(|a| a.provider_file_id.clone().zip(a.secondary_file_id.clone()))
            .collect();
        self.image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        self
    }

    fn has_ready_documents(&self) -> bool {
        self.vector_store_id.is_some()
            && self
                .ready
                .iter()
                .any(|a| a.kind == AttachmentKind::Document && a.for_file_search)
    }

    fn code_interpreter_file_ids(&self) -> Vec<String> {
        self.ready
            .iter()
            .filter(|a| a.for_code_interpreter)
            .filter_map(|a| a.provider_file_id.clone())
            .collect()
    }

    fn cited_files(&self) -> HashMap<String, CitedFile> {
        self.ready
            .iter()
            .filter_map(|a| {
                a.provider_file_id.clone().map(|file_id| {
                    (
                        file_id,
                        CitedFile {
                            attachment_id: a.id,
                            filename: a.filename.clone(),
                        },
                    )
                })
            })
            .collect()
    }
}

/// Tools actually sent with the effective model.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent flag per built-in tool"
)]
struct SentTools {
    tools: Vec<LlmTool>,
    file_search: bool,
    web_search: bool,
    code_interpreter: bool,
    /// Knowledge search of the turn (`search_knowledge` sent).
    knowledge: Option<KnowledgeTurn>,
}

impl SentTools {
    /// Adds `search_knowledge` unless `file_search` is sent (`file_search`
    /// wins: the two tools are never sent together).
    fn with_knowledge(mut self, knowledge: Option<KnowledgeTurn>) -> Self {
        if self.file_search {
            return self;
        }
        if let Some(knowledge) = knowledge {
            self.tools.push(LlmTool::Function(search_knowledge_tool(
                knowledge.limits.top_k,
            )));
            self.knowledge = Some(knowledge);
        }
        self
    }
}

/// The writes of the reserve transaction.
struct ReserveWrite {
    owner: QuotaOwner,
    scope: AccessScope,
    chat_id: Uuid,
    decision: PreflightDecision,
    user_message: NewUserMessage,
    attachment_ids: Vec<Uuid>,
    turn: NewRunningTurn,
}

impl StreamService {
    /// Builds the service over the gear database (transactions), the
    /// persistence ports, the quota service, the PEP, the provider resolver,
    /// the LLM provider and the stream settings.
    #[must_use]
    pub fn new(
        db: Db,
        repos: Repositories,
        quota: Arc<QuotaService>,
        authz: Arc<AuthzService>,
        resolver: Arc<dyn LlmProviderResolver>,
        provider: Arc<dyn LlmProvider>,
        settings: StreamSettings,
    ) -> Self {
        Self {
            db,
            repos,
            quota,
            authz,
            resolver,
            provider,
            settings,
            knowledge: None,
        }
    }

    /// Sets the knowledge retriever (wired only when
    /// `knowledge_search.enabled = true`).
    #[must_use]
    pub fn with_knowledge_retriever(mut self, retriever: Arc<dyn KnowledgeRetriever>) -> Self {
        self.knowledge = Some(retriever);
        self
    }

    /// The tools sent with the effective model, `search_knowledge` included
    /// when knowledge search is on for the request and `file_search` is not
    /// sent.
    fn tools_for(
        &self,
        decision: &PreflightDecision,
        effective: &ModelCatalogEntry,
        facts: &ChatFacts,
        tenant_id: Uuid,
    ) -> SentTools {
        let sent = sent_tools(decision, effective, facts);
        if sent.file_search {
            return sent;
        }
        let knowledge = KnowledgeTurn::for_request(
            &self.settings.knowledge_search,
            self.knowledge.as_ref(),
            tenant_id,
        );
        sent.with_knowledge(knowledge)
    }

    /// The stream settings.
    #[must_use]
    pub const fn settings(&self) -> &StreamSettings {
        &self.settings
    }

    /// `POST /v1/chats/{id}/messages:stream` setup (module docs).
    ///
    /// # Errors
    ///
    /// Every pre-stream rejection of the REST error table: `EMPTY_CONTENT`,
    /// `invalid_attachment`, PEP errors, `404` (chat), `INVALID_MODEL`,
    /// `409` (`request_id_conflict`, `turn_already_running`),
    /// `TOO_MANY_IMAGES`, `FEATURE_DISABLED`, `429 quota_exceeded`,
    /// `VISION_NOT_SUPPORTED`, `INPUT_TOO_LONG`, `CONTEXT_BUDGET_EXCEEDED`;
    /// [`DomainError::Internal`] on plugin, provider resolution or storage
    /// failure.
    pub async fn send(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request: SendRequest,
        options: SendOptions,
    ) -> Result<StreamStart, DomainError> {
        self.validate_request(&request)?;
        let scope = self
            .authz
            .authorize_chat(&ctx, Action::SendMessage, Some(chat_id))
            .await?;
        let chat = self
            .repos
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let user_id = ctx.subject_id();

        let request_id = match request.request_id {
            Some(request_id) => {
                if let Some(turn) = self
                    .repos
                    .turns
                    .find_by_request_any(&scope, chat_id, request_id)
                    .await?
                {
                    return replay_or_conflict(
                        self.repos.messages.as_ref(),
                        &scope,
                        &chat,
                        request_id,
                        &turn,
                    )
                    .await
                    .map(StreamStart::Replay);
                }
                request_id
            }
            None => Uuid::new_v4(),
        };
        if self.repos.turns.running_exists(&scope, chat_id).await? {
            return Err(DomainError::aborted(AbortReason::TurnAlreadyRunning));
        }

        let policy = self.quota.load_policy(user_id).await?;
        chat_model(&policy, &chat.model)?;
        let facts = self
            .chat_facts(&scope, &ctx, chat_id, &request.attachment_ids)
            .await?;
        let decision = self
            .quota_preflight(
                &ctx,
                &chat.model,
                &policy,
                &facts,
                &request.content,
                request.web_search_enabled,
            )
            .await?;
        let effective = decision.effective_model.clone();

        let sent = self.tools_for(&decision, &effective, &facts, ctx.subject_tenant_id());
        let plan = self.assemble_context(&effective, &decision, &facts, &sent, &request.content)?;
        let summary_due = self.summary_due(&plan, &facts);
        let target = self
            .resolver
            .resolve(&effective.provider_id, ctx.subject_tenant_id())?;
        let cited_files = if sent.file_search {
            facts.cited_files()
        } else {
            HashMap::new()
        };
        let knowledge = sent.knowledge.clone();
        let llm_request = build_request(
            &ctx,
            chat_id,
            &effective,
            &decision,
            &plan,
            &facts,
            sent,
            &request.content,
        );

        // One instant for the reserve periods and `started_at` (DESIGN quota
        // period boundary invariant): the orphan watchdog derives the periods
        // of its settlement from `started_at`.
        let now = decision.at;
        let owner = QuotaOwner {
            tenant_id: ctx.subject_tenant_id(),
            user_id,
        };
        let turn = Arc::new(TurnContext {
            turn_id: Uuid::new_v4(),
            request_id,
            chat_id,
            tenant_id: ctx.subject_tenant_id(),
            user_id,
            scope: scope.clone(),
            assistant_message_id: Uuid::new_v4(),
            selected_model: chat.model.clone(),
            decision: decision.clone(),
            summary_due,
            trace_id: options.trace_id,
        });
        self.reserve(ReserveWrite {
            owner,
            scope: scope.clone(),
            chat_id,
            decision: decision.clone(),
            user_message: NewUserMessage {
                id: Uuid::new_v4(),
                tenant_id: ctx.subject_tenant_id(),
                chat_id,
                request_id,
                content: request.content,
                created_at: now,
            },
            attachment_ids: request.attachment_ids,
            turn: NewRunningTurn {
                id: turn.turn_id,
                tenant_id: ctx.subject_tenant_id(),
                chat_id,
                request_id,
                requester_user_id: user_id,
                reserve_tokens: decision.reserve_tokens,
                max_output_tokens_applied: decision.max_output_tokens_applied,
                reserved_credits_micro: decision.reserved_credits_micro,
                policy_version_applied: decision.policy_version_applied,
                effective_model: decision.effective_model_id().to_owned(),
                minimal_generation_floor_applied: decision.minimal_generation_floor_applied,
                web_search_enabled: request.web_search_enabled,
                now,
            },
        })
        .await?;
        Ok(StreamStart::Live(self.launch(
            turn,
            plan.summary_applied,
            target,
            llm_request,
            cited_files,
            knowledge,
        )))
    }

    /// Retry / edit step 2 (module docs): the read-only preflight of the
    /// replacement turn, run before the mutation commit.
    ///
    /// # Errors
    ///
    /// `INVALID_MODEL`, `TOO_MANY_IMAGES`, `FEATURE_DISABLED` (web search,
    /// images), `429 quota_exceeded`, `VISION_NOT_SUPPORTED`,
    /// `INPUT_TOO_LONG`; [`DomainError::Internal`] on plugin or storage
    /// failure.
    pub async fn mutation_preflight(
        &self,
        ctx: &SecurityContext,
        input: MutationPreflightInput<'_>,
    ) -> Result<MutationPreflight, DomainError> {
        let policy = self.quota.load_policy(ctx.subject_id()).await?;
        chat_model(&policy, &input.chat.model)?;
        let images = match input.source_message_id {
            Some(message_id) => {
                self.repos
                    .attachments
                    .live_for_message(input.scope, input.chat.id, message_id)
                    .await?
            }
            None => Vec::new(),
        };
        let facts = self
            .history_facts(input.scope, input.chat.id, Some(input.replaced_request_id))
            .await?
            .with_images(
                images
                    .iter()
                    .filter(|a| image_usable(a, ctx.subject_id(), input.chat.id)),
            );
        let decision = self
            .quota_preflight(
                ctx,
                &input.chat.model,
                &policy,
                &facts,
                input.content,
                input.web_search_enabled,
            )
            .await?;
        Ok(MutationPreflight { decision })
    }

    /// Retry / edit steps 4 and 5 (module docs): context assembly, provider
    /// resolution and the reserve of the committed replacement turn, then the
    /// provider task. A failure marks the turn `failed` (plain CAS) and is
    /// returned.
    ///
    /// # Errors
    ///
    /// `CONTEXT_BUDGET_EXCEEDED`, `429 quota_exceeded` (reserve re-check);
    /// [`DomainError::Internal`] on provider resolution or storage failure.
    pub async fn start_mutation(
        &self,
        ctx: SecurityContext,
        turn: MutationTurn,
        preflight: MutationPreflight,
        options: SendOptions,
    ) -> Result<StreamStart, DomainError> {
        let scope = turn.scope.clone();
        let turn_id = turn.turn_id;
        match self
            .setup_mutation(&ctx, turn, preflight.decision, options)
            .await
        {
            Ok(live) => Ok(StreamStart::Live(live)),
            Err(err) => {
                let code = setup_error_code(&err);
                tracing::info!(
                    target: "mini_chat.stream",
                    turn_id = %turn_id,
                    error_code = code,
                    error = %err,
                    "retry/edit turn setup failed after the mutation commit"
                );
                self.fail_unstarted(&scope, turn_id, code).await;
                Err(err)
            }
        }
    }

    /// Marks the unstarted retry / edit turn `turn_id` `failed` with `code`
    /// (plain CAS), retried up to [`FAIL_UNSTARTED_ATTEMPTS`] times: a turn
    /// left `running` with NULL preflight columns would block the chat.
    async fn fail_unstarted(&self, scope: &AccessScope, turn_id: Uuid, code: &'static str) {
        let mut backoff = FAIL_UNSTARTED_BACKOFF;
        for attempt in 1..=FAIL_UNSTARTED_ATTEMPTS {
            match self
                .repos
                .turns
                .fail_unstarted(scope, turn_id, code, storage_now())
                .await
            {
                Ok(_) => return,
                Err(err) if attempt < FAIL_UNSTARTED_ATTEMPTS => {
                    tracing::warn!(
                        target: "mini_chat.stream",
                        turn_id = %turn_id,
                        error_code = code,
                        attempt,
                        error = %err,
                        "unstarted retry/edit turn could not be marked failed, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
                Err(err) => tracing::error!(
                    target: "mini_chat.stream",
                    turn_id = %turn_id,
                    error_code = code,
                    attempts = FAIL_UNSTARTED_ATTEMPTS,
                    error = %err,
                    "unstarted retry/edit turn could not be marked failed"
                ),
            }
        }
    }

    async fn setup_mutation(
        &self,
        ctx: &SecurityContext,
        turn: MutationTurn,
        decision: PreflightDecision,
        options: SendOptions,
    ) -> Result<LiveStream, DomainError> {
        let chat_id = turn.chat.id;
        let user_id = ctx.subject_id();
        let images = self
            .repos
            .attachments
            .live_for_message(&turn.scope, chat_id, turn.user_message_id)
            .await?;
        let facts = self
            .history_facts(&turn.scope, chat_id, Some(turn.request_id))
            .await?
            .with_images(images.iter().filter(|a| image_usable(a, user_id, chat_id)));
        let effective = decision.effective_model.clone();
        let sent = self.tools_for(&decision, &effective, &facts, ctx.subject_tenant_id());
        let plan = self.assemble_context(&effective, &decision, &facts, &sent, &turn.content)?;
        let summary_due = self.summary_due(&plan, &facts);
        let target = self
            .resolver
            .resolve(&effective.provider_id, ctx.subject_tenant_id())?;
        let cited_files = if sent.file_search {
            facts.cited_files()
        } else {
            HashMap::new()
        };
        let knowledge = sent.knowledge.clone();
        let llm_request = build_request(
            ctx,
            chat_id,
            &effective,
            &decision,
            &plan,
            &facts,
            sent,
            &turn.content,
        );
        let owner = QuotaOwner {
            tenant_id: ctx.subject_tenant_id(),
            user_id,
        };
        self.reserve_mutation(owner, &turn.scope, &decision, turn.turn_id)
            .await?;
        let context = Arc::new(TurnContext {
            turn_id: turn.turn_id,
            request_id: turn.request_id,
            chat_id,
            tenant_id: ctx.subject_tenant_id(),
            user_id,
            scope: turn.scope,
            assistant_message_id: Uuid::new_v4(),
            selected_model: turn.chat.model,
            decision,
            summary_due,
            trace_id: options.trace_id,
        });
        Ok(self.launch(
            context,
            plan.summary_applied,
            target,
            llm_request,
            cited_files,
            knowledge,
        ))
    }

    /// Steps 7 and 8 of the module docs: `TOO_MANY_IMAGES`, the quota
    /// preflight, the image guards and `INPUT_TOO_LONG`.
    async fn quota_preflight(
        &self,
        ctx: &SecurityContext,
        selected_model: &str,
        policy: &PolicyContext,
        facts: &ChatFacts,
        content: &str,
        web_search_enabled: bool,
    ) -> Result<PreflightDecision, DomainError> {
        if facts.image_count > self.settings.max_images_per_message {
            return Err(DomainError::out_of_range(OutOfRangeReason::TooManyImages));
        }
        let owner = QuotaOwner {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
        };
        let decision = self
            .quota
            .preflight(PreflightInput {
                owner,
                selected_model,
                policy_version: policy.policy_version,
                snapshot: &policy.snapshot,
                user_limits: &policy.user_limits,
                request: PreflightRequest {
                    message_utf8_bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
                    image_count: facts.image_count,
                    web_search_requested: web_search_enabled,
                    has_ready_documents: facts.has_ready_documents(),
                    has_ready_code_interpreter_files: !facts.code_interpreter_file_ids().is_empty(),
                },
                prior_context_tokens: facts.prior_context_tokens,
                now: storage_now(),
            })
            .await?;
        if facts.image_count > 0 {
            if policy.snapshot.kill_switches.disable_images {
                return Err(DomainError::failed_precondition(
                    PreconditionSubject::Images,
                ));
            }
            if !decision.vision_supported {
                return Err(DomainError::invalid_argument(
                    InvalidArgumentReason::VisionNotSupported,
                ));
            }
        }
        let effective = &decision.effective_model;
        check_input_length(
            content,
            effective.max_input_tokens,
            &effective.estimation_budgets,
        )?;
        Ok(decision)
    }

    fn summary_due(&self, plan: &ContextPlan, facts: &ChatFacts) -> bool {
        summary_trigger(
            plan.assembled_tokens,
            plan.effective_budget,
            self.settings.compression_threshold_pct,
            facts.summary.is_some(),
            plan.messages_truncated,
            self.settings.summary_enabled,
        ) != crate::domain::context::SummaryTrigger::None
    }

    /// Spawns the provider task of `turn` and returns its live stream.
    fn launch(
        &self,
        turn: Arc<TurnContext>,
        thread_summary_applied: Option<u32>,
        target: crate::domain::llm::ProviderTarget,
        request: LlmRequest,
        cited_files: HashMap<String, CitedFile>,
        knowledge: Option<KnowledgeTurn>,
    ) -> LiveStream {
        let (events, rx) = mpsc::channel(self.settings.sse_channel_capacity);
        let cancel = CancellationToken::new();
        let first = StreamStarted {
            request_id: turn.request_id,
            message_id: turn.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied,
        };
        tracing::info!(
            target: "mini_chat.stream",
            chat_id = %turn.chat_id,
            turn_id = %turn.turn_id,
            request_id = %turn.request_id,
            effective_model = %turn.decision.effective_model.id,
            "turn started"
        );
        let task = ProviderTask {
            turn,
            finalizer: Finalizer::new(self.db.clone(), self.repos.clone(), Arc::clone(&self.quota)),
            turns: Arc::clone(&self.repos.turns),
            quota: Arc::clone(&self.quota),
            provider: Arc::clone(&self.provider),
            target,
            request,
            limits: TaskLimits {
                web_search_max_calls: self.settings.web_search_max_calls_per_message,
                code_interpreter_max_calls: self.settings.code_interpreter_max_calls_per_message,
                progress_interval: self.settings.progress_interval,
            },
            cited_files,
            knowledge,
            events,
            cancel: cancel.clone(),
        };
        tokio::spawn(task.run());
        LiveStream { first, rx, cancel }
    }

    fn validate_request(&self, request: &SendRequest) -> Result<(), DomainError> {
        if request.content.trim().is_empty() {
            return Err(DomainError::invalid_argument(
                InvalidArgumentReason::EmptyContent,
            ));
        }
        let distinct: HashSet<&Uuid> = request.attachment_ids.iter().collect();
        let max = u64::from(self.settings.max_documents_per_chat)
            + u64::from(self.settings.max_images_per_message);
        let count = u64::try_from(request.attachment_ids.len()).unwrap_or(u64::MAX);
        if distinct.len() != request.attachment_ids.len() || count > max {
            return Err(invalid_attachment());
        }
        Ok(())
    }

    async fn chat_facts(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<ChatFacts, DomainError> {
        let facts = self.history_facts(scope, chat_id, None).await?;
        let requested = self
            .repos
            .attachments
            .find_in_chat(scope, chat_id, attachment_ids)
            .await?;
        Ok(facts.with_images(
            attachment_ids
                .iter()
                .filter_map(|id| requested.iter().find(|a| a.id == *id))
                .filter(|a| image_usable(a, ctx.subject_id(), chat_id)),
        ))
    }

    /// The history and tool facts of `chat_id` without images. The snapshot
    /// boundary is the latest live message and `prior_context_tokens` the
    /// usage of the latest live assistant message with usage; with
    /// `exclude_request`, both skip the messages of that turn (the replaced
    /// turn in the retry / edit preflight, the replacement turn whose user
    /// message is already stored in the retry / edit setup).
    async fn history_facts(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        exclude_request: Option<Uuid>,
    ) -> Result<ChatFacts, DomainError> {
        let messages = &self.repos.messages;
        let boundary = match exclude_request {
            Some(request_id) => {
                messages
                    .latest_key_excluding_request(scope, chat_id, request_id)
                    .await?
            }
            None => messages.latest_key(scope, chat_id).await?,
        };
        let prior_context_tokens = messages
            .prior_context_tokens(scope, chat_id, exclude_request)
            .await?;
        let summary = messages.thread_summary(scope, chat_id).await?;
        let recent = match boundary {
            Some(boundary) => messages
                .recent_for_context(
                    scope,
                    chat_id,
                    boundary,
                    summary.as_ref().map(|s| s.frontier),
                    self.settings.context.recent_messages_limit,
                )
                .await?
                .into_iter()
                .filter_map(|m| {
                    let role = match m.role {
                        MessageRole::User => ContextRole::User,
                        MessageRole::Assistant => ContextRole::Assistant,
                        MessageRole::System => return None,
                    };
                    Some(HistoryMessage {
                        id: m.id,
                        role,
                        content: m.content,
                        created_at: m.created_at,
                    })
                })
                .collect(),
            None => Vec::new(),
        };
        let ready = self
            .repos
            .attachments
            .ready_for_chat(scope, chat_id)
            .await?;
        let vector_store_id = self
            .repos
            .attachments
            .vector_store_id(scope, chat_id)
            .await?;
        Ok(ChatFacts {
            prior_context_tokens,
            summary,
            recent,
            ready,
            vector_store_id,
            image_file_ids: Vec::new(),
            secondary_file_ids: BTreeMap::new(),
            image_count: 0,
        })
    }

    fn assemble_context(
        &self,
        effective: &ModelCatalogEntry,
        decision: &PreflightDecision,
        facts: &ChatFacts,
        sent: &SentTools,
        content: &str,
    ) -> Result<ContextPlan, DomainError> {
        let budgets = effective.estimation_budgets;
        assemble(ContextInput {
            system_prompt: effective.system_prompt.clone(),
            guards: select_guards(
                sent.file_search,
                sent.web_search,
                sent.knowledge.is_some(),
                &self.settings.context,
                &self.settings.knowledge_search,
            ),
            summary: facts.summary.as_ref().map(|summary| SummaryForContext {
                text: summary.text.clone(),
                token_estimate: u32::try_from(summary.token_estimate).unwrap_or(0),
            }),
            recent: facts.recent.clone(),
            user_message: content.to_owned(),
            image_count: facts.image_count,
            limits: ModelLimits {
                context_window: effective.context_window,
                max_input_tokens: effective.max_input_tokens,
                max_output_tokens_applied: decision.max_output_tokens_applied,
            },
            budgets,
            surcharges: ToolSurcharges::for_tools(
                &budgets,
                sent.file_search,
                sent.web_search,
                sent.code_interpreter,
            ),
        })
    }

    /// The reserve transaction (module docs, step 11).
    async fn reserve(&self, write: ReserveWrite) -> Result<(), DomainError> {
        let repos = self.repos.clone();
        let quota = Arc::clone(&self.quota);
        let scope = write.scope.clone();
        let chat_id = write.chat_id;
        let request_id = write.turn.request_id;
        let result = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    quota
                        .reserve_in_tx(tx, &write.decision, write.owner)
                        .await?;
                    let message_id = write.user_message.id;
                    let tenant_id = write.user_message.tenant_id;
                    let at = write.user_message.created_at;
                    repos
                        .messages
                        .insert_user_in_tx(tx, &write.scope, write.user_message)
                        .await?;
                    // A chat soft-deleted since the load must not receive a
                    // turn: the transaction rolls back with 404.
                    if !repos
                        .chats
                        .touch_in_tx(tx, &write.scope, write.chat_id, at)
                        .await?
                    {
                        return Err(chat_not_found(write.chat_id));
                    }
                    if !write.attachment_ids.is_empty() {
                        let rows = repos
                            .attachments
                            .find_in_chat_in_tx(
                                tx,
                                &write.scope,
                                write.chat_id,
                                &write.attachment_ids,
                            )
                            .await?;
                        let all_usable = write.attachment_ids.iter().all(|id| {
                            rows.iter().any(|row| {
                                row.id == *id
                                    && row.tenant_id == tenant_id
                                    && attachment_usable(
                                        row,
                                        write.turn.requester_user_id,
                                        write.chat_id,
                                    )
                            })
                        });
                        if !all_usable {
                            return Err(invalid_attachment());
                        }
                        repos
                            .attachments
                            .link_in_tx(
                                tx,
                                &write.scope,
                                MessageAttachmentLinks {
                                    tenant_id,
                                    chat_id: write.chat_id,
                                    message_id,
                                    attachment_ids: write.attachment_ids,
                                    at,
                                },
                            )
                            .await?;
                    }
                    repos
                        .turns
                        .insert_running_in_tx(tx, &write.scope, write.turn)
                        .await
                })
            })
            .await;
        match result {
            Err(DomainError::Aborted {
                reason: reason @ (AbortReason::TurnAlreadyRunning | AbortReason::RequestIdConflict),
            }) => {
                let taken = self
                    .repos
                    .turns
                    .find_by_request_any(&scope, chat_id, request_id)
                    .await?
                    .is_some();
                Err(DomainError::aborted(if taken {
                    AbortReason::RequestIdConflict
                } else {
                    reason
                }))
            }
            other => other,
        }
    }

    /// The reserve transaction of a retry / edit turn: the reserve increment
    /// first, the limit re-check, then the one-time write of the turn's
    /// preflight columns.
    async fn reserve_mutation(
        &self,
        owner: QuotaOwner,
        scope: &AccessScope,
        decision: &PreflightDecision,
        turn_id: Uuid,
    ) -> Result<(), DomainError> {
        let repos = self.repos.clone();
        let quota = Arc::clone(&self.quota);
        let scope = scope.clone();
        let decision = decision.clone();
        self.db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    quota.reserve_in_tx(tx, &decision, owner).await?;
                    let written = repos
                        .turns
                        .record_preflight_in_tx(
                            tx,
                            &scope,
                            turn_id,
                            TurnPreflightColumns {
                                reserve_tokens: decision.reserve_tokens,
                                max_output_tokens_applied: decision.max_output_tokens_applied,
                                reserved_credits_micro: decision.reserved_credits_micro,
                                policy_version_applied: decision.policy_version_applied,
                                effective_model: decision.effective_model_id().to_owned(),
                                minimal_generation_floor_applied: decision
                                    .minimal_generation_floor_applied,
                            },
                        )
                        .await?;
                    if written {
                        Ok(())
                    } else {
                        Err(DomainError::internal(format!(
                            "retry/edit turn {turn_id} is no longer running before its reserve"
                        )))
                    }
                })
            })
            .await
    }
}

// ── Replay (read-only path) ──────────────────────────────────────────────

/// The idempotency outcome of an existing turn for `request_id`: the replay
/// events of a completed, live turn, `409 request_id_conflict` otherwise.
///
/// This path only reads (the turn row and the persisted assistant message):
/// it has no access to the reserve, settlement or outbox.
///
/// # Errors
///
/// `409 aborted` `request_id_conflict` for a running, failed, cancelled or
/// soft-deleted turn; [`DomainError::Internal`] when a completed turn has no
/// persisted assistant message.
pub async fn replay_or_conflict(
    messages: &dyn MessageRepository,
    scope: &AccessScope,
    chat: &ChatRecord,
    request_id: Uuid,
    turn: &TurnKeyRecord,
) -> Result<Vec<StreamEvent>, DomainError> {
    if turn.deleted || turn.state != TurnState::Completed {
        return Err(DomainError::aborted(AbortReason::RequestIdConflict));
    }
    let message_id = turn.assistant_message_id.ok_or_else(|| {
        DomainError::internal(format!(
            "completed turn {} has no assistant message",
            turn.id
        ))
    })?;
    let message = messages
        .find_active(scope, chat.id, message_id)
        .await?
        .ok_or_else(|| {
            DomainError::internal(format!(
                "assistant message {message_id} of completed turn {} is missing",
                turn.id
            ))
        })?;
    let effective_model = message
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| chat.model.clone());
    let downgraded = effective_model != chat.model;
    Ok(vec![
        StreamEvent::StreamStarted(StreamStarted {
            request_id,
            message_id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        StreamEvent::Delta(Delta {
            kind: DeltaKind::Text,
            content: message.content,
        }),
        StreamEvent::Done(Box::new(Done {
            usage: DoneUsage {
                input_tokens: message.input_tokens,
                output_tokens: message.output_tokens,
            },
            effective_model,
            selected_model: chat.model.clone(),
            quota_decision: if downgraded {
                QuotaDecisionKind::Downgrade
            } else {
                QuotaDecisionKind::Allow
            },
            downgrade_from: downgraded.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        })),
    ])
}

// ── Helpers ──────────────────────────────────────────────────────────────

const fn invalid_attachment() -> DomainError {
    DomainError::invalid_argument(InvalidArgumentReason::InvalidAttachment)
}

/// The chat model in the user's catalog, regardless of `enabled`.
fn chat_model<'p>(
    policy: &'p PolicyContext,
    model_id: &str,
) -> Result<&'p ModelCatalogEntry, DomainError> {
    policy
        .snapshot
        .model_catalog
        .iter()
        .find(|entry| entry.id == model_id)
        .ok_or_else(|| DomainError::invalid_argument(InvalidArgumentReason::InvalidModel))
}

/// An attachment the caller may reference on a message of `chat_id`: same
/// chat, uploaded by the caller, `ready`, not deleted (the tenant comes from
/// the scope of the query).
fn attachment_usable(row: &ChatAttachmentRecord, user_id: Uuid, chat_id: Uuid) -> bool {
    row.chat_id == chat_id
        && row.uploaded_by_user_id == user_id
        && row.status == AttachmentStatus::Ready
        && !row.deleted
}

/// A usable image attachment of a message of `chat_id` ([`attachment_usable`]
/// and `kind = image`).
fn image_usable(row: &ChatAttachmentRecord, user_id: Uuid, chat_id: Uuid) -> bool {
    row.kind == AttachmentKind::Image && attachment_usable(row, user_id, chat_id)
}

/// `chat_turns.error_code` of an unstarted retry / edit turn whose setup
/// failed with `err` (DESIGN section 3.6 "Retry / edit variant").
#[must_use]
pub fn setup_error_code(err: &DomainError) -> &'static str {
    match err {
        DomainError::OutOfRange {
            reason: OutOfRangeReason::ContextBudgetExceeded,
            ..
        } => CONTEXT_LENGTH_EXCEEDED,
        DomainError::ResourceExhausted {
            limit: ExhaustedLimit::Quota(_),
        } => QUOTA_EXCEEDED,
        _ => TURN_SETUP_FAILED,
    }
}

/// The tools sent with the effective model: `file_search` with the chat's
/// vector store, `web_search` with the catalog context size,
/// `code_interpreter` with the ready XLSX files.
fn sent_tools(
    decision: &PreflightDecision,
    effective: &ModelCatalogEntry,
    facts: &ChatFacts,
) -> SentTools {
    let mut tools = Vec::new();
    let vector_store = facts
        .vector_store_id
        .clone()
        .filter(|_| decision.tools.file_search);
    let file_search = vector_store.is_some();
    if let Some(vector_store_id) = vector_store {
        tools.push(LlmTool::FileSearch {
            vector_store_id,
            max_num_results: effective.max_num_results,
        });
    }
    let web_search = decision.tools.web_search;
    if web_search {
        tools.push(LlmTool::WebSearch {
            search_context_size: effective.web_search_context_size.clone(),
        });
    }
    let file_ids = facts.code_interpreter_file_ids();
    let code_interpreter = decision.tools.code_interpreter && !file_ids.is_empty();
    if code_interpreter {
        tools.push(LlmTool::CodeInterpreter { file_ids });
    }
    SentTools {
        tools,
        file_search,
        web_search,
        code_interpreter,
        knowledge: None,
    }
}

/// The provider request of a turn: the planned context, then the current user
/// message with its images.
#[allow(
    clippy::too_many_arguments,
    reason = "assembles one request from the independent setup results"
)]
fn build_request(
    ctx: &SecurityContext,
    chat_id: Uuid,
    effective: &ModelCatalogEntry,
    decision: &PreflightDecision,
    plan: &ContextPlan,
    facts: &ChatFacts,
    sent: SentTools,
    content: &str,
) -> LlmRequest {
    let tenant_id = ctx.subject_tenant_id().to_string();
    let user_id = ctx.subject_id().to_string();
    let mut input: Vec<LlmInputItem> = plan
        .messages
        .iter()
        .map(|message| match message.role {
            ContextRole::User => LlmInputItem::User {
                text: message.content.clone(),
                image_file_ids: Vec::new(),
            },
            ContextRole::Assistant => LlmInputItem::Assistant {
                text: message.content.clone(),
            },
        })
        .collect();
    input.push(LlmInputItem::User {
        text: content.to_owned(),
        image_file_ids: facts.image_file_ids.clone(),
    });
    let feature = feature_label(&sent.tools);
    LlmRequest {
        model: effective.provider_model_id.clone(),
        instructions: plan.instructions.clone(),
        input,
        tools: sent.tools,
        max_output_tokens: decision.max_output_tokens_applied,
        max_tool_calls: effective.max_tool_calls,
        api_params: effective.general_config.api_params.clone(),
        user: user_field(&tenant_id, &user_id),
        metadata: LlmMetadata {
            tenant_id,
            user_id,
            chat_id: chat_id.to_string(),
            request_type: LlmRequestType::Chat,
            feature,
        },
        secondary_file_ids: facts.secondary_file_ids.clone(),
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "knowledge_loop_tests.rs"]
mod knowledge_loop_tests;
