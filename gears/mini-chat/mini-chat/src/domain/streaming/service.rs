//! The send setup service: validation, admission, replay, preflight, reserve and the hand-over
//! of the [`ProviderCallPlan`](crate::domain::streaming::ProviderCallPlan) (flows
//! `cpt-cf-mini-chat-flow-message-streaming-send-message` up to the pre-provider cancellation
//! check and `cpt-cf-mini-chat-flow-message-streaming-resend-request-id`).

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::attachments::contributions::{
    AttachmentToolSource, ChatToolInput, TurnContributions,
};
use crate::domain::authz::{Operation, PepService};
use crate::domain::chats::{ChatDetail, scope_of};
use crate::domain::error::{DomainError, StreamErrorCode};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::provider_registry::{EffectiveProvider, ProviderRegistry};
use crate::domain::quota::estimation::RequestFacts;
use crate::domain::quota::{PreflightDecision, PreflightRequest, QuotaService};
use crate::domain::streaming::admission::{AdmissionDecision, decide_admission};
use crate::domain::streaming::context::{
    ContextAssembly, CurrentMessage, ImageInput, assemble_context,
};
use crate::domain::streaming::finalizer::{TurnFinalizer, TurnOutcome, TurnTarget};
use crate::domain::streaming::image_guards::check_image_guards;
use crate::domain::streaming::plan::{
    ProviderCallPlan, ProviderTaskPort, SendOutcome, StartedTurn,
};
use crate::domain::streaming::replay::build_replay;
use crate::domain::streaming::request::{
    AttachmentLimits, RawSendBody, ValidatedSend, validate_send,
};
use crate::domain::streaming::store::{
    AttachmentFact, ContextReads, ReserveCommand, ReservedTurn, SnapshotBoundary, StreamStore,
};
use crate::domain::streaming::tools::{
    FileSearchSpec, ToolContribution, ToolDefinition, ToolKind, ToolPart, ToolSet, ToolTurnFacts,
    assemble_tools,
};

/// `attachments.kind` of an image.
const IMAGE_KIND: &str = "image";

/// The configuration values the send setup reads (`cpt-cf-mini-chat-algo-message-streaming-configuration`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSettings {
    /// `context.recent_messages_limit`.
    pub recent_messages_limit: u32,
    /// `context.web_search_guard`.
    pub web_search_guard: String,
    /// `context.file_search_guard`.
    pub file_search_guard: String,
    /// `rag.max_images_per_message`.
    pub max_images_per_message: u32,
    /// `rag.max_documents_per_chat`.
    pub max_documents_per_chat: u32,
    /// `streaming.sse_ping_interval_seconds`: the idle time before a `ping` event.
    pub sse_ping_interval: Duration,
}

impl StreamSettings {
    /// The values of the validated gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-context
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-rag
        // the values were validated (range and defaults) by the foundation; this feature adds
        // no key
        Self {
            recent_messages_limit: config.context.recent_messages_limit,
            web_search_guard: config.context.web_search_guard.clone(),
            file_search_guard: config.context.file_search_guard.clone(),
            max_images_per_message: config.rag.max_images_per_message,
            max_documents_per_chat: config.rag.max_documents_per_chat,
            sse_ping_interval: Duration::from_secs(u64::from(
                config.streaming.sse_ping_interval_seconds,
            )),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-rag
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-context
    }
}

/// The result of the send preflight: everything up to the reserve transaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedTurn {
    pub decision: PreflightDecision,
    /// The catalog entry of the effective model.
    pub model: ModelCatalogEntry,
    /// The images of the request in `attachment_ids` order.
    pub images: Vec<ImageInput>,
    pub context: ContextAssembly,
    pub tools: ToolSet,
    pub provider: EffectiveProvider,
    /// Pre-allocated UUID v4.
    pub assistant_message_id: Uuid,
}

/// What the post-commit setup of a retry or an edit assembles before the reserve.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ReplacementAssembly {
    pub tools: ToolSet,
    pub context: ContextAssembly,
    pub provider: EffectiveProvider,
}

