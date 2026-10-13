//! The send-message flow up to the committed reserve (DESIGN sections 3.3 "Streaming Contract",
//! 3.6 "Send Message with Streaming Response", 3.9 "Idempotency Rules").

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mini_chat_sdk::ModelCatalogEntry;
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::facts::TurnFacts;
use super::model::{
    EventStream, MutationPreflight, MutationReserveCommand, NewTurnIds, PreparedMutation,
    ReserveCommand, SendInput, TurnContext, secondary_image_ids,
};
use super::ports::{ConversationRepository, TurnRepository};
use super::replay::ReplayService;
use super::run::{RunDeps, TurnRun};
use crate::config::{
    ContextConfig, MiniChatConfig, RagConfig, StreamingConfig, ThreadSummaryWorkerConfig,
};
use crate::domain::authz::{Pep, actions};
use crate::domain::billing::SettlementBasis;
use crate::domain::chat::Chat;
use crate::domain::chat::ChatRepository;
use crate::domain::clock;
use crate::domain::context::{ContextInputs, ContextPlan, assemble, check_input_length};
use crate::domain::error::{DisabledFeature, DomainError};
use crate::domain::estimation::{ReserveInputs, ToolFacts, ToolSet};
use crate::domain::llm::{
    ApiParams, InputMessage, InputRole, LlmProvider, LlmProviderResolver, LlmRequest,
    RequestIdentity, RequestType, ToolSpec,
};
use crate::domain::metrics::Metrics;
use crate::domain::mutation::MutationRepository;
use crate::domain::ports::ModelPolicyGateway;
use crate::domain::quota::{DowngradeReason, PreflightDecision, PreflightRequest, QuotaService};
use crate::domain::summary::{SummaryTrigger, evaluate as evaluate_summary_trigger};
use crate::domain::turn::TurnState;

/// The configuration the stream flow reads.
#[domain_model]
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// `streaming`.
    pub streaming: StreamingConfig,
    /// `context`.
    pub context: ContextConfig,
    /// `rag` (attachment limits).
    pub rag: RagConfig,
    /// `estimation_budgets.minimal_generation_floor`.
    pub minimal_generation_floor: u32,
    /// `thread_summary_worker` (the summary trigger).
    pub summary: ThreadSummaryWorkerConfig,
}

impl StreamConfig {
    /// The stream configuration of a validated gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            streaming: config.streaming.clone(),
            context: config.context.clone(),
            rag: config.rag.clone(),
            minimal_generation_floor: config.estimation_budgets.minimal_generation_floor,
            summary: config.thread_summary_worker.clone(),
        }
    }
}

/// What the preflight reads from the request.
struct PreflightInput<'a> {
    content: &'a str,
    web_search: bool,
    image_count: usize,
    exclude_request: Option<Uuid>,
}

/// The result of the preflight.
struct Preflighted {
    decision: PreflightDecision,
    limits: mini_chat_sdk::UserLimits,
    turn_context: TurnContext,
    now: time::OffsetDateTime,
}

/// The assembled turn: context, provider and request, ready for the reserve.
struct TurnSetup {
    plan: ContextPlan,
    provider: Arc<dyn LlmProvider>,
    request: LlmRequest,
    file_map: HashMap<String, (Uuid, String)>,
    summary_trigger: Option<SummaryTrigger>,
    max_output: i32,
    floor: i32,
}

/// Everything the provider task of a turn needs besides its setup.
struct Launch<'a> {
    ctx: &'a SecurityContext,
    scope: AccessScope,
    chat: &'a Chat,
    request_id: Uuid,
    turn_id: Uuid,
    assistant_message_id: Uuid,
    decision: &'a PreflightDecision,
    limits: mini_chat_sdk::UserLimits,
    setup: TurnSetup,
    now: time::OffsetDateTime,
}

/// `POST /v1/chats/{id}/messages:stream`: validation, idempotency, preflight, context assembly,
/// the reserve transaction and the start of the provider task.
#[domain_model]
pub struct StreamService {
    pep: Pep,
    chats: Arc<dyn ChatRepository>,
    turns: Arc<dyn TurnRepository>,
    mutations: Arc<dyn MutationRepository>,
    conversation: Arc<dyn ConversationRepository>,
    policy: Arc<dyn ModelPolicyGateway>,
    quota: Arc<QuotaService>,
    providers: Arc<dyn LlmProviderResolver>,
    replay: ReplayService,
    deps: RunDeps,
    config: StreamConfig,
    metrics: Arc<dyn Metrics>,
    /// The provider tasks, so that gear stop can wait for their finalization.
    tasks: TaskTracker,
}

