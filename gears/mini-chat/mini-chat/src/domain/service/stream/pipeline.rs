//! The send pipeline: the setup of a turn up to the reserve commit, the spawn
//! of the provider task and the turn status query
//! (`cpt-cf-mini-chat-flow-streaming-turns-send-message`,
//! `cpt-cf-mini-chat-algo-streaming-turns-create-turn`).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::context::{ContextInput, ContextPlan, assemble_context, check_input_length};
use super::finalize::{TaskDeps, TurnRun};
use super::idempotency::{IdempotencyOutcome, resolve_idempotency, resolve_unique_violation};
use super::inputs::{InputLimits, TurnInputs, load_turn_inputs};
use super::knowledge::KnowledgeToolUse;
use super::ports::{
    CreateTurnInput, KnowledgeRetriever, ProviderClient, ProviderDirectory, StreamMetrics,
    TurnRepository,
};
use super::relay::RelayHandle;
use super::replay::replay_completed;
use super::request::ProviderRequestInput;
use super::status::{TurnStatus, project_status};
use super::task::run_provider_task;
use super::tools::{GuardTexts, KnowledgeInput, KnowledgeProviderInfo, ToolsInput, decide_tools};
use super::types::StreamEvent;
use super::types::StreamStarted;
use super::validate::{SendRequest, ValidationLimits, validate_request};
use crate::config::MiniChatConfig;
use crate::domain::authz::{AuthzService, ChatAction};
use crate::domain::error::{DisabledFeature, DomainError, ResourceKind};
use crate::domain::model::Chat;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::quota::preflight::{PreflightDecision, PreflightRequest};
use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest};
use crate::domain::service::quota::service::{Clock, QuotaService};
use crate::domain::service::workers::summary_trigger::SummaryTriggerPlan;

/// The settings of the stream service, taken from the gear configuration.
#[derive(Debug, Clone)]
pub struct StreamSettings {
    /// `streaming.sse_ping_interval_seconds` (also the liveness interval).
    pub ping_interval: Duration,
    /// `streaming.sse_channel_capacity`.
    pub channel_capacity: usize,
    /// Attachment id limits of the request validation.
    pub validation: ValidationLimits,
    /// Input loading limits.
    pub inputs: InputLimits,
    /// Context guard texts.
    pub guards: GuardTexts,
    /// Knowledge-search configuration; `provider` and `retriever_configured`
    /// are filled per turn.
    pub knowledge: KnowledgeInput,
    /// `knowledge_search.provider_id`: the provider entry the per-turn
    /// readiness check resolves.
    pub knowledge_provider_id: Option<String>,
    /// `thread_summary_worker.enabled`: gates the summary trigger of the
    /// finalization transaction.
    pub summary_enabled: bool,
    /// `thread_summary_worker.compression_threshold_pct`.
    pub summary_threshold_pct: u32,
}

impl StreamSettings {
    /// The settings of the gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            ping_interval: Duration::from_secs(u64::from(
                config.streaming.sse_ping_interval_seconds,
            )),
            channel_capacity: usize::from(config.streaming.sse_channel_capacity),
            validation: ValidationLimits {
                max_documents_per_chat: config.rag.max_documents_per_chat,
                max_images_per_message: config.rag.max_images_per_message,
            },
            inputs: InputLimits {
                recent_messages_limit: config.context.recent_messages_limit,
                max_images_per_message: config.rag.max_images_per_message,
            },
            guards: GuardTexts {
                file_search: config.context.file_search_guard.clone(),
                web_search: config.context.web_search_guard.clone(),
            },
            knowledge: KnowledgeInput {
                enabled: config.knowledge_search.enabled,
                provider: None,
                retriever_configured: false,
                guard: config.knowledge_search.guard.clone(),
                max_calls_per_message: config.knowledge_search.max_calls_per_message,
                top_k: config.knowledge_search.top_k,
                max_chunk_chars: config.knowledge_search.max_chunk_chars,
            },
            knowledge_provider_id: config.knowledge_search.provider_id.clone(),
            summary_enabled: config.thread_summary_worker.enabled,
            summary_threshold_pct: config.thread_summary_worker.compression_threshold_pct,
        }
    }
}

