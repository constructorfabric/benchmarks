//! Send-message streaming pipeline (DESIGN "Send Message with Streaming Response", sections 3.7
//! "Turn Lifecycle", 3.9 "Turn Mutation Rules", 5.7 "Turn Finalization Contract").
//!
//! - [`setup`]: validation, authorization, idempotency and parallel-turn checks, preflight,
//!   context assembly and the reserve transaction; produces a [`TurnContext`] (or a replay).
//!   Retry and edit reuse its steps around the mutation commit ([`StreamService::run_mutation`]).
//! - [`replay`]: side-effect-free replay of a completed turn.
//! - [`relay`]: the live stream handed to the HTTP layer; owns the cancellation drop guard.
//! - [`provider_task`]: reads the provider stream, forwards events, finalizes the turn.
//! - [`finalize`]: the CAS finalization transaction (state, message, settlement, outbox).
//! - [`events`]: the SSE events.

pub mod events;
pub mod finalize;
mod knowledge;
pub mod provider_task;
pub mod relay;
pub mod replay;
pub mod setup;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub use events::{
    CitationDto, CitationSource, DeltaKind, DonePayload, EventStream, StreamEvent, TextSpan,
    ToolPhase, UsageDto,
};
pub use finalize::{FinalizeResult, TerminalOutcome, ToolCounts, finalize_turn};
pub use knowledge::KnowledgeParams;

use crate::config::MiniChatConfig;
use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::domain::model_service::ModelService;
use crate::domain::quota::{PreflightDecision, QuotaService};
use crate::infra::db::entity::{chat_turns, chats};
use crate::infra::gateways::policy::PolicyGateway;
use crate::infra::llm::{ChatTarget, LlmClient, ProviderResolver};
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::storage::knowledge::KnowledgeRetriever;
use crate::metrics::Metrics;

/// A user message to send (`POST /v1/chats/{id}/messages:stream`).
#[derive(Debug, Clone, Default)]
pub struct SendMessage {
    /// Stored as sent; must not be blank.
    pub content: String,
    /// Idempotency key; generated (v4) when `None`.
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
}

/// What replaces the latest turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// Re-submit the original user message.
    Retry,
    /// Submit this content instead (not blank).
    Edit(String),
}

/// A retry or edit that passed the turn service's preview (authorized chat, latest terminal turn
/// of the caller).
#[derive(Debug, Clone)]
pub struct MutationPlan {
    pub replacement: Replacement,
    /// Tenant scope of the authorized chat.
    pub scope: AccessScope,
    /// The turn being replaced.
    pub target: chat_turns::Model,
}

/// Context facts of a turn for the thread summary trigger
/// ([`crate::domain::thread_summary::evaluate_trigger`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTriggerInfo {
    /// Estimate of everything sent to the provider.
    pub assembled_tokens: i64,
    /// Input budget of the effective model.
    pub effective_budget: i64,
    /// At least one history message did not fit.
    pub messages_truncated: bool,
    /// The chat already has a thread summary.
    pub summary_exists: bool,
}

/// A started turn: everything the provider task and the finalization need.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub turn_id: Uuid,
    pub chat_id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub request_id: Uuid,
    pub user_message_id: Uuid,
    /// Pre-allocated id of the assistant message (sent in `stream_started`).
    pub assistant_message_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    pub decision: PreflightDecision,
    pub target: ChatTarget,
    /// Provider of the effective model.
    pub provider_id: String,
    pub web_search_enabled: bool,
    /// `provider_file_id -> (attachment_id, filename)` of the chat's ready attachments, filled
    /// when `file_search` is in the request (citation mapping).
    pub file_citation_map: HashMap<String, (Uuid, String)>,
    pub summary: SummaryTriggerInfo,
    /// Token estimate of the thread summary when it is part of the context
    /// (`stream_started.thread_summary_applied`).
    pub thread_summary_applied: Option<i64>,
    /// Knowledge search of the turn (`search_knowledge` offered), when active.
    pub knowledge: Option<KnowledgeParams>,
    pub started_at: Instant,
}

/// Services the pipeline uses; see [`StreamService::new`].
pub struct StreamDeps {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<Authz>,
    pub models: Arc<ModelService>,
    pub policy: Arc<dyn PolicyGateway>,
    pub quota: Arc<QuotaService>,
    pub providers: Arc<ProviderResolver>,
    pub llm: Arc<LlmClient>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub metrics: Arc<Metrics>,
    /// Knowledge retriever (only when `knowledge_search.enabled`).
    pub knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

/// The send pipeline. Cheap to clone (every field is shared): the provider task of each turn
/// holds a clone for its finalization.
#[derive(Clone)]
pub struct StreamService {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<Authz>,
    models: Arc<ModelService>,
    policy: Arc<dyn PolicyGateway>,
    quota: Arc<QuotaService>,
    providers: Arc<ProviderResolver>,
    llm: Arc<LlmClient>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<Metrics>,
    knowledge: Option<Arc<dyn KnowledgeRetriever>>,
}

impl StreamService {
    #[must_use]
    pub fn new(deps: StreamDeps) -> Self {
        let StreamDeps {
            cfg,
            db,
            authz,
            models,
            policy,
            quota,
            providers,
            llm,
            outbox,
            metrics,
            knowledge,
        } = deps;
        Self {
            cfg,
            db,
            authz,
            models,
            policy,
            quota,
            providers,
            llm,
            outbox,
            metrics,
            knowledge,
        }
    }

    /// Sends a user message to chat `chat_id`: replays a completed turn of the same
    /// `request_id`, or starts a new turn and returns its live event stream.
    ///
    /// Every rejection happens before a stream exists and is returned as an error. The caller
    /// should run this in a spawned task (DESIGN 1163): a turn committed by the setup is then
    /// streamed (and cancelled when the response is dropped) even if the client disconnected
    /// during the setup.
    ///
    /// # Errors
    /// See [`setup::prepare`].
    pub async fn send(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendMessage,
    ) -> Result<EventStream, DomainError> {
        match setup::prepare(self, ctx, chat_id, req).await? {
            setup::SendPlan::Replay(replay) => Ok(replay::events(replay)),
            setup::SendPlan::Live(live) => Ok(self.spawn(*live)),
        }
    }

    /// Replaces the latest turn of `chat` (retry or edit) and returns the new turn's live event
    /// stream, exactly like a send (same relay, finalization, settlement and outbox path).
    ///
    /// Like [`Self::send`], the caller should run this in a spawned task.
    ///
    /// # Errors
    /// See [`setup::prepare_mutation`].
    pub async fn run_mutation(
        &self,
        ctx: &SecurityContext,
        chat: chats::Model,
        plan: MutationPlan,
    ) -> Result<EventStream, DomainError> {
        let live = setup::prepare_mutation(self, ctx, chat, plan).await?;
        Ok(self.spawn(live))
    }

    fn spawn(&self, live: setup::LiveTurn) -> EventStream {
        let setup::LiveTurn {
            turn,
            adapter,
            request,
        } = live;
        relay::spawn_turn(self.clone(), turn, adapter, request)
    }
}

#[cfg(test)]
mod tests;