/// Collaborators of the stream service.
pub struct StreamServiceParts {
    /// The PEP.
    pub pep: Pep,
    /// Chat reads.
    pub chats: Arc<dyn ChatRepository>,
    /// Turn persistence.
    pub turns: Arc<dyn TurnRepository>,
    /// Retry/edit reserve and failure of an unstarted turn.
    pub mutations: Arc<dyn MutationRepository>,
    /// Conversation reads.
    pub conversation: Arc<dyn ConversationRepository>,
    /// The model policy.
    pub policy: Arc<dyn ModelPolicyGateway>,
    /// The quota service.
    pub quota: Arc<QuotaService>,
    /// The provider resolver.
    pub providers: Arc<dyn LlmProviderResolver>,
    /// The provider task collaborators.
    pub deps: RunDeps,
    /// The stream configuration.
    pub config: StreamConfig,
}

impl StreamService {
    /// Creates the service.
    #[must_use]
    pub fn new(parts: StreamServiceParts) -> Self {
        let replay = ReplayService::new(Arc::clone(&parts.turns));
        let metrics = Arc::clone(&parts.deps.metrics);
        Self {
            pep: parts.pep,
            chats: parts.chats,
            turns: parts.turns,
            mutations: parts.mutations,
            conversation: parts.conversation,
            policy: parts.policy,
            quota: parts.quota,
            providers: parts.providers,
            replay,
            deps: parts.deps,
            config: parts.config,
            metrics,
            tasks: TaskTracker::new(),
        }
    }

    /// Waits until every provider task started so far has finished (its turn is finalized). The
    /// gear stop calls this before the outbox stops, so that no finalization runs without it; the
    /// caller bounds the wait.
    pub async fn wait_for_turns(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    /// Runs the setup and returns the event stream. Every failure before the reserve commit is
    /// an error and leaves nothing behind; after it the provider task owns the turn.
    ///
    /// # Errors
    /// Any of the pre-stream errors of the REST error table.
    pub async fn start(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        input: SendInput,
    ) -> Result<EventStream, DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::SEND_MESSAGE, Some(chat_id))
            .await?;
        let chat = self
            .chats
            .find(&scope, chat_id)
            .await?
            .ok_or(DomainError::ChatNotFound { chat_id })?
            .chat;
        let request_id = input.request_id.unwrap_or_else(Uuid::new_v4);
        let ping_interval = self.ping_interval();

        // Idempotency first (replay is a separate, side-effect-free path), then the parallel guard.
        if let Some(existing) = self
            .turns
            .find_by_request(&scope, chat_id, request_id)
            .await?
        {
            if existing.state == TurnState::Completed && !existing.deleted {
                let events = self.replay.events(&scope, &existing, &chat.model).await?;
                return Ok(EventStream {
                    events,
                    ping_interval,
                });
            }
            return Err(DomainError::RequestIdConflict);
        }
        if self.turns.find_running(&scope, chat_id).await?.is_some() {
            return Err(DomainError::TurnAlreadyRunning);
        }

        if input.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let attachments = self
            .validate_attachments(&scope, ctx, chat_id, &input)
            .await?;
        let image_ids: Vec<String> = attachments
            .iter()
            .filter(|attachment| attachment.kind == "image")
            .filter_map(|attachment| attachment.provider_file_id.clone())
            .collect();
        let secondary_ids = secondary_image_ids(&attachments);
        let image_count = attachments
            .iter()
            .filter(|attachment| attachment.kind == "image")
            .count();

        let pre = self
            .preflight(
                ctx,
                &scope,
                &chat,
                &PreflightInput {
                    content: &input.content,
                    web_search: input.web_search,
                    image_count,
                    exclude_request: None,
                },
            )
            .await?;
        let setup = self
            .plan_turn(
                ctx,
                &scope,
                &chat,
                &pre.decision,
                &pre.turn_context,
                &input.content,
                image_ids,
                secondary_ids,
                image_count,
            )
            .await?;