/// Collaborators of the stream service.
pub struct StreamDeps {
    /// PEP.
    pub authz: Arc<AuthzService>,
    /// Chat store (scoped chat load).
    pub chats: Arc<dyn ChatRepository>,
    /// Model-policy gateway (catalog check).
    pub policy: Arc<dyn PolicyCatalog>,
    /// Quota service (preflight, tool limits, settlement metrics, warnings).
    pub quota: Arc<QuotaService>,
    /// Turn store.
    pub turns: Arc<dyn TurnRepository>,
    /// Provider directory.
    pub providers: Arc<dyn ProviderDirectory>,
    /// Provider client (adapters behind OAGW).
    pub client: Arc<dyn ProviderClient>,
    /// The knowledge retriever; `None` when knowledge search is not wired.
    pub retriever: Option<Arc<dyn KnowledgeRetriever>>,
    /// Metrics sink.
    pub metrics: Arc<dyn StreamMetrics>,
    /// Settings.
    pub settings: StreamSettings,
    /// Clock of the timestamps the service writes.
    pub clock: Clock,
}

/// The `ContextPlan` outputs the finalization carries to the thread-summary
/// trigger (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`:
/// `assembled_context_tokens`, `messages_truncated`, the effective input budget
/// and the summary-loaded flag), with the `system_request_id` generated before
/// the finalization transaction.
// @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-trigger-conditions
// @cpt-begin:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-send-long-turn
fn summary_trigger_plan(settings: &StreamSettings, plan: &ContextPlan) -> SummaryTriggerPlan {
    SummaryTriggerPlan {
        enabled: settings.summary_enabled,
        compression_threshold_pct: settings.summary_threshold_pct,
        assembled_context_tokens: plan.assembled_context_tokens,
        messages_truncated: plan.messages_truncated,
        effective_input_budget: plan.effective_input_budget,
        summary_applied: plan.thread_summary_applied.is_some(),
        system_request_id: Uuid::new_v4(),
    }
}
// @cpt-end:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-send-long-turn
// @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-trigger-conditions

/// What the setup hands to the handler.
pub enum SendOutcome {
    /// A completed turn is replayed: `stream_started`, one `delta`, `done`.
    Replay(Vec<StreamEvent>),
    /// A new turn: the relay handle and the `stream_started` payload.
    Stream {
        /// The `stream_started` payload.
        started: StreamStarted,
        /// The receiver with its drop guard.
        relay: RelayHandle,
    },
}

/// What the post-commit setup of a retry or an edit hands to the stream
/// service (`cpt-cf-mini-chat-algo-turn-mutations-run-replacement`, steps
/// `inst-mutation-decide-tools` to `inst-hand-off`).
pub struct ReplacementStart<'a> {
    /// The request context (the provider call runs with it).
    pub ctx: SecurityContext,
    /// The chat of the replacement.
    pub chat: &'a Chat,
    /// The requesting user.
    pub user_id: Uuid,
    /// The new `request_id`.
    pub request_id: Uuid,
    /// The new turn.
    pub turn_id: Uuid,
    /// The pre-allocated assistant message id.
    pub assistant_message_id: Uuid,
    /// The mutation time (the new user message time).
    pub user_message_at: time::OffsetDateTime,
    /// The preflight decision.
    pub decision: PreflightDecision,
    /// The attachment part of the inputs plus the history of the replacement.
    pub inputs: TurnInputs,
    /// The content of the new user message.
    pub content: &'a str,
    /// The instant the handler accepted the request.
    pub accepted_at: Instant,
}

/// The stream service.
pub struct StreamService {
    deps: StreamDeps,
    task: Arc<TaskDeps>,
}

