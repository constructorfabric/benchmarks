//! Streaming core: `POST /v1/chats/{id}/messages:stream` (DESIGN 3.3
//! "Streaming Contract", 3.6 "Send Message with Streaming Response", 5.7 "Turn
//! Finalization Contract").
//!
//! Flow of one send, in the order the contract fixes:
//!
//! 1. PDP (`send_message` on the chat), owner-scoped chat lookup (404);
//! 2. idempotency on `(chat_id, request_id)`: a completed turn is replayed
//!    from storage, every other state is `request_id_conflict`;
//! 3. parallel-turn guard (`turn_already_running`);
//! 4. preflight ([`preflight`]): model still in the catalog, content, kill
//!    switch, quota (downgrade cascade -> 429 `tokens`, daily web search quota
//!    -> 429 `web_search`), input length, attachments and vision on the
//!    effective model, tools, context budget, context assembly, provider
//!    resolution;
//! 5. the reserve transaction ([`reserve`]): `chat_turns` row (`running`, with
//!    the reserve columns), the `quota_usage` reserve and its limit re-check,
//!    user message, `message_attachments`, `chats.updated_at`;
//! 6. the provider task ([`run`]) relays the provider stream through a bounded
//!    channel and finalizes the turn with the CAS of [`finalize`] (assistant
//!    message, quota settlement, usage and audit outbox messages, all in one
//!    transaction); the terminal SSE event is sent only after the
//!    finalization committed.
//!
//! A retry / edit (`domain::turn_mutations`) reuses steps 4-6 through
//! `mutation_preflight` (the read-only half of step 4, run before the mutation
//! commits) and `resume_mutation` (context assembly, provider resolution and
//! the reserve after the commit, then the same provider task and finalization):
//! there is no second settlement or outbox path.
//!
//! Every rejection of steps 1-4 (and the 429 of the limit re-check in step 5,
//! which rolls the transaction back) is a normal JSON error and leaves no row
//! behind. The setup (steps 1-5) runs in a spawned task whose result the
//! handler awaits, so a client that disconnects before the response head is
//! written cannot strand a committed turn: the provider task still starts and
//! sees the dropped receiver (DESIGN 3.3 "SSE stream close rules", 2).

mod finalize;
mod mutation;
mod preflight;
mod replay;
mod reserve;
mod run;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use authz_resolver_sdk::PolicyEnforcer;
use futures::Stream;
use tokio::sync::mpsc;
use toolkit_db::DBProvider;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use self::reserve::{Contention, Reserved, running_guard};
use crate::config::MiniChatConfig;
use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::chat_not_found;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    AuditPort, ChatRepository, LlmGateway, ModelPolicyPort, ThreadSummaryPort, TurnRepository,
    UsagePort,
};
use crate::domain::quota::QuotaWarning;

/// Streaming error code of a turn whose outcome was decided by someone else
/// (the orphan watchdog) or whose provider task died.
pub(crate) const STREAM_INTERRUPTED: &str = "stream_interrupted";

/// What the client asked for (`StreamMessageRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMessageCommand {
    /// Message text, stored verbatim.
    pub content: String,
    /// Idempotency key; generated (UUID v4) when absent.
    pub request_id: Option<Uuid>,
    /// Attachments to link to the user message.
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled`.
    pub web_search: bool,
}

/// Lifecycle phase of a tool event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    /// The tool started.
    Start,
    /// The tool finished.
    Done,
}

/// Source of a citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    /// An uploaded document.
    File,
    /// A web page.
    Web,
}

/// One item of the `citations` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// Where the citation comes from.
    pub source: CitationSource,
    /// Page title, or the stored file name.
    pub title: String,
    /// Page URL (web only).
    pub url: Option<String>,
    /// Internal attachment id (file only).
    pub attachment_id: Option<Uuid>,
    /// Excerpt (empty for file citations).
    pub snippet: String,
    /// Character range of the annotation, when the provider sent one.
    pub span: Option<(u64, u64)>,
}

/// `done.quota_decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    /// The turn ran on the selected model.
    Allow,
    /// The turn ran on another model.
    Downgrade,
}

/// Payload of the terminal `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneEvent {
    /// Provider reported input tokens.
    pub input_tokens: u64,
    /// Provider reported output tokens.
    pub output_tokens: u64,
    /// Model the turn ran on (catalog id).
    pub effective_model: String,
    /// `chats.model` (catalog id).
    pub selected_model: String,
    /// Allow or downgrade.
    pub quota_decision: QuotaDecision,
    /// The selected model, on a downgrade.
    pub downgrade_from: Option<String>,
    /// Why the turn was downgraded, when known (not stored, so never on a replay).
    pub downgrade_reason: Option<String>,
    /// Per-tier, per-period remaining quota after this turn's settlement
    /// (completed turn of the CAS winner only; never on a replay).
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// One event of the SSE contract, in domain terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// First event of every stream.
    Started {
        /// Resolved turn correlation key.
        request_id: Uuid,
        /// Pre-allocated (new turn) or persisted (replay) assistant message id.
        message_id: Uuid,
        /// `false` on a replay.
        is_new_turn: bool,
        /// `token_estimate` of the thread summary that is part of the context
        /// (never on a replay).
        thread_summary_applied: Option<u64>,
    },
    /// Keep-alive before the first content event.
    Ping,
    /// Incremental answer text.
    Delta(String),
    /// Tool activity.
    Tool {
        /// Phase.
        phase: ToolPhase,
        /// Tool name.
        name: String,
        /// Non-sensitive metadata.
        details: serde_json::Value,
    },
    /// Source references, at most once, right before `done`.
    Citations(Vec<Citation>),
    /// Terminal: the turn is committed as `completed`.
    Done(DoneEvent),
    /// Terminal: `{code, message}`.
    Error {
        /// Stable streaming error code.
        code: String,
        /// Sanitized human readable text.
        message: String,
    },
}

