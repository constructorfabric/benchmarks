//! Stream service: the send/retry/edit streaming pipeline — preflight,
//! context assembly, provider dispatch, SSE relay, finalization.

use std::sync::Arc;

use mini_chat_sdk::UsageTokens;
use tokio::sync::mpsc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    Citation, CitationSource, CitationsData, DeltaData, DeltaKind, DoneData, ErrorData,
    MiniChatSseEvent, QuotaDecisionKind, StreamMessageRequest, StreamStartedData,
    ThreadSummaryInfo, ToolData, ToolPhase, Usage,
};
use crate::domain::context::{self, AssemblyInput, ContextMessage};
use crate::domain::finalize::{FinalizeOutcome, FinalizeService, QuotaMeta, TerminalInput, ToolCallCounts};
use crate::domain::quota_service::{PreflightInput, PreflightOutcome, QuotaDecision, QuotaService};
use crate::domain::services::ServiceContext;
use crate::error::{DomainError, ResourceKind};
use crate::infra::llm::resolver::{ProviderRegistry, ResolvedProvider};
use crate::infra::llm::providers::ProviderAdapter;
use crate::infra::llm::{
    InputMessage, LlmProviderError, LlmRequest, ProviderEvent, RawCitation, ToolSpec,
};
use crate::infra::metrics::{Metrics, label};
use crate::infra::storage::entity::{attachments, chat_turns};
use crate::infra::storage::repo::messages::NewMessage;
use crate::infra::storage::repo::turns::state;

/// The SSE relay channel item.
pub type SseSender = mpsc::Sender<MiniChatSseEvent>;
/// The receiving end handed to the SSE writer.
pub type SseReceiver = mpsc::Receiver<MiniChatSseEvent>;

/// The result of a successful stream setup.
pub enum StreamSetup {
    /// Live generation.
    Live {
        /// Resolved request id.
        request_id: Uuid,
        /// Pre-allocated assistant message id.
        message_id: Uuid,
        /// The event channel.
        receiver: SseReceiver,
        /// Cancelled when the SSE stream is dropped (client disconnect).
        cancel: tokio_util::sync::CancellationToken,
    },
    /// Buffered replay of a completed turn.
    Replay(Vec<MiniChatSseEvent>),
}

/// The stream service.
pub struct StreamService {
    ctx: Arc<ServiceContext>,
    quota: QuotaService,
    finalize: Arc<FinalizeService>,
    adapter: Arc<ProviderAdapter>,
    registry: Arc<ProviderRegistry>,
    metrics: Metrics,
}

/// A validated, resolved attachment ready for the current turn.
pub(crate) struct ResolvedAttachments {
    /// Image count.
    pub(crate) image_count: u32,
    /// All resolved rows.
    pub(crate) rows: Vec<attachments::Model>,
}

impl StreamService {
    /// Create the service.
    #[must_use]
    pub fn new(
        ctx: Arc<ServiceContext>,
        adapter: Arc<ProviderAdapter>,
        registry: Arc<ProviderRegistry>,
        metrics: Metrics,
    ) -> Self {
        Self {
            quota: QuotaService::new(Arc::clone(&ctx)),
            finalize: Arc::new(FinalizeService::new(Arc::clone(&ctx))),
            ctx,
            adapter,
            registry,
            metrics,
        }
    }

    /// `POST /v1/chats/{id}/messages:stream`.
    pub async fn send_message(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        body: StreamMessageRequest,
    ) -> Result<StreamSetup, DomainError> {
        if body.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let request_id = body.request_id.unwrap_or_else(Uuid::new_v4);
        let scope = self
            .ctx
            .pep
            .chat_scope(ctx, "send_message", Some(chat_id))
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        let chat = self
            .ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;

        // Idempotency check (highest priority).
        let existing = self
            .ctx
            .repos
            .turns
            .get_any(&conn, &scope, tenant_id, chat_id, request_id)
            .await?;
        if let Some(turn) = existing {
            if turn.state == state::COMPLETED && turn.deleted_at.is_none() {
                return self.replay(&scope, &chat, &turn).await;
            }
            return Err(DomainError::RequestIdConflict);
        }
        // Parallel turn guard (second priority).
        if self
            .ctx
            .repos
            .turns
            .any_running(&conn, &scope, tenant_id, chat_id)
            .await?
            .is_some()
        {
            return Err(DomainError::TurnAlreadyRunning);
        }

        let web_search_enabled = body.web_search.is_some_and(|w| w.enabled);
        let attachment_ids = body.attachment_ids.clone().unwrap_or_default();

        let bundle = self
            .preflight_phase(
                ctx,
                &scope,
                &chat,
                &body.content,
                &attachment_ids,
                web_search_enabled,
            )
            .await?;
        let prepared = self
            .assemble_phase(&chat, &body.content, web_search_enabled, bundle)
            .await?;
        self.commit_and_spawn(ctx, &chat, &body.content, &attachment_ids, request_id, prepared)
            .await
    }

    /// Mutation/send preflight (read-only): model resolution, attachment
    /// checks, kill switches, quota cascade, vision and input-length
    /// guards.
    pub(crate) async fn preflight_phase(
        &self,
        ctx: &SecurityContext,
        scope: &toolkit_db::secure::AccessScope,
        chat: &crate::infra::storage::entity::chats::Model,
        content: &str,
        attachment_ids: &[Uuid],
        web_search_enabled: bool,
    ) -> Result<PreflightBundle, DomainError> {

        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;

        // Model resolution (no enabled filter): the chat's model must exist.
        let policy = self.quota.current_policy(user_id).await?;
        if policy.snapshot.model(&chat.model).is_none() {
            return Err(DomainError::InvalidModel);
        }
        let kill = policy.snapshot.kill_switches;

        // Cheap attachment_ids checks before any query.
        if attachment_ids.len() as u64
            > u64::from(self.ctx.config.rag.max_documents_per_chat)
                + u64::from(self.ctx.config.rag.max_images_per_message)
        {
            return Err(DomainError::InvalidAttachment);
        }
        let mut seen = std::collections::HashSet::new();
        for id in attachment_ids {
            if !seen.insert(*id) {
                return Err(DomainError::InvalidAttachment);
            }
        }
        let rows = self
            .ctx
            .repos
            .attachments
            .get_many(&conn, scope, tenant_id, chat.id, attachment_ids)
            .await?;
        let resolved = Self::validate_attachments(&rows, attachment_ids, user_id)?;
        if resolved.image_count > self.ctx.config.rag.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }

        // Kill switches.
        if web_search_enabled && kill.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        if resolved.image_count > 0 && kill.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }

        // Attachment state of the chat for tool gating.
        let has_ready_documents = self
            .ctx
            .repos
            .attachments
            .has_ready_document(&conn, scope, tenant_id, chat.id)
            .await?;
        let ci_files = self
            .ctx
            .repos
            .attachments
            .ready_code_interpreter_files(&conn, scope, tenant_id, chat.id)
            .await?;
        let has_ready_ci = !ci_files.is_empty();

        // Snapshot boundary + prior context tokens.
        let boundary = self
            .ctx
            .repos
            .messages
            .latest_boundary(&conn, scope, tenant_id, chat.id)
            .await?;
        let prior_context_tokens = self
            .ctx
            .repos
            .messages
            .latest_assistant_usage(&conn, scope, tenant_id, chat.id)
            .await?
            .unwrap_or(0);

        // Quota preflight (cascade + daily tool quotas).
        let preflight_input = PreflightInput {
            tenant_id,
            user_id,
            selected_model: chat.model.clone(),
            content_bytes: content.len(),
            prior_context_tokens,
            image_count: resolved.image_count,
            has_ready_documents,
            has_ready_code_interpreter: has_ready_ci,
            web_search_requested: web_search_enabled,
        };
        let (policy, outcome) = self.quota.preflight(&preflight_input).await?;
        let effective = outcome.effective_model.clone();
        self.metrics.quota_preflight.add(
            1,
            &[
                label("decision", decision_label(&outcome.decision)),
                label("model", effective.id.clone()),
                label(
                    "tier",
                    match effective.tier {
                        mini_chat_sdk::ModelTier::Premium => "premium",
                        mini_chat_sdk::ModelTier::Standard => "standard",
                    },
                ),
            ],
        );
        self.metrics.quota_estimated_tokens.record(
            outcome.reserve_tokens as f64,
            &[],
        );

        // Vision check (after the cascade, before any provider call).
        if resolved.image_count > 0 && !effective.supports_vision() {
            return Err(DomainError::VisionNotSupported);
        }
        // INPUT_TOO_LONG check.
        if effective.max_input_tokens > 0 {
            let estimate = QuotaService::estimate_text_tokens(&effective, content.len());
            if estimate > i64::from(effective.max_input_tokens) {
                return Err(DomainError::InputTooLong);
            }
        }


        Ok(PreflightBundle {
            policy,
            outcome,
            resolved,
            has_ready_documents,
            ci_files,
            boundary,
            effective,
            tenant_id,
            user_id,
            scope: scope.clone(),
        })
    }

    /// Context assembly and provider resolution (send path: before the
    /// reserve transaction; mutation path: after the mutation commit).
    pub(crate) async fn assemble_phase(
        &self,
        chat: &crate::infra::storage::entity::chats::Model,
        content: &str,
        web_search_enabled: bool,
        bundle: PreflightBundle,
    ) -> Result<PreparedTurn, DomainError> {
        let tenant_id = bundle.tenant_id;
        let user_id = bundle.user_id;
        let conn = self.ctx.db.conn()?;
        let scope = bundle.scope.clone();
        let effective = bundle.effective.clone();
        let outcome = bundle.outcome.clone();
        let policy = bundle.policy;
        let resolved = bundle.resolved;
        let ci_files = bundle.ci_files;
        let has_ready_documents = bundle.has_ready_documents;
        let boundary = bundle.boundary;
        let kill = policy.snapshot.kill_switches;
        // Context assembly.
        let summary_row = self
            .ctx
            .repos
            .summaries
            .get(&conn, tenant_id, chat.id)
            .await?;
        let frontier = summary_row
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let recent = match boundary {
            Some(boundary) => {
                let mut rows = self
                    .ctx
                    .repos
                    .messages
                    .recent_for_context(
                        &conn,
                        &scope,
                        tenant_id,
                        chat.id,
                        boundary,
                        frontier,
                        self.ctx.config.context.recent_messages_limit,
                    )
                    .await?;
                rows.reverse();
                rows
            }
            None => Vec::new(),
        };

        // Tool inclusion and guards.
        let web_search_included = outcome.web_search_included;
        let file_search_included = outcome.file_search_included;
        let ci_included = outcome.code_interpreter_included;
        // Knowledge search: mutually exclusive with file_search; only when
        // enabled and the parameters can be built for this tenant.
        let knowledge_included = !file_search_included
            && self
                .ctx
                .knowledge
                .as_ref()
                .is_some_and(|k| k.available_for(&tenant_id.to_string()));
        let mut system_prompt = effective.system_prompt.clone();
        if file_search_included {
            if !system_prompt.is_empty() {
                system_prompt.push_str("\n\n");
            }
            system_prompt.push_str(&self.ctx.config.context.file_search_guard);
        }
        if web_search_included {
            if !system_prompt.is_empty() {
                system_prompt.push_str("\n\n");
            }
            system_prompt.push_str(&self.ctx.config.context.web_search_guard);
        }
        if knowledge_included {
            if !system_prompt.is_empty() {
                system_prompt.push_str("\n\n");
            }
            system_prompt.push_str(&self.ctx.config.knowledge_search.guard);
        }

        let tool_surcharges = tool_surcharges(
            &effective,
            has_ready_documents,
            !ci_files.is_empty(),
            web_search_enabled,
            &kill,
        );
        let recent_msgs: Vec<ContextMessage> = recent
            .into_iter()
            .map(|m| ContextMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();
        let summary_input = summary_row.as_ref().map(|s| {
            (
                context::summary_message_text(crate::config::SUMMARY_PREAMBLE, &s.summary_text),
                u32::try_from(s.token_estimate.max(0)).unwrap_or(0),
            )
        });
        let plan = context::assemble(
            &effective,
            i64::from(outcome.max_output_tokens_applied),
            tool_surcharges,
            AssemblyInput {
                system_prompt,
                summary: summary_input,
                recent: recent_msgs,
                user_message: content.to_owned(),
                image_count: resolved.image_count,
            },
        )?;

        // Provider resolution.
        let tenant_key = tenant_id.to_string();
        let provider = self
            .registry
            .resolve(&effective.provider_id, &tenant_key)
            .ok_or_else(|| {
                tracing::error!(
                    "mini-chat: no provider entry for model {} (provider_id {})",
                    effective.id,
                    effective.provider_id
                );
                DomainError::Internal
            })?;

        // Image file ids for the request (Anthropic chats use secondary ids).
        let image_file_ids = self.image_file_ids(&provider, &resolved.rows, &tenant_key);

        // Tools for the request.
        let mut tools: Vec<ToolSpec> = Vec::new();
        if file_search_included {
            let row = self
                .ctx
                .repos
                .vector_stores
                .get(&conn, tenant_id, chat.id)
                .await?;
            if let Some(vs) = row.and_then(|r| r.vector_store_id) {
                tools.push(ToolSpec::FileSearch {
                    vector_store_ids: vec![vs],
                    max_num_results: effective.max_num_results,
                });
            }
        }
        if web_search_included {
            tools.push(ToolSpec::WebSearch {
                search_context_size: effective.web_search_context_size,
            });
        }
        if ci_included {
            tools.push(ToolSpec::CodeInterpreter { file_ids: ci_files });
        }
        if knowledge_included {
            tools.push(ToolSpec::Function {
                name: "search_knowledge".to_owned(),
                description: "Search the organization knowledge base.".to_owned(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "top_k": {"type": "integer"},
                    },
                    "required": ["query"],
                }),
            });
        }

        let plan_summary_applied = plan
            .summary
            .as_ref()
            .map(|(_, estimate)| ThreadSummaryInfo {
                token_estimate: *estimate,
            });
        let plan_summary_text = plan.summary.clone().map(|(text, _)| text);
        let summary_frontier = frontier;
        let (effective_budget, _) =
            context::budgets(&effective, i64::from(outcome.max_output_tokens_applied), 0);
        let trigger_fired = context::summary_trigger(
            self.ctx.config.thread_summary_worker.enabled,
            summary_row.is_some(),
            plan.messages_truncated,
            plan.assembled_context_tokens,
            effective_budget,
            self.ctx.config.thread_summary_worker.compression_threshold_pct,
        );
        let system_prompt = plan.system_prompt.clone();

        Ok(PreparedTurn {
            policy,
            outcome,
            chat_id: chat.id,
            user_message: content.to_owned(),
            system_prompt,
            summary_frontier,
            trigger_fired,
            plan_summary_applied,
            plan_summary_text,
            tools,
            provider,
            image_file_ids,
            plan_messages: plan.messages,
            tenant_id,
            user_id,
            scope: scope.clone(),
            effective,
            turn_id: Uuid::new_v4(),
            web_search_requested: web_search_enabled,
        })
    }

    /// Commit the reserve transaction and spawn the provider task.
    async fn commit_and_spawn(
        &self,
        ctx: &SecurityContext,
        chat: &crate::infra::storage::entity::chats::Model,
        content: &str,
        attachment_ids: &[Uuid],
        request_id: Uuid,
        prepared: PreparedTurn,
    ) -> Result<StreamSetup, DomainError> {
        let tenant_id = prepared.tenant_id;
        let user_id = prepared.user_id;
        let scope = prepared.scope.clone();
        let outcome = prepared.outcome.clone();
        let turn_id = prepared.turn_id;
        let web_search_requested = prepared.web_search_requested;
        let chat_id = chat.id;
        let db = self.ctx.db.clone();
        let repos = self.ctx.repos.clone();
        let svc_ctx = Arc::clone(&self.ctx);
        let limits = prepared.policy.limits;
        let content_owned = content.to_owned();
        let attachment_ids_owned = attachment_ids.to_vec();

        let result = db
            .transaction(|tx| {
                let scope = scope.clone();
                let svc_ctx = Arc::clone(&svc_ctx);
                Box::pin(async move {
                    let quota = QuotaService::new(Arc::clone(&svc_ctx));
                    quota
                        .write_reserve(tx, tenant_id, user_id, &limits, &outcome)
                        .await?;
                    let user_msg_id = Uuid::new_v4();
                    repos
                        .messages
                        .insert(
                            tx,
                            &scope,
                            NewMessage {
                                id: user_msg_id,
                                tenant_id,
                                chat_id,
                                request_id: Some(request_id),
                                role: "user".to_owned(),
                                content: content_owned,
                                model: None,
                                provider_response_id: None,
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_tokens: 0,
                            },
                        )
                        .await?;
                    // Attachment validation inside the reserve transaction.
                    if !attachment_ids_owned.is_empty() {
                        let rows = repos
                            .attachments
                            .get_many(tx, &scope, tenant_id, chat_id, &attachment_ids_owned)
                            .await?;
                        validate_attachment_rows(&rows, &attachment_ids_owned, user_id)?;
                        repos
                            .message_attachments
                            .insert_batch(
                                tx,
                                &scope,
                                tenant_id,
                                chat_id,
                                user_msg_id,
                                &attachment_ids_owned,
                            )
                            .await?;
                    }
                    let insert = repos
                        .turns
                        .insert(
                            tx,
                            &scope,
                            crate::infra::storage::repo::turns::NewTurn {
                                id: turn_id,
                                tenant_id,
                                chat_id,
                                request_id,
                                requester_user_id: user_id,
                                web_search_enabled: web_search_requested,
                                reserve_tokens: outcome.reserve_tokens,
                                max_output_tokens_applied: outcome.max_output_tokens_applied,
                                reserved_credits_micro: outcome.reserved_credits_micro,
                                policy_version_applied: outcome.policy_version,
                                effective_model: outcome.effective_model.id.clone(),
                                minimal_generation_floor_applied: outcome
                                    .minimal_generation_floor_applied,
                            },
                        )
                        .await;
                    let turn = match insert {
                        Ok(turn) => turn,
                        Err(err) if err.is_unique_violation() => {
                            // Distinguish the request-id conflict from the
                            // running-turn race.
                            let existing = repos
                                .turns
                                .get_any(tx, &scope, tenant_id, chat_id, request_id)
                                .await?;
                            return Err(if existing.is_some() {
                                DomainError::RequestIdConflict
                            } else {
                                DomainError::TurnAlreadyRunning
                            });
                        }
                        Err(err) => return Err(err),
                    };
                    repos.chats.touch(tx, tenant_id, chat_id).await?;
                    Ok(turn)
                })
            })
            .await;
        let turn = result?;

        Ok(self.spawn_pipeline(ctx, chat, prepared, turn, request_id))
    }

    /// Mutation-path reserve: write the reserve and fill the new turn's
    /// preflight fields (the turn row was committed by the mutation
    /// transaction). A failure marks the new turn `failed` and returns the
    /// JSON error.
    pub(crate) async fn mutation_reserve_and_spawn(
        &self,
        ctx: &SecurityContext,
        chat: &crate::infra::storage::entity::chats::Model,
        prepared: PreparedTurn,
        request_id: Uuid,
    ) -> Result<StreamSetup, DomainError> {
        let tenant_id = prepared.tenant_id;
        let user_id = prepared.user_id;
        let turn_id = prepared.turn_id;
        let limits = prepared.policy.limits;
        let outcome = prepared.outcome.clone();
        let db = self.ctx.db.clone();
        let repos = self.ctx.repos.clone();
        let svc_ctx = Arc::clone(&self.ctx);
        let result = db
            .transaction(|tx| {
                let svc_ctx = Arc::clone(&svc_ctx);
                let outcome = outcome.clone();
                Box::pin(async move {
                    let quota = QuotaService::new(Arc::clone(&svc_ctx));
                    quota
                        .write_reserve(tx, tenant_id, user_id, &limits, &outcome)
                        .await?;
                    repos
                        .turns
                        .fill_preflight(
                            tx,
                            turn_id,
                            &crate::infra::storage::repo::turns::PreflightFields {
                                reserve_tokens: outcome.reserve_tokens,
                                max_output_tokens_applied: outcome.max_output_tokens_applied,
                                reserved_credits_micro: outcome.reserved_credits_micro,
                                policy_version_applied: outcome.policy_version,
                                effective_model: outcome.effective_model.id.clone(),
                                minimal_generation_floor_applied: outcome
                                    .minimal_generation_floor_applied,
                            },
                        )
                        .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(err) = result {
            // The mutation already committed the new turn; mark it failed.
            let code = if matches!(err, DomainError::QuotaExceeded(_)) {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            let conn = self.ctx.db.conn()?;
            drop(self.ctx.repos.turns.finalize_failed(&conn, turn_id, code, None).await);
            return Err(err);
        }
        let conn = self.ctx.db.conn()?;
        let turn = self
            .ctx
            .repos
            .turns
            .get_by_id(&conn, tenant_id, turn_id)
            .await?
            .ok_or(DomainError::Internal)?;
        Ok(self.spawn_pipeline(ctx, chat, prepared, turn, request_id))
    }

    /// Spawn the provider task and return the live stream setup.
    fn spawn_pipeline(
        &self,
        ctx: &SecurityContext,
        chat: &crate::infra::storage::entity::chats::Model,
        prepared: PreparedTurn,
        turn: chat_turns::Model,
        request_id: Uuid,
    ) -> StreamSetup {
        let (sender, receiver) = mpsc::channel(usize::from(
            self.ctx.config.streaming.sse_channel_capacity,
        ));
        let message_id = Uuid::new_v4();
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = StreamTaskHandle {
            ctx: Arc::clone(&self.ctx),
            finalize: Arc::clone(&self.finalize),
            adapter: Arc::clone(&self.adapter),
            metrics: self.metrics.clone(),
        };
        let ctx = ctx.clone();
        let chat_model = chat.model.clone();
        let chat_id = chat.id;
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            handle
                .run_pipeline(ctx, chat_id, chat_model, prepared, turn, request_id, message_id, sender, task_cancel)
                .await;
        });
        StreamSetup::Live {
            request_id,
            message_id,
            receiver,
            cancel,
        }
    }

    /// Buffered replay of a completed turn (side-effect-free).
    async fn replay(
        &self,
        scope: &toolkit_db::secure::AccessScope,
        chat: &crate::infra::storage::entity::chats::Model,
        turn: &chat_turns::Model,
    ) -> Result<StreamSetup, DomainError> {
        let conn = self.ctx.db.conn()?;
        let message_id = turn
            .assistant_message_id
            .ok_or(DomainError::Internal)?;
        let message = self
            .ctx
            .repos
            .messages
            .get(&conn, scope, turn.tenant_id, turn.chat_id, message_id)
            .await?
            .ok_or(DomainError::Internal)?;
        let effective_model = turn.effective_model.clone().unwrap_or_default();
        let downgraded = effective_model != chat.model;
        let events = vec![
            MiniChatSseEvent::StreamStarted(StreamStartedData {
                request_id: turn.request_id,
                message_id,
                is_new_turn: false,
                thread_summary_applied: None,
            }),
            MiniChatSseEvent::Delta(DeltaData {
                kind: DeltaKind::Text,
                content: message.content.clone(),
            }),
            MiniChatSseEvent::Done(DoneData {
                usage: Usage {
                    input_tokens: message.input_tokens,
                    output_tokens: message.output_tokens,
                },
                effective_model: effective_model.clone(),
                selected_model: chat.model.clone(),
                quota_decision: if downgraded {
                    QuotaDecisionKind::Downgrade
                } else {
                    QuotaDecisionKind::Allow
                },
                downgrade_from: if downgraded {
                    Some(chat.model.clone())
                } else {
                    None
                },
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ];
        Ok(StreamSetup::Replay(events))
    }

    /// Validate the fetched attachment rows against the request.
    fn validate_attachments(
        rows: &[attachments::Model],
        attachment_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<ResolvedAttachments, DomainError> {
        validate_attachment_rows(rows, attachment_ids, user_id)?;
        let mut image_count = 0_u32;
        for id in attachment_ids {
            let row = rows
                .iter()
                .find(|r| r.id == *id)
                .ok_or(DomainError::InvalidAttachment)?;
            if row.attachment_kind == "image" {
                image_count += 1;
            }
        }
        Ok(ResolvedAttachments {
            image_count,
            rows: rows.to_vec(),
        })
    }

    /// Resolve image file ids for the provider request. Anthropic chats use
    /// the secondary (Anthropic Files) ids; other file ids are dropped
    /// there.
    fn image_file_ids(
        &self,
        provider: &ResolvedProvider,
        rows: &[attachments::Model],
        tenant_key: &str,
    ) -> Vec<String> {
        let anthropic = self
            .registry
            .anthropic_files_target(&provider.provider_id, tenant_key);
        rows.iter()
            .filter(|r| r.attachment_kind == "image")
            .filter_map(|r| {
                if anthropic.is_some() {
                    r.secondary_file_id.clone()
                } else {
                    r.provider_file_id.clone()
                }
            })
            .collect()
    }
}

/// The preflight-only outcome shared by the send and mutation paths.
pub(crate) struct PreflightBundle {
    pub(crate) policy: crate::domain::quota_service::PolicySet,
    pub(crate) outcome: PreflightOutcome,
    pub(crate) resolved: ResolvedAttachments,
    pub(crate) has_ready_documents: bool,
    pub(crate) ci_files: Vec<String>,
    pub(crate) boundary: Option<(time::OffsetDateTime, Uuid)>,
    pub(crate) effective: mini_chat_sdk::ModelCatalogEntry,
    pub(crate) tenant_id: Uuid,
    pub(crate) user_id: Uuid,
    pub(crate) scope: toolkit_db::secure::AccessScope,
}

/// A prepared turn (preflight done, context assembled, provider resolved).
pub(crate) struct PreparedTurn {
    pub(crate) policy: crate::domain::quota_service::PolicySet,
    pub(crate) outcome: PreflightOutcome,
    pub(crate) chat_id: Uuid,
    pub(crate) user_message: String,
    pub(crate) system_prompt: String,
    pub(crate) plan_summary_applied: Option<ThreadSummaryInfo>,
    pub(crate) plan_summary_text: Option<String>,
    pub(crate) plan_messages: Vec<ContextMessage>,
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) provider: ResolvedProvider,
    pub(crate) image_file_ids: Vec<String>,
    pub(crate) tenant_id: Uuid,
    pub(crate) user_id: Uuid,
    pub(crate) scope: toolkit_db::secure::AccessScope,
    pub(crate) effective: mini_chat_sdk::ModelCatalogEntry,
    pub(crate) turn_id: Uuid,
    pub(crate) web_search_requested: bool,
    pub(crate) summary_frontier: Option<(time::OffsetDateTime, Uuid)>,
    pub(crate) trigger_fired: bool,
}

/// The Arc'd handle shared with the spawned provider task.
struct StreamTaskHandle {
    ctx: Arc<ServiceContext>,
    finalize: Arc<FinalizeService>,
    adapter: Arc<ProviderAdapter>,
    metrics: Metrics,
}


/// Validate fetched rows against the requested ids (in-transaction
/// authoritative check).
fn validate_attachment_rows(
    rows: &[attachments::Model],
    attachment_ids: &[Uuid],
    user_id: Uuid,
) -> Result<(), DomainError> {
    if rows.len() != attachment_ids.len() {
        return Err(DomainError::InvalidAttachment);
    }
    for row in rows {
        if row.uploaded_by_user_id != user_id || row.status != "ready" {
            return Err(DomainError::InvalidAttachment);
        }
    }
    Ok(())
}

/// `allow` | `downgrade` label for metrics.
fn decision_label(decision: &QuotaDecision) -> &'static str {
    match decision {
        QuotaDecision::Allow => "allow",
        QuotaDecision::Downgrade { .. } => "downgrade",
    }
}

/// Tool surcharges for the effective model (preflight formula, context
/// budget deduction).
fn tool_surcharges(
    model: &mini_chat_sdk::ModelCatalogEntry,
    has_ready_documents: bool,
    has_ready_code_interpreter: bool,
    web_search_requested: bool,
    kill: &mini_chat_sdk::KillSwitches,
) -> i64 {
    let budgets = &model.estimation_budgets;
    let mut total = 0_i64;
    if has_ready_documents
        && model.general_config.tool_support.file_search
        && !kill.disable_file_search
    {
        total += i64::from(budgets.tool_surcharge_tokens);
    }
    if web_search_requested && model.general_config.tool_support.web_search {
        total += i64::from(budgets.web_search_surcharge_tokens);
    }
    if has_ready_code_interpreter
        && model.general_config.tool_support.code_interpreter
        && !kill.disable_code_interpreter
    {
        total += i64::from(budgets.code_interpreter_surcharge_tokens);
    }
    total
}

impl StreamTaskHandle {
    /// The provider task: drives the provider stream, relays SSE events,
    /// enforces mid-turn tool limits, and finalizes the turn.
    #[allow(clippy::too_many_arguments)]
    async fn run_pipeline(
        self,
        ctx: SecurityContext,
        chat_id: Uuid,
        chat_model: String,
        prepared: PreparedTurn,
        turn: chat_turns::Model,
        request_id: Uuid,
        message_id: Uuid,
        sender: SseSender,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let started = std::time::Instant::now();
        let provider_label = prepared.provider.provider_id.clone();
        let model_label = prepared.outcome.effective_model.id.clone();
        self.metrics
            .stream_started
            .add(1, &[label("provider", provider_label.clone()), label("model", model_label.clone())]);
        self.metrics.active_streams.add(1, &[]);
        let _guard = ActiveStreamGuard(self.metrics.clone());

        // stream_started first.
        if sender
            .send(MiniChatSseEvent::StreamStarted(StreamStartedData {
                request_id,
                message_id,
                is_new_turn: true,
                thread_summary_applied: prepared.plan_summary_applied.clone(),
            }))
            .await
            .is_err()
        {
            self.cancel_turn(&turn, &chat_model, String::new(), 0).await;
            return;
        }

        let result = self
            .drive(&ctx, &prepared, &turn, chat_id, &sender, &cancel)
            .await;

        match result {
            DriveResult::Completed {
                text,
                usage,
                provider_response_id,
                incomplete_reason,
                citations,
                counts,
            } => {
                // Citations event before the terminal event.
                if !citations.is_empty() && sender
                    .send(MiniChatSseEvent::Citations(CitationsData { items: citations }))
                    .await
                    .is_err()
                {
                    self.cancel_turn(&turn, &chat_model, text, counts.web_search).await;
                    return;
                }
                let quota = quota_meta(&prepared);
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let fin_start = std::time::Instant::now();
                let (outcome, warnings) = self
                    .finalize
                    .finalize(
                        &turn,
                        &chat_model,
                        &quota,
                        TerminalInput::Completed {
                            text: text.clone(),
                            usage,
                            provider_response_id,
                            incomplete_reason: incomplete_reason.clone(),
                        },
                        counts,
                        latency_ms,
                        if prepared.trigger_fired {
                            Some(prepared.summary_frontier)
                        } else {
                            None
                        },
                    )
                    .await;
                self.metrics
                    .finalization_latency_ms
                    .record(fin_start.elapsed().as_secs_f64() * 1000.0, &[]);
                self.metrics.stream_total_latency_ms.record(
                    started.elapsed().as_secs_f64() * 1000.0,
                    &[
                        label("provider", provider_label.clone()),
                        label("model", model_label.clone()),
                    ],
                );
                match outcome {
                    FinalizeOutcome::Won(summary_scheduled) => {
                        if let Some(reason) = &incomplete_reason {
                            self.metrics.stream_incomplete.add(
                                1,
                                &[
                                    label("provider", provider_label.clone()),
                                    label("model", model_label.clone()),
                                    label("reason", reason.clone()),
                                ],
                            );
                        }
                        self.metrics.stream_completed.add(
                            1,
                            &[label("provider", provider_label), label("model", model_label)],
                        );
                        self.record_summary_trigger(summary_scheduled);
                        let (decision, downgrade_from, downgrade_reason) = decision_fields(&prepared, &chat_model);
                        let _ = sender
                            .send(MiniChatSseEvent::Done(DoneData {
                                usage: Usage {
                                    input_tokens: usage.map_or(0, |u| u.input_tokens),
                                    output_tokens: usage.map_or(0, |u| u.output_tokens),
                                },
                                effective_model: prepared.outcome.effective_model.id.clone(),
                                selected_model: chat_model,
                                quota_decision: decision,
                                downgrade_from,
                                downgrade_reason,
                                quota_warnings: warnings,
                            }))
                            .await;
                    }
                    FinalizeOutcome::PersistenceFailed => {
                        self.send_error(&sender, "message_persistence_failed", "The response could not be persisted").await;
                    }
                    FinalizeOutcome::TxFailed => {
                        self.send_error(&sender, "finalization_failed", "Internal error while finalizing the turn").await;
                    }
                    FinalizeOutcome::Lost => {
                        self.send_error(&sender, "stream_interrupted", "The stream was interrupted").await;
                    }
                }
            }
            DriveResult::Failed {
                code,
                message,
                usage,
                provider_response_id,
                counts,
                ..
            } => {
                let quota = quota_meta(&prepared);
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let (outcome, _) = self
                    .finalize
                    .finalize(
                        &turn,
                        &chat_model,
                        &quota,
                        TerminalInput::Failed {
                            error_code: code.clone(),
                            error_detail: Some(message.clone()),
                            usage,
                            provider_response_id,
                        },
                        counts,
                        latency_ms,
                        None,
                    )
                    .await;
                self.metrics.stream_failed.add(
                    1,
                    &[
                        label("provider", provider_label),
                        label("model", model_label),
                        label("error_code", code.clone()),
                    ],
                );
                match outcome {
                    FinalizeOutcome::Lost => {
                        self.send_error(&sender, "stream_interrupted", "The stream was interrupted").await;
                    }
                    _ => {
                        self.send_error(&sender, &code, &message).await;
                    }
                }
            }
            DriveResult::Cancelled { text, counts } => {
                self.metrics.stream_disconnected.add(1, &[label("stage", "mid_stream")]);
                self.cancel_turn_with_counts(&turn, &chat_model, text, counts).await;
            }
        }
    }

    /// Drive the agentic loop: provider request, optional `search_knowledge`
    /// retrieval, continuation.
    async fn drive(
        &self,
        ctx: &SecurityContext,
        prepared: &PreparedTurn,
        turn: &chat_turns::Model,
        chat_id: Uuid,
        sender: &SseSender,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> DriveResult {
        let knowledge_enabled = prepared
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::Function { name, .. } if name == "search_knowledge"));
        let max_calls = self.ctx.config.knowledge_search.max_calls_per_message;
        let max_iterations = max_calls + 2;
        let mut state = DriveState::default();
        let mut trailing: Vec<serde_json::Value> = Vec::new();
        let mut knowledge_calls = 0_u32;
        let mut iterations = 0_u32;
        loop {
            iterations += 1;
            let request = build_llm_request(prepared, Some(trailing.clone()));
            let mut stream = match self
                .adapter
                .stream_chat(ctx, &prepared.provider, &request)
                .await
            {
                Ok(stream) => stream,
                Err(err) => {
                    return provider_err_result(err, state.counts);
                }
            };
            match self
                .drive_stream(turn, chat_id, sender, &mut stream, state, cancel)
                .await
            {
                IterationResult::Terminal(result) => return result,
                IterationResult::Completed {
                    state: new_state,
                    usage,
                    provider_response_id,
                    incomplete_reason,
                } => {
                    state = new_state;
                    let Some((name, call_id, arguments)) = state.function_call.take() else {
                        return DriveResult::Completed {
                            text: state.text,
                            usage,
                            provider_response_id,
                            incomplete_reason,
                            citations: state.citations,
                            counts: state.counts,
                        };
                    };
                    if name != "search_knowledge" || !knowledge_enabled {
                        return DriveResult::Failed {
                            code: "unexpected_tool_use".to_owned(),
                            message: "the model requested an unsupported tool".to_owned(),
                            usage,
                            provider_response_id,
                            counts: state.counts,
                        };
                    }
                    if iterations > max_iterations {
                        return DriveResult::Failed {
                            code: "agentic_iterations_exceeded".to_owned(),
                            message: "knowledge search iteration limit exceeded".to_owned(),
                            usage,
                            provider_response_id,
                            counts: state.counts,
                        };
                    }
                    knowledge_calls += 1;
                    // The in-memory count includes failed retrievals.
                    state.counts.file_search += 1;
                    let output = if knowledge_calls > max_calls {
                        "search limit reached".to_owned()
                    } else {
                        match self.run_knowledge_search(ctx, prepared.tenant_id, &arguments).await {
                            Ok(output) => {
                                state.file_search_persist += 1;
                                self.persist_tool_counts(turn.id, &state.counts).await;
                                output
                            }
                            Err(err) => {
                                tracing::warn!("mini-chat: knowledge search failed: {err}");
                                "knowledge search failed".to_owned()
                            }
                        }
                    };
                    trailing.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": "search_knowledge",
                        "arguments": arguments,
                    }));
                    trailing.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                }
            }
        }
    }

    /// Run one knowledge retrieval and format the `function_call_output`.
    async fn run_knowledge_search(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        arguments: &str,
    ) -> Result<String, LlmProviderError> {
        let Some(knowledge) = &self.ctx.knowledge else {
            return Err(LlmProviderError::Unavailable(
                "knowledge search is not configured".to_owned(),
            ));
        };
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let top_k = args
            .get("top_k")
            .and_then(serde_json::Value::as_u64)
            .map(|v| usize::try_from(v).unwrap_or(usize::MAX));
        let started = std::time::Instant::now();
        let result = knowledge
            .search(ctx, &tenant_id.to_string(), &query, top_k)
            .await;
        self.metrics.knowledge_search_latency_ms.record(
            started.elapsed().as_secs_f64() * 1000.0,
            &[],
        );
        let chunks = match result {
            Ok(chunks) => {
                self.metrics
                    .knowledge_search
                    .add(1, &[label("result", "ok")]);
                self.metrics
                    .knowledge_search_chunks
                    .record(chunks.len() as f64, &[]);
                chunks
            }
            Err(err) => {
                self.metrics
                    .knowledge_search
                    .add(1, &[label("result", "error")]);
                return Err(err);
            }
        };
        if chunks.is_empty() {
            return Ok("No relevant knowledge found.".to_owned());
        }
        Ok(chunks.join("\n\n---\n\n"))
    }

    /// Process one provider event stream (one agentic-loop iteration).
    /// Process one provider event stream.
    #[allow(clippy::too_many_arguments)]
    async fn drive_stream(
        &self,
        turn: &chat_turns::Model,
        chat_id: Uuid,
        sender: &SseSender,
        stream: &mut crate::infra::llm::ProviderEventStream,
        mut state: DriveState,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> IterationResult {
        use futures::StreamExt;
        let mut last_progress = std::time::Instant::now();
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return IterationResult::Terminal(DriveResult::Cancelled {
                        text: state.text,
                        counts: state.counts,
                    });
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                // The provider stream ended without a terminal event.
                return IterationResult::Terminal(DriveResult::Failed {
                    code: "provider_error".to_owned(),
                    message: "The provider stream ended unexpectedly".to_owned(),
                    usage: None,
                    provider_response_id: None,
                    counts: state.counts,
                });
            };
            let event = match event {
                Ok(event) => event,
                Err(err) => {
                    return IterationResult::Terminal(provider_err_result(err, state.counts));
                }
            };
            // Disconnect detection: any send failure cancels the turn.
            match event {
                ProviderEvent::TextDelta(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    state.text.push_str(&text);
                    if sender
                        .send(MiniChatSseEvent::Delta(DeltaData {
                            kind: DeltaKind::Text,
                            content: text,
                        }))
                        .await
                        .is_err()
                    {
                        return IterationResult::Terminal(DriveResult::Cancelled {
                    text: state.text,
                            counts: state.counts,
                        });
                    }
                    self.refresh_progress(turn.id, &mut last_progress).await;
                }
                ProviderEvent::ReasoningDelta(text) => {
                    if sender
                        .send(MiniChatSseEvent::Delta(DeltaData {
                            kind: DeltaKind::Reasoning,
                            content: text,
                        }))
                        .await
                        .is_err()
                    {
                        return IterationResult::Terminal(DriveResult::Cancelled {
                    text: state.text,
                            counts: state.counts,
                        });
                    }
                }
                ProviderEvent::ToolStart { name, details } => {
                    match name.as_str() {
                        "web_search" => {
                            state.web_started += 1;
                            if state.web_started > self.ctx.config.quota.web_search_max_calls_per_message {
                                return IterationResult::Terminal(DriveResult::Failed {
                                    code: "web_search_calls_exceeded".to_owned(),
                                    message: "web search call limit exceeded".to_owned(),
                                    usage: None,
                                    provider_response_id: None,
                                    counts: state.counts,
                                });
                            }
                        }
                        "code_interpreter" => {
                            state.ci_started += 1;
                            if state.ci_started > self.ctx.config.quota.code_interpreter_max_calls_per_message {
                                return IterationResult::Terminal(DriveResult::Failed {
                                    code: "code_interpreter_calls_exceeded".to_owned(),
                                    message: "code interpreter call limit exceeded".to_owned(),
                                    usage: None,
                                    provider_response_id: None,
                                    counts: state.counts,
                                });
                            }
                        }
                        _ => {}
                    }
                    if sender
                        .send(MiniChatSseEvent::Tool(ToolData {
                            phase: ToolPhase::Start,
                            name,
                            details,
                        }))
                        .await
                        .is_err()
                    {
                        return IterationResult::Terminal(DriveResult::Cancelled {
                    text: state.text,
                            counts: state.counts,
                        });
                    }
                    self.refresh_progress(turn.id, &mut last_progress).await;
                }
                ProviderEvent::ToolDone { name, details } => {
                    match name.as_str() {
                        "web_search" => state.counts.web_search += 1,
                        "code_interpreter" => state.counts.code_interpreter += 1,
                        "file_search" => {
                            state.counts.file_search += 1;
                            state.file_search_persist += 1;
                        }
                        _ => {}
                    }
                    self.persist_file_search_count(turn.id, &state).await;
                    if sender
                        .send(MiniChatSseEvent::Tool(ToolData {
                            phase: ToolPhase::Done,
                            name,
                            details,
                        }))
                        .await
                        .is_err()
                    {
                        return IterationResult::Terminal(DriveResult::Cancelled {
                    text: state.text,
                            counts: state.counts,
                        });
                    }
                }
                ProviderEvent::FunctionCall {
                    name,
                    call_id,
                    arguments,
                } => {
                    state.function_call = Some((name, call_id, arguments));
                }
                ProviderEvent::Citations(raw) => {
                    state.citations = self.resolve_citations(turn, chat_id, raw).await;
                }
                ProviderEvent::CompletedWithCitations {
                    usage,
                    provider_response_id,
                    incomplete_reason,
                    citations,
                } => {
                    state.citations = self.resolve_citations(turn, chat_id, citations).await;
                    return IterationResult::Completed {
                        state,
                        usage,
                        provider_response_id,
                        incomplete_reason,
                    };
                }
                ProviderEvent::Completed {
                    usage,
                    provider_response_id,
                    incomplete_reason,
                } => {
                    return IterationResult::Completed {
                        state,
                        usage,
                        provider_response_id,
                        incomplete_reason,
                    };
                }
                ProviderEvent::Failed {
                    code,
                    message,
                    usage,
                    provider_response_id,
                } => {
                    return IterationResult::Terminal(DriveResult::Failed {
                        code,
                        message,
                        usage,
                        provider_response_id,
                        counts: state.counts,
                    });
                }
            }
        }
    }

    /// Resolve raw provider citations against the chat's attachments.
    async fn resolve_citations(
        &self,
        turn: &chat_turns::Model,
        chat_id: Uuid,
        raw: Vec<RawCitation>,
    ) -> Vec<Citation> {
        let has_file = raw.iter().any(|c| c.source == CitationSource::File);
        let map = if has_file {
            let Ok(conn) = self.ctx.db.conn() else {
                return Vec::new();
            };
            let rows = self
                .ctx
                .repos
                .attachments
                .ready_with_provider_ids(
                    &conn,
                    &toolkit_db::secure::AccessScope::allow_all(),
                    turn.tenant_id,
                    chat_id,
                )
                .await
                .unwrap_or_default();
            rows.into_iter()
                .filter_map(|r| r.provider_file_id.map(|id| (id, (r.id, r.filename))))
                .collect::<std::collections::HashMap<_, _>>()
        } else {
            std::collections::HashMap::new()
        };
        raw.into_iter()
            .filter_map(|c| match c.source {
                CitationSource::Web => Some(Citation {
                    source: CitationSource::Web,
                    title: c.title.unwrap_or_default(),
                    snippet: c.snippet.unwrap_or_default(),
                    url: c.url,
                    attachment_id: None,
                    span: c.span.map(|(start, end)| crate::api::rest::dto::TextSpan { start, end }),
                    score: None,
                }),
                CitationSource::File => {
                    let file_id = c.provider_file_id?;
                    let (attachment_id, filename) = map.get(&file_id)?;
                    Some(Citation {
                        source: CitationSource::File,
                        title: filename.clone(),
                        snippet: String::new(),
                        url: None,
                        attachment_id: Some(*attachment_id),
                        span: None,
                        score: None,
                    })
                }
            })
            .collect()
    }

    /// Refresh `last_progress_at` at most every 30 s.
    async fn refresh_progress(&self, turn_id: Uuid, last: &mut std::time::Instant) {
        if last.elapsed() < std::time::Duration::from_secs(30) {
            return;
        }
        *last = std::time::Instant::now();
        let Ok(conn) = self.ctx.db.conn() else { return };
        drop(self
            .ctx
            .repos
            .turns
            .refresh_progress(&conn, turn_id, time::OffsetDateTime::now_utc())
            .await);
    }

    /// Persist the completed tool-call counters on the running turn.
    async fn persist_tool_counts(&self, turn_id: Uuid, counts: &ToolCallCounts) {
        let Ok(conn) = self.ctx.db.conn() else { return };
        drop(self
            .ctx
            .repos
            .turns
            .set_tool_counts(&conn, turn_id, counts.web_search, counts.code_interpreter, counts.file_search)
            .await);
    }

    /// Persist the provider-native/recovered file search count separately.
    async fn persist_file_search_count(&self, turn_id: Uuid, counts: &DriveState) {
        let Ok(conn) = self.ctx.db.conn() else { return };
        drop(self
            .ctx
            .repos
            .turns
            .set_tool_counts(&conn, turn_id, counts.counts.web_search, counts.counts.code_interpreter, counts.file_search_persist)
            .await);
    }

    /// Send a terminal SSE error (best-effort).
    async fn send_error(&self, sender: &SseSender, code: &str, message: &str) {
        let _ = sender
            .send(MiniChatSseEvent::Error(ErrorData {
                code: code.to_owned(),
                message: message.to_owned(),
            }))
            .await;
    }

    /// Finalize a turn as cancelled (client disconnect).
    async fn cancel_turn(&self, turn: &chat_turns::Model, chat_model: &str, text: String, web_search: i32) {
        self.cancel_turn_with_counts(
            turn,
            chat_model,
            text,
            ToolCallCounts {
                web_search,
                code_interpreter: 0,
                file_search: 0,
            },
        )
        .await;
    }

    async fn cancel_turn_with_counts(&self, turn: &chat_turns::Model, chat_model: &str, text: String, counts: ToolCallCounts) {
        let quota = QuotaMeta {
            decision: "unknown",
            downgrade_from: None,
            downgrade_reason: None,
            daily_start: crate::domain::services::periods::daily_start(turn.started_at),
            monthly_start: crate::domain::services::periods::monthly_start(turn.started_at),
        };
        let (outcome, _) = self
            .finalize
            .finalize(
                turn,
                chat_model,
                &quota,
                TerminalInput::Cancelled { partial_text: text },
                counts,
                0,
                None,
            )
            .await;
        if matches!(outcome, FinalizeOutcome::Won(_)) {
            self.metrics
                .cancel_effective
                .add(1, &[label("trigger", "disconnect")]);
            self.metrics
                .streams_aborted
                .add(1, &[label("trigger", "client_disconnect")]);
        }
    }

    /// Record the thread summary trigger metric after a completed
    /// finalization.
    fn record_summary_trigger(&self, scheduled: bool) {
        if !self.ctx.config.thread_summary_worker.enabled {
            return;
        }
        let result = if scheduled { "scheduled" } else { "not_needed" };
        self.metrics
            .thread_summary_trigger
            .add(1, &[label("result", result)]);
    }
}