impl StreamService {
    /// Creates the service.
    #[must_use]
    pub fn new(deps: StreamDeps) -> Self {
        let task = Arc::new(TaskDeps {
            turns: Arc::clone(&deps.turns),
            client: Arc::clone(&deps.client),
            quota: Arc::clone(&deps.quota),
            metrics: Arc::clone(&deps.metrics),
            ping_interval: deps.settings.ping_interval,
            clock: Arc::clone(&deps.clock),
            tool_use: match &deps.retriever {
                Some(retriever) => Arc::new(KnowledgeToolUse::new(
                    Arc::clone(&deps.client),
                    Arc::clone(retriever),
                    Arc::clone(&deps.metrics),
                )),
                None => Arc::new(super::task::RejectToolUse),
            },
        });
        Self { deps, task }
    }

    /// The knowledge-search configuration of one turn: the readiness of the
    /// provider entry named by `knowledge_search.provider_id` for the tenant
    /// and of the retriever.
    fn knowledge_input(&self, tenant_id: Uuid) -> KnowledgeInput {
        let mut input = self.deps.settings.knowledge.clone();
        input.retriever_configured = self.deps.retriever.is_some();
        if input.enabled
            && let Some(provider_id) = &self.deps.settings.knowledge_provider_id
        {
            match self.deps.providers.resolve(provider_id, tenant_id) {
                Ok(target) => {
                    input.provider = Some(KnowledgeProviderInfo {
                        kind: target.kind,
                        has_alias: !target.alias.is_empty(),
                        api_version: target.api_version,
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "the knowledge-search provider does not resolve");
                }
            }
        }
        input
    }

    /// The settings of the service (the mutation service reads the input
    /// limits).
    #[must_use]
    pub fn settings(&self) -> &StreamSettings {
        &self.deps.settings
    }

    /// The post-commit setup of a retry or an edit: the tool decision, the
    /// context assembly, the provider resolution and request building, the
    /// fill-mode reserve as the last step, then the hand-off to the provider
    /// task. Any failure is returned as it is; the caller marks the new turn
    /// failed. The returned sender is the go-signal of the provider task: the
    /// caller fires it after the result was delivered.
    ///
    /// # Errors
    ///
    /// `ContextBudgetExceeded`, `ProviderResolution`, `Internal`,
    /// `QuotaExceeded(Tokens)` and `Database`.
    pub async fn start_replacement(
        &self,
        input: ReplacementStart<'_>,
    ) -> Result<(StreamStarted, RelayHandle, oneshot::Sender<()>), DomainError> {
        let deps = &self.deps;
        let chat = input.chat;
        let knowledge = self.knowledge_input(chat.tenant_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-decide-tools
        let tools = decide_tools(&ToolsInput {
            decision: &input.decision,
            inputs: &input.inputs,
            guards: &deps.settings.guards,
            knowledge: &knowledge,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-decide-tools
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-assemble-context
        let plan = assemble_context(&ContextInput {
            entry: &input.decision.entry,
            max_output_tokens_applied: input.decision.reserve.max_output_tokens_applied,
            tools: &tools,
            content: input.content,
            images: &input.inputs.images,
            summary: input.inputs.summary.as_ref(),
            recent: &input.inputs.recent,
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-assemble-context
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-resolve-and-build
        let target = deps
            .providers
            .resolve(&input.decision.provider_id, chat.tenant_id)?;
        let built = deps.client.build_request(&ProviderRequestInput {
            target: &target,
            entry: &input.decision.entry,
            plan: &plan,
            tools: &tools,
            max_output_tokens_applied: input.decision.reserve.max_output_tokens_applied,
            tenant_id: chat.tenant_id,
            user_id: input.user_id,
            chat_id: chat.id,
        })?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-reserve
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-reserve-fill
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-if-fill-over
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-return-fill-over
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-return-fill-error
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-return-fill
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-to-reserved
        // The reserve is the last setup step; it records the reserve metrics.
        deps.quota
            .reserve(&ReserveRequest {
                tenant_id: chat.tenant_id,
                user_id: input.user_id,
                mode: ReserveMode::Fill {
                    turn_id: input.turn_id,
                },
                decision: &input.decision,
                now: (deps.clock)(),
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-to-reserved
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-return-fill
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-return-fill-error
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-return-fill-over
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-if-fill-over
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement:p1:inst-reserve-fill
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-reserve
        deps.metrics.image_inputs_per_turn(input.inputs.image_count);

        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-hand-off
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-to-finalized
        let (tx, rx) = mpsc::channel(deps.settings.channel_capacity.max(1));
        let cancel = CancellationToken::new();
        let relay = RelayHandle::new(
            rx,
            cancel.clone(),
            Arc::clone(&deps.metrics),
            deps.settings.ping_interval,
        );
        let (go_tx, go_rx) = oneshot::channel();
        let provider_id = input.decision.provider_id.clone();
        let run = TurnRun {
            ctx: input.ctx,
            chat: chat.clone(),
            user_id: input.user_id,
            request_id: input.request_id,
            turn_id: input.turn_id,
            assistant_message_id: input.assistant_message_id,
            user_message_at: input.user_message_at,
            decision: input.decision,
            tools,
            request: built,
            provider_id,
            accepted_at: input.accepted_at,
            summary: summary_trigger_plan(&deps.settings, &plan),
        };
        tokio::spawn(run_provider_task(
            Arc::clone(&self.task),
            run,
            tx,
            cancel,
            go_rx,
        ));
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-to-finalized
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-hand-off
        let started = StreamStarted {
            request_id: input.request_id,
            message_id: input.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: plan.thread_summary_applied,
        };
        Ok((started, relay, go_tx))
    }

    /// The task collaborators (tests drive the provider task directly).
    #[must_use]
    pub fn task_deps(&self) -> Arc<TaskDeps> {
        Arc::clone(&self.task)
    }

    /// Runs the setup of a send in a separately spawned task and awaits only
    /// its result, so a dropped caller never interrupts a started reserve
    /// transaction and never strands a committed turn: a result that cannot
    /// be delivered is dropped, which cancels the turn's token, and the
    /// provider task then finalizes the turn as `cancelled` without calling
    /// the provider.
    ///
    /// # Errors
    ///
    /// Every pre-stream refusal as its [`DomainError`].
    pub async fn send(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request: SendRequest,
    ) -> Result<SendOutcome, DomainError> {
        let accepted_at = Instant::now();
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-spawn-setup
        let (result_tx, result_rx) = oneshot::channel();
        let this = Arc::clone(self);
        tokio::spawn(async move {
            match this.setup(ctx, chat_id, request, accepted_at).await {
                Ok((outcome, go)) => {
                    // A receiver that is gone returns the outcome: dropping its
                    // relay handle cancels the token before the go-signal.
                    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-handler-gone
                    if let Err(undelivered) = result_tx.send(Ok(outcome)) {
                        drop(undelivered);
                    }
                    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-handler-gone
                    if let Some(go) = go
                        && go.send(()).is_err()
                    {
                        tracing::debug!("the provider task ended before its go-signal");
                    }
                }
                Err(error) => {
                    if result_tx.send(Err(error)).is_err() {
                        tracing::debug!("the handler is gone, the setup error is dropped");
                    }
                }
            }
        });
        result_rx.await.unwrap_or_else(|_| {
            Err(DomainError::Internal(
                "the send setup ended without a result".to_owned(),
            ))
        })
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-spawn-setup
    }

    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    ///
    /// `AuthzDenied`, `PdpUnavailable`, `NotFound { Chat }`,
    /// `NotFound { Turn }` (unknown, soft-deleted or foreign), `Database`.
    pub async fn turn_status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatus, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-authorize-read-turn
        let scope = self
            .deps
            .authz
            .chat_scope(ctx, ChatAction::ReadTurn, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-authorize-read-turn
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-load-turn
        self.deps.chats.load(&scope, chat_id).await?;
        let turn = self
            .deps
            .turns
            .find_by_request_id(&scope, chat_id, request_id)
            .await?
            .filter(|turn| !turn.deleted)
            // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-if-turn-missing
            // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-return-turn-404
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Turn,
            })?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-resolve-and-build
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-return-turn-404
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-if-turn-missing
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-load-turn
        Ok(project_status(&turn))
    }

    /// The setup of a send up to the hand-off: every refusal, the reserve
    /// transaction and the spawn of the provider task.
    async fn setup(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request: SendRequest,
        accepted_at: Instant,
    ) -> Result<(SendOutcome, Option<oneshot::Sender<()>>), DomainError> {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-validate-request
        let normalized = validate_request(&request, &deps.settings.validation)?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-validate-request
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-authorize-send
        let scope = deps
            .authz
            .chat_scope(&ctx, ChatAction::SendMessage, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-authorize-send
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-load-chat
        let chat = deps.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-load-chat

        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-resolve-idempotency
        match resolve_idempotency(&*deps.turns, &scope, chat.id, normalized.request_id).await? {
            // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-if-replay
            IdempotencyOutcome::Replay(turn) => {
                let events = replay_completed(&*deps.turns, &scope, &chat, &turn).await?;
                // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-replay
                return Ok((SendOutcome::Replay(events), None));
                // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-replay
            }
            // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-if-replay
            // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-if-conflict
            // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-conflict
            IdempotencyOutcome::RequestIdConflict => return Err(DomainError::RequestIdConflict),
            IdempotencyOutcome::TurnAlreadyRunning => return Err(DomainError::TurnAlreadyRunning),
            // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-conflict
            // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-if-conflict
            IdempotencyOutcome::Proceed => {}
        }
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-resolve-idempotency

        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-check-chat-model
        let user_id = ctx.subject_id();
        let version = deps.policy.current_policy_version(user_id).await?;
        let snapshot = deps
            .policy
            .policy_snapshot(user_id, version.policy_version)
            .await?;
        if !snapshot
            .model_catalog
            .iter()
            .any(|entry| entry.id == chat.model)
        {
            return Err(DomainError::InvalidModel);
        }
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-check-chat-model

        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-load-inputs
        let inputs = load_turn_inputs(
            &*deps.turns,
            &scope,
            chat.id,
            &normalized.attachment_ids,
            &deps.settings.inputs,
        )
        .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-load-inputs
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-preflight
        let decision = deps
            .quota
            .preflight(&PreflightRequest {
                tenant_id: chat.tenant_id,
                user_id,
                chat_id: chat.id,
                selected_model: chat.model.clone(),
                message_bytes: normalized.content.len() as u64,
                image_count: inputs.image_count,
                web_search_enabled: normalized.web_search_enabled,
                has_ready_file_search_documents: inputs.has_ready_file_search_documents,
                has_ready_code_interpreter_files: inputs.has_ready_code_interpreter_files,
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-preflight
        check_input_length(&decision.entry, &normalized.content)?;
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-image-guards
        if inputs.image_count > 0 {
            if decision.snapshot.kill_switches.disable_images {
                return Err(DomainError::FeatureDisabled(DisabledFeature::Images));
            }
            if !decision.vision_input {
                return Err(DomainError::VisionNotSupported);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-image-guards

        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-decide-tools
        let knowledge = self.knowledge_input(chat.tenant_id);
        let tools = decide_tools(&ToolsInput {
            decision: &decision,
            inputs: &inputs,
            guards: &deps.settings.guards,
            knowledge: &knowledge,
        });
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-decide-tools
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-assemble-context
        let plan = assemble_context(&ContextInput {
            entry: &decision.entry,
            max_output_tokens_applied: decision.reserve.max_output_tokens_applied,
            tools: &tools,
            content: &normalized.content,
            images: &inputs.images,
            summary: inputs.summary.as_ref(),
            recent: &inputs.recent,
        })?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-assemble-context
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-resolve-and-build
        let target = deps
            .providers
            .resolve(&decision.provider_id, chat.tenant_id)?;
        let built = deps.client.build_request(&ProviderRequestInput {
            target: &target,
            entry: &decision.entry,
            plan: &plan,
            tools: &tools,
            max_output_tokens_applied: decision.reserve.max_output_tokens_applied,
            tenant_id: chat.tenant_id,
            user_id,
            chat_id: chat.id,
        })?;
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-resolve-and-build

        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-pregenerate
        let (turn_id, user_message_id, assistant_message_id) =
            (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let now = (deps.clock)();
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-pregenerate
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-create-turn
        let created = deps
            .turns
            .create_turn(&CreateTurnInput {
                reserve: ReserveRequest {
                    tenant_id: chat.tenant_id,
                    user_id,
                    mode: ReserveMode::Send,
                    decision: &decision,
                    now,
                },
                chat_id: chat.id,
                request_id: normalized.request_id,
                turn_id,
                user_message_id,
                content: &normalized.content,
                attachment_ids: &normalized.attachment_ids,
                web_search_enabled: normalized.web_search_enabled,
            })
            .await;
        match created {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-turn
            Ok(_columns) => {}
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-turn
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-if-key-race
            Err(DomainError::UniqueViolation) => {
                return match resolve_unique_violation(
                    &*deps.turns,
                    &scope,
                    chat.id,
                    normalized.request_id,
                )
                .await?
                {
                    IdempotencyOutcome::Replay(turn) => {
                        let events = replay_completed(&*deps.turns, &scope, &chat, &turn).await?;
                        Ok((SendOutcome::Replay(events), None))
                    }
                    IdempotencyOutcome::TurnAlreadyRunning => Err(DomainError::TurnAlreadyRunning),
                    IdempotencyOutcome::RequestIdConflict | IdempotencyOutcome::Proceed => {
                        Err(DomainError::RequestIdConflict)
                    }
                };
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-if-key-race
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-tx-error
            Err(error) => return Err(error),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-tx-error
        }
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-create-turn

        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-record-reserve
        deps.quota.record_reserve_committed();
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-turn
        deps.metrics.image_inputs_per_turn(inputs.image_count);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-turn
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-record-reserve

        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-create-channel
        let (tx, rx) = mpsc::channel(deps.settings.channel_capacity.max(1));
        let cancel = CancellationToken::new();
        let relay = RelayHandle::new(
            rx,
            cancel.clone(),
            Arc::clone(&deps.metrics),
            deps.settings.ping_interval,
        );
        let (go_tx, go_rx) = oneshot::channel();
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-create-channel
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-spawn-provider-task
        let provider_id = decision.provider_id.clone();
        let run = TurnRun {
            ctx,
            chat: chat.clone(),
            user_id,
            request_id: normalized.request_id,
            turn_id,
            assistant_message_id,
            user_message_at: now,
            decision,
            tools,
            request: built,
            provider_id,
            accepted_at,
            summary: summary_trigger_plan(&deps.settings, &plan),
        };
        tokio::spawn(run_provider_task(
            Arc::clone(&self.task),
            run,
            tx,
            cancel,
            go_rx,
        ));
        let started = StreamStarted {
            request_id: normalized.request_id,
            message_id: assistant_message_id,
            is_new_turn: true,
            // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-return-compressed-turn
            thread_summary_applied: plan.thread_summary_applied,
            // @cpt-end:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-return-compressed-turn
        };
        Ok((SendOutcome::Stream { started, relay }, Some(go_tx)))
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-spawn-provider-task
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pipeline_summary_tests.rs"]
mod summary_tests;