/// The event stream of one turn. Dropping it is the client disconnect: the
/// provider task notices the closed channel and cancels the turn.
pub struct TurnEvents {
    inner: Pin<Box<dyn Stream<Item = StreamEvent> + Send>>,
}

impl TurnEvents {
    /// Events relayed from a running turn through a bounded channel. The
    /// stream ends right after the terminal event; a provider task that ended
    /// without one (a panic) is reported as `stream_interrupted`.
    fn live(receiver: mpsc::Receiver<StreamEvent>) -> Self {
        Self {
            inner: Box::pin(futures::stream::unfold(
                (receiver, false),
                |(mut receiver, finished)| async move {
                    if finished {
                        return None;
                    }
                    let event = receiver.recv().await.unwrap_or_else(|| StreamEvent::Error {
                        code: STREAM_INTERRUPTED.to_owned(),
                        message: "the turn ended without a result".to_owned(),
                    });
                    let terminal =
                        matches!(event, StreamEvent::Done(_) | StreamEvent::Error { .. });
                    Some((event, (receiver, terminal)))
                },
            )),
        }
    }

    /// A finite, already known sequence (replay).
    fn fixed(events: Vec<StreamEvent>) -> Self {
        Self {
            inner: Box::pin(futures::stream::iter(events)),
        }
    }
}

