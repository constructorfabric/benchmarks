//! Stream service: the send pipeline of `POST /v1/chats/{id}/messages:stream`
//! (DESIGN section 3.6 "Send Message with Streaming Response", section 3.3
//! "Streaming Contract", section 4 "Parallel Turn Policy" and "Idempotency
//! Rules", section 5.7 "Terminal SSE Event Emission Guard").
//!
//! - `setup`: validation, idempotency, preflight, context assembly and the
//!   reserve transaction (everything before the provider call).
//! - `replay`: the side-effect-free replay of a completed turn (separate code
//!   path without access to quota, settlement or the outbox).
//! - `provider_task`: the spawned task that streams the provider response,
//!   enforces the mid-turn tool limits and finalizes the turn.
//! - `relay`: the event stream handed to the SSE writer.

pub mod events;
mod provider_task;
pub mod relay;
mod replay;
mod setup;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, ChatAction, LlmClient, ResolvedProvider};
use crate::domain::services::finalization_service::FinalizationService;
use crate::domain::services::model_service::ModelService;
use crate::domain::services::quota_service::{PreflightDecision, PreflightInput, QuotaService};
use crate::domain::services::turn_service::{MutationKind, Replacement, TurnService};
use crate::domain::time::db_now;
use crate::infra::db::entities::chat;
use crate::infra::db::repos::turn_repo::NewRunningTurn;
use crate::infra::db::repos::{chat_repo, turn_repo};
use crate::infra::llm::ProviderResolver;
use crate::infra::llm::knowledge::{KnowledgeRetriever, KnowledgeTarget};

pub use events::StreamEvent;

/// Identity of a committed turn handed to the provider task.
#[derive(Clone, Copy, Debug)]
struct NewTurn {
    id: Uuid,
    request_id: Uuid,
    requester: Uuid,
}

/// A `messages:stream` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendMessage {
    pub chat_id: Uuid,
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
}

/// Cancels the provider task of a live turn when dropped (client disconnect).
pub struct DisconnectGuard {
    _cancel_on_drop: DropGuard,
}

impl DisconnectGuard {
    fn new(token: &CancellationToken) -> Self {
        Self {
            _cancel_on_drop: token.clone().drop_guard(),
        }
    }
}

impl std::fmt::Debug for DisconnectGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DisconnectGuard")
    }
}

/// Outcome of a successful stream setup.
#[derive(Debug)]
pub enum StreamStart {
    /// A new turn: events of the provider task.
    Live(mpsc::Receiver<StreamEvent>, DisconnectGuard),
    /// Idempotent replay of a completed turn (`stream_started`, one `delta`,
    /// `done`).
    Replay(Vec<StreamEvent>),
}

pub struct StreamService {
    db: Arc<DBProvider<DomainError>>,
    cfg: Arc<MiniChatConfig>,
    authz: Arc<dyn AuthzPort>,
    models: Arc<ModelService>,
    quota: Arc<QuotaService>,
    finalization: Arc<FinalizationService>,
    llm: Arc<dyn LlmClient>,
    resolver: Arc<ProviderResolver>,
    turns: Arc<TurnService>,
    knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

/// Dependencies of [`StreamService::new`].
pub struct StreamDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub cfg: Arc<MiniChatConfig>,
    pub authz: Arc<dyn AuthzPort>,
    pub models: Arc<ModelService>,
    pub quota: Arc<QuotaService>,
    pub finalization: Arc<FinalizationService>,
    pub llm: Arc<dyn LlmClient>,
    pub resolver: Arc<ProviderResolver>,
    pub turns: Arc<TurnService>,
    /// Knowledge retriever; `Some` only when `knowledge_search.enabled`.
    pub knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

impl StreamService {
    #[must_use]
    pub fn new(d: StreamDeps) -> Self {
        Self {
            db: d.db,
            cfg: d.cfg,
            authz: d.authz,
            models: d.models,
            quota: d.quota,
            finalization: d.finalization,
            llm: d.llm,
            resolver: d.resolver,
            turns: d.turns,
            knowledge: d.knowledge,
        }
    }