/// Per-request drive state.
#[derive(Default)]
struct DriveState {
    text: String,
    counts: ToolCallCounts,
    /// Provider-native `file_search` completions plus successful knowledge
    /// retrievals (persisted on `chat_turns.file_search_completed_count`).
    file_search_persist: i32,
    web_started: u32,
    ci_started: u32,
    citations: Vec<Citation>,
    function_call: Option<(String, String, String)>,
}

/// The outcome of one agentic-loop iteration.
enum IterationResult {
    /// The provider completed the request.
    Completed {
        state: DriveState,
        usage: Option<UsageTokens>,
        provider_response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    /// The iteration ended the turn (failed or cancelled).
    Terminal(DriveResult),
}

/// The outcome of driving the provider stream.
enum DriveResult {
    Completed {
        text: String,
        usage: Option<UsageTokens>,
        provider_response_id: Option<String>,
        incomplete_reason: Option<String>,
        citations: Vec<Citation>,
        counts: ToolCallCounts,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
        provider_response_id: Option<String>,
        counts: ToolCallCounts,
    },
    Cancelled {
        text: String,
        counts: ToolCallCounts,
    },
}

/// Map a provider-layer error to a failed drive result.
fn provider_err_result(
    err: LlmProviderError,
    counts: ToolCallCounts,
) -> DriveResult {
    match err {
        LlmProviderError::RateLimited { retry_after } => {
            let message = match retry_after {
                Some(secs) => format!("Provider is rate limited, retry in {secs}s"),
                None => "Provider is rate limited".to_owned(),
            };
            DriveResult::Failed {
                code: "rate_limited".to_owned(),
                message,
                usage: None,
                provider_response_id: None,
                counts,
            }
        }
        LlmProviderError::Timeout => DriveResult::Failed {
            code: "provider_timeout".to_owned(),
            message: "The provider request timed out".to_owned(),
            usage: None,
            provider_response_id: None,
            counts,
        },
        LlmProviderError::Provider {
            message,
            usage,
            provider_response_id,
            ..
        } => DriveResult::Failed {
            code: "provider_error".to_owned(),
            message,
            usage,
            provider_response_id,
            counts,
        },
        other => DriveResult::Failed {
            code: "provider_error".to_owned(),
            message: format!("{other}"),
            usage: None,
            provider_response_id: None,
            counts,
        },
    }
}

/// Quota metadata for finalization from a prepared turn.
fn quota_meta(prepared: &PreparedTurn) -> QuotaMeta {
    let (decision, from, reason) = decision_fields_raw(&prepared.outcome);
    QuotaMeta {
        decision,
        downgrade_from: from.map(str::to_owned),
        downgrade_reason: reason,
        daily_start: prepared.outcome.daily_start,
        monthly_start: prepared.outcome.monthly_start,
    }
}

fn decision_fields_raw(outcome: &PreflightOutcome) -> (&'static str, Option<&str>, Option<&'static str>) {
    match &outcome.decision {
        QuotaDecision::Allow => ("allow", None, None),
        QuotaDecision::Downgrade { reason } => ("downgrade", None, Some(*reason)),
    }
}

fn decision_fields(prepared: &PreparedTurn, selected_model: &str) -> (QuotaDecisionKind, Option<String>, Option<String>) {
    match &prepared.outcome.decision {
        QuotaDecision::Allow => (QuotaDecisionKind::Allow, None, None),
        QuotaDecision::Downgrade { reason } => (
            QuotaDecisionKind::Downgrade,
            Some(selected_model.to_owned()),
            Some((*reason).to_owned()),
        ),
    }
}

/// Decrement active streams on drop.
struct ActiveStreamGuard(Metrics);

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        self.0.active_streams.add(-1, &[]);
    }
}