impl Stream for TurnEvents {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// The scopes of one send: the PDP scope narrowed to the owner (the chat), the
/// tenant scope of the chat children and the tenant + owner scope of the
/// caller's `quota_usage` rows.
#[derive(Clone)]
pub(crate) struct TurnScopes {
    pub(crate) owner: AccessScope,
    pub(crate) tenant: AccessScope,
    pub(crate) quota: AccessScope,
}

impl TurnScopes {
    /// The scopes of one turn from the PDP scope of the chat action.
    pub(crate) fn of(scope: &AccessScope) -> Self {
        Self {
            owner: scope.clone(),
            tenant: scope.tenant_only(),
            quota: scope.tenant_and_owner(),
        }
    }
}

/// What a retry / edit asks of the stream service: the user message to send
/// again and the turn it replaces.
pub(crate) struct MutationRequest {
    /// The original content (retry) or the new content (edit), verbatim.
    pub(crate) content: String,
    /// `web_search_enabled` of the replaced turn.
    pub(crate) web_search: bool,
    /// The non-deleted attachments of the replaced user message.
    pub(crate) attachment_ids: Vec<Uuid>,
    /// `request_id` of the replaced turn.
    pub(crate) replaced_request: Uuid,
}

/// The decisions of the mutation preflight (quota, kill switches, guards),
/// carried across the mutation commit.
pub(crate) struct MutationPlan {
    gate: preflight::PreflightGate,
    command: SendMessageCommand,
}

/// The new turn the mutation commit inserted (`running`, no reserve yet).
pub(crate) struct StartedTurn {
    /// `chat_turns.id`.
    pub(crate) turn_id: Uuid,
    /// The server generated `request_id`.
    pub(crate) request_id: Uuid,
    /// `created_at` of the new user message.
    pub(crate) user_created_at: time::OffsetDateTime,
}

/// Sends messages and streams the answer.
pub struct StreamService {
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyPort>,
    llm: Arc<dyn LlmGateway>,
    config: Arc<MiniChatConfig>,
    enforcer: PolicyEnforcer,
    usage: Option<Arc<dyn UsagePort>>,
    audit: Option<Arc<dyn AuditPort>>,
    thread_summary: Option<Arc<dyn ThreadSummaryPort>>,
}

impl StreamService {
    /// Creates the service.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static, L: LlmGateway + 'static>(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        llm: Arc<L>,
        config: Arc<MiniChatConfig>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            db,
            policy,
            llm,
            config,
            enforcer,
            usage: None,
            audit: None,
            thread_summary: None,
        }
    }

    /// Wires the usage and audit outbox ports of the finalization: every
    /// settled turn then enqueues its usage and audit message in the
    /// finalization transaction. Without them nothing is enqueued.
    #[must_use]
    pub fn with_outbox<U: UsagePort + 'static, A: AuditPort + 'static>(
        mut self,
        usage: Arc<U>,
        audit: Arc<A>,
    ) -> Self {
        self.usage = Some(usage);
        self.audit = Some(audit);
        self
    }

    /// Enables the thread-summary trigger: a completed turn whose context
    /// reached the threshold or was truncated then enqueues one thread-summary
    /// work item in its finalization transaction (when
    /// `thread_summary_worker.enabled`). Without it nothing is enqueued.
    #[must_use]
    pub fn with_thread_summary<T: ThreadSummaryPort + 'static>(mut self, port: Arc<T>) -> Self {
        self.thread_summary = Some(port);
        self
    }

    /// Starts (or replays) a turn and returns its event stream.
    ///
    /// The setup runs in a spawned task and is awaited here: it completes
    /// even when the caller is dropped mid-way, and the turn it committed is
    /// then cancelled by the provider task.
    ///
    /// # Errors
    /// Every error is raised before any event exists and maps to a JSON
    /// problem: `AccessDenied` / `ServiceUnavailable` (PDP), `NotFound` (chat),
    /// `RequestIdConflict`, `TurnAlreadyRunning`, `InvalidModel`,
    /// `EmptyContent`, `FeatureDisabled`, `OutOfRange` (`INPUT_TOO_LONG`,
    /// `CONTEXT_BUDGET_EXCEEDED`), `InvalidAttachment`, `Internal`.
    pub async fn send_message(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        command: SendMessageCommand,
    ) -> Result<TurnEvents, DomainError> {
        let service = Arc::clone(self);
        tokio::spawn(async move { service.start_turn(&ctx, chat_id, command).await })
            .await
            .map_err(|error| DomainError::internal(format!("stream setup task failed: {error}")))?
    }

    async fn start_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        command: SendMessageCommand,
    ) -> Result<TurnEvents, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::SEND_MESSAGE, Some(chat_id)).await?;
        // The chat is looked up with the owner-narrowed scope; its children
        // (turns, messages, attachments) are then read with the tenant scope,
        // the caller's quota rows with the tenant + owner scope.
        let scopes = TurnScopes::of(&scope);
        let tenant_scope = scopes.tenant.clone();
        let conn = self.db.conn()?;
        let chat = conn
            .find_live_chat(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let request_id = command.request_id.unwrap_or_else(Uuid::new_v4);

        // Idempotency first, then the parallel-turn guard (DESIGN "Check
        // Priority Order"): a completed key is replayed while another turn runs.
        if let Some(turn) = conn.find_turn(&tenant_scope, chat_id, request_id).await? {
            return self.replay(&conn, &tenant_scope, &chat, &turn).await;
        }
        match running_guard(&conn, &tenant_scope, chat_id, request_id).await? {
            None => {}
            Some(Contention::SameKey(turn)) => {
                return self.replay(&conn, &tenant_scope, &chat, &turn).await;
            }
            Some(Contention::Parallel) => return Err(DomainError::TurnAlreadyRunning),
        }

        let plan = self.preflight(&conn, ctx, &scopes, &chat, &command).await?;
        let admitted = match self
            .reserve(ctx, &scopes, &chat, request_id, &command, plan)
            .await?
        {
            Reserved::Admitted(admitted) => *admitted,
            Reserved::Existing(turn) => {
                return self.replay(&conn, &tenant_scope, &chat, &turn).await;
            }
        };
        Ok(self.launch(ctx, scopes, admitted))
    }

    /// The read-only half of a retry / edit preflight: the quota cascade, the
    /// kill switches and the guards of the (carried-forward) message. A
    /// rejection is a JSON error and nothing was written.
    ///
    /// # Errors
    /// `InvalidModel`, `EmptyContent`, `FeatureDisabled`, `QuotaExceeded`,
    /// `OutOfRange` (`INPUT_TOO_LONG`), `InvalidAttachment`, `InvalidInput`
    /// (`VISION_NOT_SUPPORTED`), `Internal`.
    pub(crate) async fn mutation_preflight(
        &self,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &crate::domain::chats::ChatRow,
        request: MutationRequest,
    ) -> Result<MutationPlan, DomainError> {
        self.mutation_gate(ctx, scopes, chat, request).await
    }

    /// The second half of a retry / edit, after the mutation committed:
    /// context assembly, provider resolution, the quota reserve with its limit
    /// re-check, then the same provider task, finalization and settlement as a
    /// send. A failure up to the reserve marks the new turn `failed` and is
    /// returned as the JSON error (DESIGN 3.9 rule 8).
    ///
    /// # Errors
    /// `OutOfRange` (`CONTEXT_BUDGET_EXCEEDED`), `QuotaExceeded`, `Internal`.
    pub(crate) async fn resume_mutation(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        scopes: TurnScopes,
        chat: crate::domain::chats::ChatRow,
        started: StartedTurn,
        plan: MutationPlan,
    ) -> Result<TurnEvents, DomainError> {
        self.resume_started_turn(ctx, scopes, chat, started, plan)
            .await
    }
}