        let turn_id = Uuid::new_v4();
        let assistant_message_id = Uuid::new_v4();
        let command = ReserveCommand {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            chat_id,
            request_id,
            turn_id,
            user_message_id: Uuid::new_v4(),
            content: input.content.clone(),
            attachment_ids: input.attachment_ids.clone(),
            web_search_enabled: input.web_search,
            effective_model: pre.decision.effective.id.clone(),
            policy_version: pre.decision.policy_version,
            reserve_tokens: pre.decision.reserve.reserve_tokens,
            max_output_tokens_applied: setup.max_output,
            minimal_generation_floor_applied: setup.floor,
            plan: pre.decision.plan.clone(),
            now: pre.now,
        };
        if let Err(err) = self.turns.reserve(&scope, command).await {
            return Err(self
                .map_reserve_error(&scope, chat_id, request_id, err)
                .await);
        }
        self.record_reserve();
        Ok(self.launch(Launch {
            ctx,
            scope,
            chat: &chat,
            request_id,
            turn_id,
            assistant_message_id,
            decision: &pre.decision,
            limits: pre.limits,
            setup,
            now: pre.now,
        }))
    }

    /// The preflight of a retry or an edit, run before the mutation commit: the same checks as a
    /// new message (kill switches, model, quota cascade, input length, image guards on the
    /// original message's images) over the conversation without the replaced turn. Nothing is
    /// written.
    ///
    /// # Errors
    /// Any of the preflight errors of the REST error table.
    pub async fn mutation_preflight(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat: &Chat,
        input: MutationPreflight,
    ) -> Result<PreparedMutation, DomainError> {
        let pre = self
            .preflight(
                ctx,
                scope,
                chat,
                &PreflightInput {
                    content: &input.content,
                    web_search: input.web_search,
                    image_count: input.image_count,
                    exclude_request: Some(input.replaced_request),
                },
            )
            .await?;
        Ok(PreparedMutation {
            input,
            decision: pre.decision,
            limits: pre.limits,
            now: pre.now,
        })
    }

    /// Continues a retry or an edit after the mutation commit: context assembly, provider
    /// resolution and the reserve (which fills the preflight columns of the new turn), then the
    /// stream of the send path. A failure marks the new turn `failed` (`context_length_exceeded`,
    /// `quota_exceeded` or `turn_setup_failed`) with a plain CAS, without settlement or outbox
    /// event, and is returned as the JSON error.
    ///
    /// # Errors
    /// The setup failure of the new turn.
    pub async fn start_mutated(
        &self,
        ctx: &SecurityContext,
        scope: AccessScope,
        chat: Chat,
        prepared: PreparedMutation,
        ids: NewTurnIds,
    ) -> Result<EventStream, DomainError> {
        let setup = match self
            .setup_mutated(ctx, &scope, &chat, &prepared, &ids)
            .await
        {
            Ok(setup) => setup,
            Err(err) => {
                self.fail_unstarted(&scope, &ids, &err).await;
                return Err(err);
            }
        };
        self.record_reserve();
        let PreparedMutation {
            decision,
            limits,
            now,
            ..
        } = prepared;
        Ok(self.launch(Launch {
            ctx,
            scope,
            chat: &chat,
            request_id: ids.request_id,
            turn_id: ids.turn_id,
            assistant_message_id: ids.assistant_message_id,
            decision: &decision,
            limits,
            setup,
            now,
        }))
    }

    async fn setup_mutated(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat: &Chat,
        prepared: &PreparedMutation,
        ids: &NewTurnIds,
    ) -> Result<TurnSetup, DomainError> {
        // The new user message is already stored: it is the current message, not history.
        let turn_context = self
            .conversation
            .turn_context(
                scope,
                chat.id,
                self.config.context.recent_messages_limit,
                Some(ids.request_id),
            )
            .await?;
        let setup = self
            .plan_turn(
                ctx,
                scope,
                chat,
                &prepared.decision,
                &turn_context,
                &prepared.input.content,
                prepared.input.image_ids.clone(),
                prepared.input.image_secondary_ids.clone(),
                prepared.input.image_count,
            )
            .await?;
        self.mutations
            .reserve(
                scope,
                MutationReserveCommand {
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    turn_id: ids.turn_id,
                    effective_model: prepared.decision.effective.id.clone(),
                    policy_version: prepared.decision.policy_version,
                    reserve_tokens: prepared.decision.reserve.reserve_tokens,
                    max_output_tokens_applied: setup.max_output,
                    minimal_generation_floor_applied: setup.floor,
                    plan: prepared.decision.plan.clone(),
                    now: prepared.now,
                },
            )
            .await?;
        Ok(setup)
    }

    /// Marks an unstarted retry/edit turn `failed` after a setup failure. A failure of the CAS
    /// itself is logged: the orphan watchdog finalizes the turn later.
    async fn fail_unstarted(&self, scope: &AccessScope, ids: &NewTurnIds, err: &DomainError) {
        let code = match err {
            DomainError::ContextBudgetExceeded => "context_length_exceeded",
            DomainError::QuotaExceeded { .. } => "quota_exceeded",
            _ => "turn_setup_failed",
        };
        match self
            .mutations
            .fail_unstarted(scope, ids.turn_id, code, clock::now())
            .await
        {
            Ok(_) => {}
            Err(cas_err) => {
                tracing::error!(
                    turn_id = %ids.turn_id,
                    error = %cas_err,
                    "the unstarted turn could not be marked failed; the orphan watchdog will finalize it"
                );
            }
        }
    }

    fn ping_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.config.streaming.sse_ping_interval_seconds))
    }

    fn record_reserve(&self) {
        for period in crate::domain::quota::PeriodType::ALL {
            self.metrics.quota_reserve(period.as_str());
        }
    }

    /// The checks and the quota preflight shared by a new message and a retry/edit: kill switches,
    /// the chat's model (no enabled filter), the user's limits, image guards, the conversation
    /// read, the quota cascade, input length and the vision capability.
    async fn preflight(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat: &Chat,
        input: &PreflightInput<'_>,
    ) -> Result<Preflighted, DomainError> {
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        if input.web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled {
                feature: DisabledFeature::WebSearch,
            });
        }
        if snapshot
            .model_catalog
            .iter()
            .all(|entry| entry.id != chat.model)
        {
            return Err(DomainError::InvalidModel {
                detail: format!("the chat's model `{}` is not in the catalog", chat.model),
            });
        }
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let image_count = input.image_count;
        if image_count > 0 && snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled {
                feature: DisabledFeature::Images,
            });
        }
        if u32::try_from(image_count).unwrap_or(u32::MAX) > self.config.rag.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }
        if image_count > 0 {
            self.metrics
                .image_inputs_per_turn(f64::from(u32::try_from(image_count).unwrap_or(u32::MAX)));
        }
        let turn_context = self
            .conversation
            .turn_context(
                scope,
                chat.id,
                self.config.context.recent_messages_limit,
                input.exclude_request,
            )
            .await?;

        let now = clock::now();
        let content_bytes = input.content.len() as u64;
        let decision = self
            .quota
            .preflight(
                scope,
                &snapshot,
                &limits,
                &PreflightRequest {
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    selected_model: &chat.model,
                    reserve_inputs: ReserveInputs {
                        content_bytes,
                        prior_context_tokens: turn_context.prior_context_tokens,
                        image_count: u32::try_from(image_count).unwrap_or(u32::MAX),
                        streaming_max_output_tokens: self.config.streaming.max_output_tokens,
                    },
                    tool_facts: ToolFacts {
                        web_search_requested: input.web_search,
                        ready_documents: turn_context.ready_documents,
                        ready_code_interpreter_files: !turn_context
                            .code_interpreter_file_ids
                            .is_empty(),
                    },
                    now,
                },
            )
            .await?;
        let effective = &decision.effective;
        let tier = effective.tier.as_str();
        self.metrics
            .quota_preflight(decision.decision(), &effective.id, tier);
        #[allow(
            clippy::cast_precision_loss,
            reason = "token counts are far below 2^52"
        )]
        self.metrics
            .quota_estimated_tokens(decision.reserve.reserve_tokens as f64);

        check_input_length(effective, content_bytes)?;
        if image_count > 0
            && !effective
                .multimodal_capabilities
                .iter()
                .any(|capability| capability == "VISION_INPUT")
        {
            return Err(DomainError::VisionNotSupported);
        }
        Ok(Preflighted {
            decision,
            limits,
            turn_context,
            now,
        })
    }

    /// Context assembly, provider resolution and the provider request of a turn.
    #[allow(
        clippy::too_many_arguments,
        reason = "the inputs of the plan are independent facts of the turn"
    )]
    async fn plan_turn(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat: &Chat,
        decision: &PreflightDecision,
        turn_context: &TurnContext,
        content: &str,
        image_ids: Vec<String>,
        secondary_file_ids: HashMap<String, String>,
        image_count: usize,
    ) -> Result<TurnSetup, DomainError> {
        let effective = &decision.effective;
        let system_prompt = self.system_prompt(effective, decision.tools);
        let plan = assemble(
            effective,
            ContextInputs {
                system_prompt: &system_prompt,
                tools: decision.tools,
                max_output_tokens_applied: decision.reserve.max_output_tokens_applied,
                current_text: content,
                image_count: u32::try_from(image_count).unwrap_or(u32::MAX),
                summary: turn_context
                    .summary
                    .as_ref()
                    .map(|summary| summary.text.as_str()),
                history: turn_context.history.clone(),
            },
        )?;

        let provider = self
            .providers
            .resolve(ctx.subject_tenant_id(), &effective.provider_id)?;
        let tools = tool_specs(effective, decision.tools, turn_context);
        let mut messages = Vec::with_capacity(plan.history.len() + 2);
        if let Some(summary) = &plan.summary_message {
            messages.push(InputMessage {
                role: InputRole::User,
                text: summary.clone(),
                image_file_ids: Vec::new(),
            });
        }
        messages.extend(plan.history.iter().cloned());
        messages.push(InputMessage {
            role: InputRole::User,
            text: content.to_owned(),
            image_file_ids: image_ids,
        });
        let request = LlmRequest {
            model: effective.provider_model_id.clone(),
            system_prompt,
            input: messages,
            max_output_tokens: decision.reserve.max_output_tokens_applied,
            tools: tools.clone(),
            max_tool_calls: (!tools.is_empty()).then_some(effective.max_tool_calls),
            identity: RequestIdentity {
                tenant_id: ctx.subject_tenant_id(),
                user_id: ctx.subject_id(),
                chat_id: chat.id,
            },
            request_type: RequestType::Chat,
            api_params: ApiParams::from_entry(effective),
            feature: decision.tools.feature_label(),
            secondary_file_ids,
        };

        let file_map = if decision.tools.file_search {
            self.conversation.file_citation_map(scope, chat.id).await?
        } else {
            HashMap::default()
        };
        let max_output =
            i32::try_from(decision.reserve.max_output_tokens_applied).unwrap_or(i32::MAX);
        let floor = i32::try_from(
            self.config
                .minimal_generation_floor
                .min(decision.reserve.max_output_tokens_applied),
        )
        .unwrap_or(i32::MAX);
        let summary_trigger = evaluate_summary_trigger(
            &self.config.summary,
            effective,
            decision.reserve.max_output_tokens_applied,
            &plan,
        );
        Ok(TurnSetup {
            plan,
            provider,
            request,
            file_map,
            summary_trigger,
            max_output,
            floor,
        })
    }

    /// Spawns the provider task of a turn whose reserve is committed and returns its stream.
    fn launch(&self, launch: Launch<'_>) -> EventStream {
        let Launch {
            ctx,
            scope,
            chat,
            request_id,
            turn_id,
            assistant_message_id,
            decision,
            limits,
            setup,
            now,
        } = launch;
        let effective = &decision.effective;
        let facts = TurnFacts {
            turn_id,
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            request_id,
            assistant_message_id,
            selected_model: chat.model.clone(),
            effective_model: effective.id.clone(),
            quota_decision: decision.decision(),
            downgrade_from: decision
                .downgrade_reason
                .is_some()
                .then(|| chat.model.clone()),
            downgrade_reason: decision.downgrade_reason.map(DowngradeReason::as_str),
            policy_version: decision.policy_version,
            started_at: now,
            basis: SettlementBasis {
                reserve_tokens: decision.reserve.reserve_tokens,
                max_output_tokens_applied: i64::from(decision.reserve.max_output_tokens_applied),
                reserved_credits_micro: decision.reserve.reserved_credits_micro,
                minimal_generation_floor_applied: i64::from(setup.floor),
                in_mult: i64::try_from(effective.input_tokens_credit_multiplier_micro)
                    .unwrap_or(i64::MAX),
                out_mult: i64::try_from(effective.output_tokens_credit_multiplier_micro)
                    .unwrap_or(i64::MAX),
            },
            premium: effective.tier == mini_chat_sdk::ModelTier::Premium,
            thread_summary_tokens: setup.plan.summary_tokens,
            trace_id: self.deps.trace.current_trace_id(),
            summary_trigger: setup.summary_trigger,
        };
        let (tx, rx) = mpsc::channel(usize::from(self.config.streaming.sse_channel_capacity));
        let run = TurnRun {
            deps: self.deps.clone(),
            ctx: ctx.clone(),
            scope,
            provider: setup.provider,
            request: setup.request,
            facts,
            limits,
            file_map: setup.file_map,
            tx,
        };
        self.tasks.spawn(run.run());
        EventStream {
            events: rx,
            ping_interval: self.ping_interval(),
        }
    }

    /// Duplicate, foreign, not-ready and over-limit attachment ids are `invalid_attachment`.
    async fn validate_attachments(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        chat_id: Uuid,
        input: &SendInput,
    ) -> Result<Vec<super::model::AttachmentRef>, DomainError> {
        if input.attachment_ids.is_empty() {
            return Ok(Vec::new());
        }
        let unique: HashSet<&Uuid> = input.attachment_ids.iter().collect();
        let limit = self
            .config
            .rag
            .max_documents_per_chat
            .saturating_add(self.config.rag.max_images_per_message);
        if unique.len() != input.attachment_ids.len()
            || u32::try_from(input.attachment_ids.len()).unwrap_or(u32::MAX) > limit
        {
            return Err(DomainError::InvalidAttachment);
        }
        let mut found = self
            .conversation
            .ready_attachments(scope, chat_id, ctx.subject_id(), &input.attachment_ids)
            .await?;
        if found.len() != input.attachment_ids.len() {
            return Err(DomainError::InvalidAttachment);
        }
        // The provider input follows the order of the request, not the order of the rows.
        found.sort_by_key(|attachment| {
            input
                .attachment_ids
                .iter()
                .position(|id| *id == attachment.id)
        });
        Ok(found)
    }

    fn system_prompt(&self, entry: &ModelCatalogEntry, tools: ToolSet) -> String {
        let mut parts = vec![entry.system_prompt.as_str()];
        if tools.file_search {
            parts.push(&self.config.context.file_search_guard);
        }
        if tools.web_search {
            parts.push(&self.config.context.web_search_guard);
        }
        parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// A unique-index loser is `request_id_conflict` when the key exists, otherwise
    /// `turn_already_running`.
    async fn map_reserve_error(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        err: DomainError,
    ) -> DomainError {
        if !matches!(err, DomainError::UniqueViolation) {
            return err;
        }
        match self.turns.find_by_request(scope, chat_id, request_id).await {
            Ok(Some(_)) => DomainError::RequestIdConflict,
            Ok(None) => DomainError::TurnAlreadyRunning,
            Err(other) => other,
        }
    }
}

fn tool_specs(
    entry: &ModelCatalogEntry,
    tools: ToolSet,
    context: &super::model::TurnContext,
) -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    if tools.file_search
        && let Some(vector_store_id) = &context.vector_store_id
    {
        specs.push(ToolSpec::FileSearch {
            vector_store_id: vector_store_id.clone(),
            max_num_results: entry.max_num_results,
        });
    }
    if tools.web_search {
        specs.push(ToolSpec::WebSearch {
            context_size: match entry.web_search_context_size {
                mini_chat_sdk::WebSearchContextSize::Low => "low",
                mini_chat_sdk::WebSearchContextSize::Medium => "medium",
                mini_chat_sdk::WebSearchContextSize::High => "high",
            }
            .to_owned(),
        });
    }
    if tools.code_interpreter {
        specs.push(ToolSpec::CodeInterpreter {
            file_ids: context.code_interpreter_file_ids.clone(),
        });
    }
    specs
}