/// The facts of the mutation preflight that the post-commit setup of a retry or an edit
/// assembles from.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct ReplacementRequest<'a> {
    /// The server-generated `request_id` of the replacement turn.
    pub request_id: Uuid,
    /// The replacement user message content.
    pub content: &'a str,
    /// The carried images, in the original link order.
    pub carried_images: &'a [AttachmentFact],
    /// The carried `web_search.enabled`.
    pub web_search_enabled: bool,
    pub decision: &'a PreflightDecision,
    /// The catalog entry of the effective model.
    pub model: &'a ModelCatalogEntry,
    /// The snapshot boundary of the mutation preflight.
    pub boundary: Option<&'a SnapshotBoundary>,
}

/// The model input of an image attachment: its internal provider file id (an image that has none
/// cannot be sent to the model).
fn image_input(attachment: &AttachmentFact) -> Result<ImageInput, DomainError> {
    let provider_file_id = attachment
        .provider_file_id
        .clone()
        .ok_or(DomainError::InvalidAttachment)?;
    Ok(ImageInput {
        attachment_id: attachment.id,
        provider_file_id,
        secondary_file_id: attachment.usable_secondary_file_id().map(str::to_owned),
    })
}

/// A contribution of the attachments feature decided for this turn.
struct DecidedContribution {
    kind: ToolKind,
    part: ToolPart,
}

impl ToolContribution for DecidedContribution {
    fn kind(&self) -> ToolKind {
        self.kind
    }

    fn contribute(&self, _facts: &ToolTurnFacts) -> Option<ToolPart> {
        Some(self.part.clone())
    }
}

fn part_of(
    kind: ToolKind,
    guard: Option<String>,
    include: Option<String>,
) -> Arc<dyn ToolContribution> {
    Arc::new(DecidedContribution {
        kind,
        part: ToolPart {
            definition: ToolDefinition {
                kind,
                search_context_size: None,
            },
            guard,
            include,
        },
    })
}

/// The tool parts of the attachments feature for this turn, in the evaluation order.
fn turn_contributions(turn: &TurnContributions) -> Vec<Arc<dyn ToolContribution>> {
    let mut parts = Vec::new();
    if let Some(file_search) = &turn.file_search {
        parts.push(part_of(
            ToolKind::FileSearch,
            Some(file_search.guard.clone()),
            None,
        ));
    }
    if let Some(code_interpreter) = &turn.code_interpreter {
        parts.push(part_of(
            ToolKind::CodeInterpreter,
            None,
            Some(code_interpreter.include.clone()),
        ));
    }
    if let Some(knowledge) = &turn.knowledge {
        parts.push(part_of(
            ToolKind::SearchKnowledge,
            Some(knowledge.guard.clone()),
            None,
        ));
    }
    parts
}

/// The data of the assembled tools: the chat store, the container files, the citation map and
/// the tool-result format.
fn apply_turn_contributions(tools: &mut ToolSet, turn: TurnContributions) {
    let has = |kind: ToolKind| tools.tools.iter().any(|tool| tool.kind == kind);
    if has(ToolKind::FileSearch)
        && let Some(file_search) = turn.file_search
    {
        tools.file_search = Some(FileSearchSpec {
            vector_store_ids: file_search.vector_store_ids,
            max_num_results: file_search.max_num_results,
        });
        tools.citation_files = file_search.citation_files;
    }
    if has(ToolKind::CodeInterpreter)
        && let Some(code_interpreter) = turn.code_interpreter
    {
        tools.code_interpreter_files = code_interpreter.file_ids;
    }
    if has(ToolKind::SearchKnowledge)
        && let Some(knowledge) = turn.knowledge
    {
        tools.knowledge_format = Some(knowledge.format);
    }
}