    /// Starts a turn (or replays a completed one). Checks in DESIGN section
    /// 3.6 order: content, attachment ids, authorization, chat, idempotency
    /// (replay or `RequestIdConflict`), parallel turn guard, chat model, image
    /// count, quota preflight, input length, image guards, context assembly,
    /// provider resolution, then the reserve transaction; the provider task
    /// is spawned only after it committed.
    ///
    /// # Errors
    /// Every pre-stream rejection (DESIGN section 3.3 REST error mapping).
    pub async fn send(
        &self,
        ctx: SecurityContext,
        req: SendMessage,
    ) -> Result<StreamStart, DomainError> {
        setup::validate_content(&req.content)?;
        setup::validate_attachment_ids(&req.attachment_ids, &self.cfg.rag)?;
        let scope = self
            .authz
            .chat_scope(&ctx, ChatAction::SendMessage, Some(req.chat_id))
            .await?;
        let conn = self.db.conn()?;
        let chat = chat_repo::find_scoped(&conn, &scope, req.chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        if let Some(request_id) = req.request_id
            && let Some(turn) =
                turn_repo::find_by_request(&conn, chat.tenant_id, chat.id, request_id).await?
        {
            return replay::replay_or_conflict(&conn, &chat, &turn)
                .await
                .map(StreamStart::Replay);
        }
        if turn_repo::find_running(&conn, chat.tenant_id, chat.id)
            .await?
            .is_some()
        {
            return Err(DomainError::TurnAlreadyRunning);
        }
        let user_id = ctx.subject_id();
        self.models
            .resolve_for_chat(user_id, &chat.model, false)
            .await?;

        let mut facts = setup::gather_facts(
            &conn,
            &chat,
            user_id,
            &req.attachment_ids,
            &self.cfg.rag,
            None,
        )
        .await?;
        facts.knowledge = self.knowledge_target(chat.tenant_id);
        let now = db_now();
        let decision = self
            .quota
            .preflight(&PreflightInput {
                tenant_id: chat.tenant_id,
                user_id,
                selected_model: chat.model.clone(),
                message_bytes: req.content.len(),
                prior_context_tokens: facts.prior_context_tokens,
                num_images: facts.num_images(),
                tool_ctx: facts.tool_ctx(req.web_search_enabled),
                now,
            })
            .await?;
        setup::check_effective_model(&decision, &req.content, facts.num_images())?;
        let prepared = setup::prepare_request(
            &conn,
            &self.cfg,
            &chat,
            user_id,
            &decision,
            &facts,
            &req.content,
        )
        .await?;
        let provider = self
            .resolver
            .resolve(&decision.effective_model.provider_id, chat.tenant_id)?;

        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let send = setup::NewSend {
            reserve: setup::reserve_request(&chat, user_id, &decision),
            message: setup::NewSend::user_message(&chat, request_id, &req.content, now),
            attachment_ids: req.attachment_ids.clone(),
            turn: NewRunningTurn {
                id: Uuid::new_v4(),
                tenant_id: chat.tenant_id,
                chat_id: chat.id,
                request_id,
                requester_user_id: user_id,
                preflight: Some(setup::turn_preflight(&decision)),
                web_search_enabled: req.web_search_enabled,
                now,
            },
        };
        let turn_id = send.turn.id;
        setup::commit_send(&self.db, &self.quota, send).await?;
        Ok(self.spawn_turn(
            &chat,
            NewTurn {
                id: turn_id,
                request_id,
                requester: user_id,
            },
            &decision,
            prepared,
            provider,
        ))
    }

    /// `POST /v1/chats/{id}/turns/{request_id}/retry`: re-submits the last
    /// turn's user message (and its live attachments) as a new turn with a
    /// server-generated request id (DESIGN section 3.6 retry/edit variant,
    /// section 3.9).
    ///
    /// # Errors
    /// Preview rejections (`NotFound`, `NotLatestTurn`, `TurnNotTerminal`,
    /// `NotRequester`), preflight rejections (`InvalidModel`, `QuotaExceeded`,
    /// kill switches, image guards, `InputTooLong`) with nothing changed,
    /// `GenerationInProgress`, and failures after the mutation commit
    /// (`ContextBudgetExceeded`, `QuotaExceeded`, provider resolution) with
    /// the new turn `failed`.
    pub async fn retry(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<StreamStart, DomainError> {
        self.mutate(&ctx, chat_id, request_id, MutationKind::Retry, None)
            .await
    }

    /// `PATCH /v1/chats/{id}/turns/{request_id}`: like [`Self::retry`] with
    /// new content (`EmptyContent` after the preview checks); the original
    /// attachments are re-linked.
    ///
    /// # Errors
    /// See [`Self::retry`]; `EmptyContent`.
    pub async fn edit(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<StreamStart, DomainError> {
        self.mutate(&ctx, chat_id, request_id, MutationKind::Edit, Some(content))
            .await
    }

    /// The retry/edit pipeline in DESIGN section 3.6 order: (1) preview,
    /// (2) chat model (no enabled filter), quota preflight and image guards,
    /// all read-only; (3) mutation commit; (4) context assembly, provider
    /// resolution and the reserve transaction (which also writes the turn's
    /// preflight columns) -- a failure here marks the new turn `failed`;
    /// (5) the provider task, as for a send.
    async fn mutate(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: MutationKind,
        new_content: Option<String>,
    ) -> Result<StreamStart, DomainError> {
        let target = self.turns.preview(ctx, chat_id, request_id, kind).await?;
        let content = match new_content {
            Some(c) => {
                setup::validate_content(&c)?;
                c
            }
            None => target
                .user_message
                .as_ref()
                .map(|m| m.content.clone())
                .ok_or_else(|| {
                    DomainError::Internal(format!("turn {} has no user message", target.turn.id))
                })?,
        };
        let (chat, user_id) = (&target.chat, target.actor_user_id);
        self.models
            .resolve_for_chat(user_id, &chat.model, false)
            .await?;
        let attachment_ids: Vec<Uuid> = target.attachments.iter().map(|a| a.id).collect();
        let conn = self.db.conn()?;
        let mut facts = setup::gather_facts(
            &conn,
            chat,
            user_id,
            &attachment_ids,
            &self.cfg.rag,
            Some(target.turn.request_id),
        )
        .await?;
        facts.knowledge = self.knowledge_target(chat.tenant_id);
        let web_search_enabled = target.turn.web_search_enabled;
        let decision = self
            .quota
            .preflight(&PreflightInput {
                tenant_id: chat.tenant_id,
                user_id,
                selected_model: chat.model.clone(),
                message_bytes: content.len(),
                prior_context_tokens: facts.prior_context_tokens,
                num_images: facts.num_images(),
                tool_ctx: facts.tool_ctx(web_search_enabled),
                now: db_now(),
            })
            .await?;
        setup::check_effective_model(&decision, &content, facts.num_images())?;

        let new = NewTurn {
            id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            requester: user_id,
        };
        let now = db_now();
        let replacement = Replacement {
            message: setup::NewSend::user_message(chat, new.request_id, &content, now),
            attachment_ids,
            turn: NewRunningTurn {
                id: new.id,
                tenant_id: chat.tenant_id,
                chat_id: chat.id,
                request_id: new.request_id,
                requester_user_id: user_id,
                preflight: None,
                web_search_enabled,
                now,
            },
        };
        self.turns.commit_replacement(&target, replacement).await?;

        let started = async {
            let prepared = setup::prepare_request(
                &conn, &self.cfg, chat, user_id, &decision, &facts, &content,
            )
            .await?;
            let provider = self
                .resolver
                .resolve(&decision.effective_model.provider_id, chat.tenant_id)?;
            setup::commit_reserve(
                &self.db,
                &self.quota,
                &setup::reserve_request(chat, user_id, &decision),
                new.id,
                setup::turn_preflight(&decision),
            )
            .await?;
            Ok::<_, DomainError>((prepared, provider))
        }
        .await;
        match started {
            Ok((prepared, provider)) => {
                Ok(self.spawn_turn(chat, new, &decision, prepared, provider))
            }
            Err(e) => {
                let code = setup::unstarted_error_code(&e);
                if let Err(fe) = self
                    .finalization
                    .finalize_unstarted(chat.tenant_id, new.id, code)
                    .await
                {
                    tracing::error!(
                        turn_id = %new.id,
                        error = %fe,
                        "failed to finalize an unstarted retry/edit turn"
                    );
                }
                Err(e)
            }
        }
    }

    /// Spawns the provider task of a committed turn (shared by send, retry
    /// and edit: one streaming, finalization and outbox path).
    fn spawn_turn(
        &self,
        chat: &chat::Model,
        new: NewTurn,
        decision: &PreflightDecision,
        prepared: setup::PreparedRequest,
        provider: ResolvedProvider,
    ) -> StreamStart {
        let run = provider_task::TurnRun {
            turn_id: new.id,
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id: new.request_id,
            user_id: new.requester,
            assistant_message_id: Uuid::new_v4(),
            selected_model: chat.model.clone(),
            effective_model: decision.effective_model.id.clone(),
            premium: decision.premium(),
            policy_version: decision.snapshot.policy_version,
            reserve_tokens: decision.reserve_tokens,
            reserved_credits_micro: decision.reserved_credits_micro,
            max_output_tokens_applied: decision.max_output_tokens_applied,
            minimal_generation_floor_applied: decision.minimal_generation_floor_applied,
            periods: decision.periods,
            quota_decision: decision.decision,
            downgrade_reason: decision.downgrade_reason,
            summary_trigger: prepared.summary_trigger,
            thread_summary_applied: prepared.thread_summary_applied,
            citation_map: prepared.citation_map,
            provider,
            request: prepared.request,
            knowledge: prepared
                .knowledge
                .and_then(|target| self.knowledge_run(target)),
        };
        provider_task::spawn(self.task_deps(), run)
    }

    /// The knowledge-search target of a request (DESIGN section 4 "Knowledge
    /// Search"): `None` (knowledge search off) unless enabled, the retriever
    /// is configured and the `knowledge_search.provider_id` entry is usable
    /// for the tenant; a missing piece is logged.
    fn knowledge_target(&self, tenant_id: Uuid) -> Option<KnowledgeTarget> {
        if !self.cfg.knowledge_search.enabled {
            return None;
        }
        self.try_knowledge_target(tenant_id)
            .map_err(|reason| {
                tracing::warn!(reason = %reason, "knowledge search is off for this request");
            })
            .ok()
    }

    fn try_knowledge_target(&self, tenant_id: Uuid) -> Result<KnowledgeTarget, String> {
        if self.knowledge.is_none() {
            return Err("no knowledge retriever is configured".to_owned());
        }
        let k = &self.cfg.knowledge_search;
        let (Some(provider_id), Some(vector_store_id)) =
            (k.provider_id.as_deref(), k.vector_store_id.as_deref())
        else {
            return Err("knowledge_search.provider_id or vector_store_id is not set".to_owned());
        };
        self.resolver
            .knowledge_target(provider_id, vector_store_id, tenant_id)
            .map_err(|e| e.to_string())
    }

    fn knowledge_run(&self, target: KnowledgeTarget) -> Option<provider_task::KnowledgeRun> {
        let k = &self.cfg.knowledge_search;
        self.knowledge
            .as_ref()
            .map(|retriever| provider_task::KnowledgeRun {
                retriever: Arc::clone(retriever),
                target,
                max_calls: k.max_calls_per_message,
                top_k: k.top_k,
                max_chunk_chars: k.max_chunk_chars,
            })
    }

    fn task_deps(&self) -> provider_task::TaskDeps {
        let s = &self.cfg.streaming;
        provider_task::TaskDeps {
            llm: Arc::clone(&self.llm),
            finalization: Arc::clone(&self.finalization),
            db: Arc::clone(&self.db),
            ping_interval: Duration::from_secs(u64::from(s.sse_ping_interval_seconds.max(1))),
            web_search_max_calls: self.cfg.quota.web_search_max_calls_per_message,
            code_interpreter_max_calls: self.cfg.quota.code_interpreter_max_calls_per_message,
            channel_capacity: usize::from(s.sse_channel_capacity),
        }
    }
}

#[cfg(test)]
#[path = "stream_service_tests.rs"]
mod stream_service_tests;