/// Build the provider request from a prepared turn (+ agentic trailing
/// items).
fn build_llm_request(prepared: &PreparedTurn, trailing: Option<Vec<serde_json::Value>>) -> LlmRequest {
    let mut input: Vec<InputMessage> = Vec::new();
    if let Some(summary_text) = &prepared.plan_summary_text {
        input.push(InputMessage {
            role: "user".to_owned(),
            text: summary_text.clone(),
            images: Vec::new(),
        });
    }
    for msg in &prepared.plan_messages {
        input.push(InputMessage {
            role: msg.role.clone(),
            text: msg.content.clone(),
            images: Vec::new(),
        });
    }
    input.push(InputMessage {
        role: "user".to_owned(),
        text: prepared.user_message.clone(),
        images: prepared.image_file_ids.clone(),
    });
    let feature = feature_label(&prepared.tools);
    let tenant_hex = prepared.tenant_id.as_simple().to_string();
    let user_hex = prepared.user_id.as_simple().to_string();
    LlmRequest {
        provider_model_id: prepared.effective.provider_model_id.clone(),
        instructions: prepared.system_prompt.clone(),
        input,
        trailing_items: trailing.unwrap_or_default(),
        tools: prepared.tools.clone(),
        max_output_tokens: prepared.outcome.max_output_tokens_applied.max(0) as u32,
        max_tool_calls: prepared.effective.max_tool_calls,
        user: format!("{tenant_hex}{user_hex}"),
        metadata: Some(serde_json::Map::from_iter([
            ("tenant_id".to_owned(), serde_json::Value::from(prepared.tenant_id.to_string())),
            ("user_id".to_owned(), serde_json::Value::from(prepared.user_id.to_string())),
            ("chat_id".to_owned(), serde_json::Value::from(prepared.chat_id.to_string())),
            ("request_type".to_owned(), serde_json::Value::from("chat")),
            ("feature".to_owned(), serde_json::Value::from(feature)),
        ])),
        api_params: prepared.effective.general_config.api_params.clone(),
        stream: true,
    }
}

/// The `feature` metadata label from the tool list.
fn feature_label(tools: &[ToolSpec]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for tool in tools {
        parts.push(match tool {
            ToolSpec::FileSearch { .. } => "file_search",
            ToolSpec::WebSearch { .. } => "web_search",
            ToolSpec::CodeInterpreter { .. } => "code_interpreter",
            ToolSpec::Function { .. } => "search_knowledge",
        });
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join("+")
    }
}