/// The send setup service.
#[domain_model]
pub struct MessageStreamService {
    pub pep: Arc<PepService>,
    pub gateway: Arc<dyn ModelPolicyGateway>,
    pub store: Arc<dyn StreamStore>,
    pub quota: Arc<QuotaService>,
    pub providers: Arc<ProviderRegistry>,
    pub provider_task: Arc<dyn ProviderTaskPort>,
    /// The finalization of a committed turn that never reached a provider response.
    pub finalizer: Arc<dyn TurnFinalizer>,
    /// The tool contributions registered by other features.
    pub contributions: Vec<Arc<dyn ToolContribution>>,
    /// The `file_search`, `code_interpreter` and `search_knowledge` contributions of
    /// `cpt-cf-mini-chat-feature-attachments-and-file-search`, evaluated per turn in that order.
    pub attachment_tools: Option<Arc<AttachmentToolSource>>,
    pub settings: StreamSettings,
}

impl MessageStreamService {
    /// Create the service over its collaborators, without tool contributions.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the collaborators of the send pipeline are injected one by one"
    )]
    pub fn new(
        pep: Arc<PepService>,
        gateway: Arc<dyn ModelPolicyGateway>,
        store: Arc<dyn StreamStore>,
        quota: Arc<QuotaService>,
        providers: Arc<ProviderRegistry>,
        provider_task: Arc<dyn ProviderTaskPort>,
        finalizer: Arc<dyn TurnFinalizer>,
        settings: StreamSettings,
    ) -> Self {
        Self {
            pep,
            gateway,
            store,
            quota,
            providers,
            provider_task,
            finalizer,
            contributions: Vec::new(),
            attachment_tools: None,
            settings,
        }
    }

    /// Register the attachment tool contributions (`file_search`, `code_interpreter`,
    /// `search_knowledge`) of FEATURE 0005; the send setup consults the source after the downgrade
    /// cascade, so the surcharge facts and the tools come from the same predicates.
    #[must_use]
    pub fn with_attachment_tools(mut self, tools: Arc<AttachmentToolSource>) -> Self {
        self.attachment_tools = Some(tools);
        self
    }

    /// Register the tool contributions of other features.
    #[must_use]
    pub fn with_contributions(mut self, contributions: Vec<Arc<dyn ToolContribution>>) -> Self {
        self.contributions = contributions;
        self
    }

    /// The send request validation with the limits of the configuration; runs before the PEP
    /// call and before the chat is loaded.
    ///
    /// # Errors
    ///
    /// `EmptyContent` or `InvalidAttachment`.
    pub fn validate(&self, body: RawSendBody) -> Result<ValidatedSend, DomainError> {
        validate_send(
            body,
            AttachmentLimits {
                max_documents_per_chat: self.settings.max_documents_per_chat,
                max_images_per_message: self.settings.max_images_per_message,
            },
        )
    }

    /// The send setup: admission with the action `send_message` only, the owner-scoped chat,
    /// the `request_id` resolution, the turn admission checks (replay before the parallel-turn
    /// guard), the preflight, the reserve transaction with its post-commit metrics, the
    /// pre-provider cancellation check (the turn is finalized `cancelled`) and the hand-over of
    /// the plan to the provider task.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { Chat }`, `Conflict`, every preflight rejection
    /// (`InvalidModel`, `PolicyUnavailable`, `TooManyImages`, `QuotaExceeded`, `FeatureDisabled`,
    /// `InputTooLong`, `VisionNotSupported`, `ContextBudgetExceeded`, `Internal`) and every
    /// reserve rejection (on those nothing remains written and no provider task started), the
    /// error of a provider task that could not start (its committed turn was finalized `failed`
    /// with `provider_error` first) and a finalization failure.
    pub async fn send(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        send: ValidatedSend,
        cancel: &CancellationToken,
    ) -> Result<SendOutcome, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-admission
        // `send_message` only (no `read` evaluation): the decision covers the whole session
        let admission = self
            .pep
            .authorize(ctx, Operation::SendMessage, Some(chat_id))
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-refusal
        // a refusal, or the not-found of the owner-scoped chat load (a missing, soft-deleted,
        // foreign-user or foreign-tenant chat), is returned as it is: no stream, nothing written
        let scope = scope_of(admission?)?;
        let chat = self.store.load_chat(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-refused
        let tenant_id = ctx.subject_tenant_id();

        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-resolve-request-id
        // the client value (any UUID version), otherwise a server-generated UUID v4
        let supplied = send.request_id.is_some();
        let request_id = send.request_id.unwrap_or_else(Uuid::new_v4);
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-resolve-request-id

        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-admission-checks
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-checks
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-select-key
        // the key lookup includes soft-deleted rows and runs only for a client-supplied key
        let existing = if supplied {
            self.store.find_turn(tenant_id, chat.id, request_id).await?
        } else {
            None
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-select-key
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-select-running
        // the parallel-turn guard is read only when no row matched the key, so a row of the
        // key (replay or conflict) is decided before it
        let running_exists = if existing.is_some() {
            false
        } else {
            self.store.has_running_turn(tenant_id, chat.id).await?
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-select-running
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-generic-detail
        // every 409 carries a generic detail (the error mapping); the internal cause is logged
        let decision = decide_admission(supplied, existing.as_ref(), running_exists);
        if let Err(DomainError::Conflict { reason }) = &decision {
            tracing::info!(chat_id = %chat.id, reason = reason.as_str(), "send refused by the turn admission checks");
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-generic-detail
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-conflict
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-409
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-other-state
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-return-409
        // a rejection is the 409 `aborted` Problem (`request_id_conflict` or
        // `turn_already_running`); no stream is opened
        let decision = decision?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-return-409
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-other-state
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-409
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-conflict
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-checks
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-admission
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-admission-checks

        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-is-replay
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-replay
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-completed
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-return-replay
        if let AdmissionDecision::Replay(turn) = decision {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-separate-path
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-no-side-effects
            // the replay path has no access to the quota reserve, the settlement, the outbox,
            // the audit enqueue or the provider: it only reads the persisted assistant message
            let assistant = match turn.assistant_message_id {
                Some(message_id) => {
                    self.store
                        .load_message(tenant_id, chat.id, message_id)
                        .await?
                }
                None => None,
            };
            let events = build_replay(&turn, assistant.as_ref(), &chat.model)?;
            return Ok(SendOutcome::Replay(events));
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-no-side-effects
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-separate-path
        }
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-return-replay
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-completed
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-replay
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-is-replay

        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-preflight
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-preflight-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-preflight
        // a rejection is its JSON Problem; nothing was written and no provider call was made
        let prepared = self.preflight(ctx, &chat, &send, request_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-preflight
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-preflight-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-preflight

        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-reserve-tx
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-reserve-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-reserve
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-before-provider
        // the reserve transaction: a failure (429 tokens, 400 `invalid_attachment`, 409, 500)
        // rolled back, so no user message, turn or reserve remains; the clock reading is the
        // base of the user message `created_at`
        let command = ReserveCommand {
            tenant_id,
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            request_id,
            turn_id: Uuid::new_v4(),
            user_message_id: Uuid::new_v4(),
            content: send.content.clone(),
            attachment_ids: send.attachment_ids.clone(),
            web_search_enabled: send.web_search_enabled,
            decision: prepared.decision.clone(),
            now: self.quota.clock.now(),
        };
        let reserved = self.store.reserve(&self.quota, &command).await?;
        // the transaction committed: record the reserve metrics of its receipt, once per period
        reserved
            .receipt
            .record_after_commit(self.quota.metrics.as_ref());
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-before-provider
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-commit
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-reserve
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-reserve-failed
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-reserve-tx

        // the reserve transaction committed: the plan for the provider task (slice S2)
        let plan = ProviderCallPlan {
            tenant_id,
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            request_id,
            turn_id: reserved.turn_id,
            user_message_id: reserved.user_message_id,
            assistant_message_id: prepared.assistant_message_id,
            selected_model: chat.model.clone(),
            model: prepared.model,
            decision: prepared.decision,
            receipt: reserved.receipt.clone(),
            context: prepared.context,
            tools: prepared.tools,
            provider: prepared.provider,
            web_search_enabled: send.web_search_enabled,
            started_at: reserved.started_at,
        };
        self.hand_over(plan, reserved, cancel).await
    }

    /// The exits after the reserve transaction committed: the pre-provider cancellation check
    /// and the hand-over of the plan to the provider task. Every exit leaves the committed turn
    /// terminal (the finalization runs here, inside the detached setup task of the handler, so
    /// a dropped handler future cannot skip it) or in the hands of the provider task.
    pub(crate) async fn hand_over(
        &self,
        plan: ProviderCallPlan,
        reserved: ReservedTurn,
        cancel: &CancellationToken,
    ) -> Result<SendOutcome, DomainError> {
        let target = TurnTarget::of_plan(&plan);
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-before-provider
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-before-provider
        // the client dropped the connection during the setup: no provider request is sent and
        // the committed turn is finalized as `cancelled` (estimated settlement, `aborted` usage
        // event, no assistant message) before the setup returns
        if cancel.is_cancelled() {
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-finalize
            self.finalizer
                .finalize(&target, TurnOutcome::Cancelled)
                .await?;
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-finalize
            return Ok(SendOutcome::CancelledBeforeProvider(Box::new(reserved)));
        }
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-before-provider
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-before-provider

        // the reserve transaction committed and the token is live: hand the plan to the
        // provider task (slice S2)
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-provider-task
        let events = match self.provider_task.start(plan.clone(), cancel.clone()).await {
            Ok(events) => events,
            Err(error) => {
                // the task could not start: the turn must not stay `running` with its reserve,
                // so it ends `failed` (`provider_error`) before the failure is returned
                let failed = TurnOutcome::Failed {
                    code: StreamErrorCode::ProviderError,
                };
                if let Err(finalization) = self.finalizer.finalize(&target, failed).await {
                    tracing::error!(
                        turn_id = %target.turn_id,
                        %finalization,
                        "the turn of a provider task that could not start was not finalized"
                    );
                }
                return Err(error);
            }
        };
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-provider-task
        Ok(SendOutcome::Streaming(Box::new(StartedTurn {
            plan,
            events,
        })))
    }

    /// The post-commit assembly of a retry or an edit
    /// (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`): the tools of the
    /// effective model, the context plan within the snapshot boundary of the mutation and the
    /// provider of the effective model, through the same helpers as the send preflight. Writes
    /// nothing and takes no reserve.
    ///
    /// # Errors
    ///
    /// `ContextBudgetExceeded`, `InvalidAttachment` for an image without a provider file, and
    /// `Internal` for a provider that cannot be resolved or a policy plugin failure.
    pub async fn assemble_replacement(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        replacement: &ReplacementRequest<'_>,
    ) -> Result<ReplacementAssembly, DomainError> {
        let user_id = ctx.subject_id();
        let tenant_id = ctx.subject_tenant_id();
        let decision = replacement.decision;
        let model = replacement.model;
        // the snapshot of the version the quota preflight bound (the policy may have moved on)
        let snapshot = self.gateway.current_snapshot(user_id).await?;
        let snapshot = if snapshot.policy_version == decision.policy_version_applied {
            snapshot
        } else {
            self.gateway
                .snapshot_of_version(user_id, decision.policy_version_applied)
                .await?
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-tools
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-tools-afresh
        // the file search, code interpreter and knowledge search tools are decided afresh from
        // the chat's current facts and the effective model; `web_search` follows the carried flag
        let tools = self
            .assemble_turn_tools(
                tenant_id,
                chat.id,
                replacement.request_id,
                replacement.web_search_enabled,
                model,
                &snapshot.kill_switches,
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-tools-afresh
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-tools
        let images = replacement
            .carried_images
            .iter()
            .map(image_input)
            .collect::<Result<Vec<_>, _>>()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-context
        // no boundary: the replaced turn was the only one, so the history is empty (the
        // replacement user message itself is already stored and must not be read back)
        let history = match replacement.boundary {
            Some(boundary) => {
                self.store
                    .read_context(
                        tenant_id,
                        chat.id,
                        Some(boundary),
                        self.settings.recent_messages_limit,
                    )
                    .await?
            }
            None => ContextReads {
                summary: None,
                recent: Vec::new(),
            },
        };
        let (context, provider) = self.assemble_context_and_provider(
            tenant_id,
            model,
            &tools,
            decision,
            &history,
            &CurrentMessage {
                content: replacement.content.to_owned(),
                images,
            },
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-context
        Ok(ReplacementAssembly {
            tools,
            context,
            provider,
        })
    }

    /// The tool set of a turn: the attachment contributions (file search, code interpreter and
    /// knowledge search, in that order) and the web search of the effective model, shared by the
    /// send preflight and the post-commit setup of a retry or an edit.
    async fn assemble_turn_tools(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
        web_search_enabled: bool,
        model: &ModelCatalogEntry,
        kill_switches: &KillSwitches,
    ) -> Result<ToolSet, DomainError> {
        let attachment_tools = match &self.attachment_tools {
            Some(source) => Some(
                source
                    .contributions(&ChatToolInput {
                        tenant_id,
                        chat_id,
                        entry: model,
                        kill_switches,
                    })
                    .await?,
            ),
            None => None,
        };
        let mut contributions = self.contributions.clone();
        if let Some(turn) = &attachment_tools {
            contributions.extend(turn_contributions(turn));
        }
        let mut tools = assemble_tools(
            model,
            kill_switches,
            &self.settings.web_search_guard,
            &ToolTurnFacts {
                chat_id,
                request_id,
                web_search_enabled,
            },
            &contributions,
        );
        if let Some(turn) = attachment_tools {
            apply_turn_contributions(&mut tools, turn);
        }
        Ok(tools)
    }

    /// The context plan within the budget and the provider of the effective model, shared by the
    /// send preflight and the post-commit setup of a retry or an edit.
    fn assemble_context_and_provider(
        &self,
        tenant_id: Uuid,
        model: &ModelCatalogEntry,
        tools: &ToolSet,
        decision: &PreflightDecision,
        history: &ContextReads,
        current: &CurrentMessage,
    ) -> Result<(ContextAssembly, EffectiveProvider), DomainError> {
        let images = current.images.clone();
        let context = assemble_context(
            model,
            tools,
            decision.max_output_tokens_applied,
            history.summary.as_ref(),
            &history.recent,
            current,
        )?;
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-resolve-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-provider-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-provider-500
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-provider
        // the catalog `provider_id` and the caller's tenant (through `tenant_overrides`) give
        // the entry, its adapter kind and its OAGW alias; an unresolvable provider is a
        // generic 500 (the error mapping)
        let provider = self
            .providers
            .resolve(&model.provider_id, tenant_id)?
            .clone();
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-provider
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-provider-500
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-provider-failed
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-resolve-provider
        // the Anthropic Messages adapter references an image by its secondary Anthropic file id;
        // an image without an uploaded secondary copy is an unusable attachment, rejected before
        // any provider call (the other adapters use the primary provider file id)
        if provider.kind == ProviderKind::AnthropicMessages
            && images.iter().any(|image| image.secondary_file_id.is_none())
        {
            return Err(DomainError::InvalidAttachment);
        }
        Ok((context, provider))
    }

    /// The send preflight (`cpt-cf-mini-chat-algo-message-streaming-send-preflight`): model
    /// resolution, snapshot boundary, image count, quota preflight, input limit, image guards,
    /// tool assembly, context assembly, provider resolution and the assistant message id. Writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// See [`send`](Self::send).
    pub async fn preflight(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        send: &ValidatedSend,
        request_id: Uuid,
    ) -> Result<PreparedTurn, DomainError> {
        let user_id = ctx.subject_id();
        let tenant_id = ctx.subject_tenant_id();

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-catalog
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-resolve-model
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-policy-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-policy-500
        // the policy snapshot is asked on every request (no cache); a plugin failure is a
        // generic 500 (the error mapping)
        let snapshot = self.gateway.current_snapshot(user_id).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-policy-500
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-policy-failed
        // the chat's model, without the enabled filter
        let selected = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == chat.model);
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-resolve-model
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-catalog
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-model-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-invalid-model
        if selected.is_none() {
            return Err(DomainError::InvalidModel {
                model: chat.model.clone(),
            });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-invalid-model
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-model-missing

        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-facts
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-snapshot-boundary
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-read-attachments
        // the snapshot boundary (computed once, immutable for the request), the referenced
        // attachments and the chat facts of the quota estimate
        let reads = self
            .store
            .read_preflight(tenant_id, chat.id, &send.attachment_ids)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-read-attachments
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-snapshot-boundary
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-facts
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-too-many-images
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-too-many-images
        let image_count = u32::try_from(
            reads
                .attachments
                .iter()
                .filter(|attachment| attachment.kind == IMAGE_KIND)
                .count(),
        )
        .unwrap_or(u32::MAX);
        if image_count > self.settings.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-too-many-images
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-too-many-images

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-quota-preflight
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-quota-rejected
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-quota
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-send-flow
        // the quota preflight of the requester; its rejection (400 `FEATURE_DISABLED`, 429, 500)
        // is returned unchanged
        let decision = self
            .quota
            .preflight(
                ctx,
                &PreflightRequest {
                    chat_id: chat.id,
                    selected_model: chat.model.clone(),
                    facts: RequestFacts {
                        message_bytes: send.content.len() as u64,
                        image_count,
                        ready_documents: reads.ready_documents,
                        ready_xlsx_attachments: reads.ready_code_interpreter,
                        web_search_enabled: send.web_search_enabled,
                    },
                },
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-send-flow
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-quota
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-quota-rejected
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-quota-preflight

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-input-too-long
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-input-too-long
        if decision.input_too_long() {
            return Err(DomainError::InputTooLong);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return-input-too-long
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-input-too-long

        // the effective model of the decision, read from the snapshot of the version the
        // preflight bound (asked again when the policy moved in between)
        let snapshot = if snapshot.policy_version == decision.policy_version_applied {
            snapshot
        } else {
            self.gateway.current_snapshot(user_id).await?
        };
        let model = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == decision.effective_model)
            .cloned()
            .ok_or_else(|| {
                DomainError::internal("the effective model is missing from the catalog")
            })?;

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-image-guards
        check_image_guards(image_count, &snapshot.kill_switches, decision.vision_input)?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-image-guards

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-tools
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-contributions
        // the file search, code interpreter and knowledge search contributions, in that order
        let tools = self
            .assemble_turn_tools(
                tenant_id,
                chat.id,
                request_id,
                send.web_search_enabled,
                &model,
                &snapshot.kill_switches,
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-contributions
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-tools

        // the images of the request in `attachment_ids` order, each with its internal provider
        // file id (an image that has none cannot be sent to the model)
        let mut images = Vec::new();
        for attachment_id in &send.attachment_ids {
            let Some(attachment) = reads
                .attachments
                .iter()
                .find(|attachment| attachment.id == *attachment_id)
            else {
                continue;
            };
            if attachment.kind != IMAGE_KIND {
                continue;
            }
            images.push(image_input(attachment)?);
        }

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-context
        // the thread summary and the recent messages within the snapshot boundary, then the
        // assembly; `CONTEXT_BUDGET_EXCEEDED` when the mandatory items do not fit
        let history = self
            .store
            .read_context(
                tenant_id,
                chat.id,
                reads.boundary.as_ref(),
                self.settings.recent_messages_limit,
            )
            .await?;
        let (context, provider) = self.assemble_context_and_provider(
            tenant_id,
            &model,
            &tools,
            &decision,
            &history,
            &CurrentMessage {
                content: send.content.clone(),
                images: images.clone(),
            },
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-context

        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-message-id
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return
        // no assistant row exists until finalization; nothing was written and no provider
        // request was sent
        Ok(PreparedTurn {
            decision,
            model,
            images,
            context,
            tools,
            provider,
            assistant_message_id: Uuid::new_v4(),
        })
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-return
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-preflight:p1:inst-pre-message-id
    }
}
