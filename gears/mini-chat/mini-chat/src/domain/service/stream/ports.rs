//! Ports of the stream pipeline: the turn store, the provider client and its
//! event stream, the provider directory and the metrics sink. The
//! infrastructure layer implements them; the domain never sees `SeaORM`, the
//! OAGW SDK or HTTP types.

use std::collections::HashMap;

use async_trait::async_trait;
use mini_chat_sdk::{TurnAuditEvent, UsageTokens};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::types::{FunctionCall, ProviderEvent, ProviderFailure, TurnRecord, TurnState};
use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::quota::reserve::{ReserveRequest, ReservedColumns};
use crate::domain::service::workers::summary_trigger::SummaryTrigger;

// ---- rows ---------------------------------------------------------------------------------------

/// `document` or `image`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// A document.
    Document,
    /// An image.
    Image,
}

/// A live attachment of the chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInfo {
    /// Attachment id.
    pub id: Uuid,
    /// Kind.
    pub kind: AttachmentKind,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// File name.
    pub filename: String,
    /// Provider file id of the primary copy.
    pub provider_file_id: Option<String>,
    /// Provider file id of the secondary copy (Anthropic images).
    pub secondary_file_id: Option<String>,
    /// `for_file_search`.
    pub for_file_search: bool,
    /// `for_code_interpreter`.
    pub for_code_interpreter: bool,
    /// Creation time (ordering of the code-interpreter file ids).
    pub created_at: OffsetDateTime,
}

/// `(created_at, id)` position of a message.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageBoundary {
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Message id.
    pub id: Uuid,
}

/// The thread summary of the chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryInfo {
    /// Summary text.
    pub summary_text: String,
    /// Frontier `(summarized_up_to_created_at, summarized_up_to_message_id)`.
    pub frontier: MessageBoundary,
    /// `token_estimate`.
    pub token_estimate: i32,
}

/// `user` or `assistant`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    /// A user message.
    User,
    /// An assistant message.
    Assistant,
}

/// A message of the recent history.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Message id.
    pub id: Uuid,
    /// Role.
    pub role: HistoryRole,
    /// Content.
    pub content: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// The recent-messages query of `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`
/// (step 9): messages with a request id, not deleted, not compressed, role
/// `user` or `assistant`, at most `boundary`, after `frontier` when a summary
/// exists; the newest `limit` rows, returned in chronological order.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecentQuery {
    /// Snapshot boundary.
    pub boundary: MessageBoundary,
    /// Summary frontier.
    pub frontier: Option<MessageBoundary>,
    /// `context.recent_messages_limit` (greater than 0).
    pub limit: u32,
}

/// The persisted assistant message of a completed turn (replay).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAssistantMessage {
    /// Message id.
    pub id: Uuid,
    /// Full content (possibly empty).
    pub content: String,
    /// Stored input tokens.
    pub input_tokens: u64,
    /// Stored output tokens.
    pub output_tokens: u64,
}

// ---- writes -------------------------------------------------------------------------------------

/// Everything the reserve transaction writes besides the quota rows
/// (`cpt-cf-mini-chat-algo-streaming-turns-create-turn`).
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct CreateTurnInput<'a> {
    /// Quota reserve (tenant, user, `Send` mode, the decision, `now`).
    pub reserve: ReserveRequest<'a>,
    /// Chat.
    pub chat_id: Uuid,
    /// Idempotency key.
    pub request_id: Uuid,
    /// Pre-generated turn id.
    pub turn_id: Uuid,
    /// Pre-generated user message id.
    pub user_message_id: Uuid,
    /// Message content, as sent.
    pub content: &'a str,
    /// Distinct attachment ids.
    pub attachment_ids: &'a [Uuid],
    /// `web_search.enabled` of the request.
    pub web_search_enabled: bool,
}

/// The assistant message INSERT of a finalization.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessageWrite {
    /// Pre-allocated message id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Request id of the turn.
    pub request_id: Uuid,
    /// Full accumulated text (possibly empty).
    pub content: String,
    /// Provider response id.
    pub provider_response_id: Option<String>,
    /// Usage of the turn (zeros stored when absent).
    pub usage: Option<UsageTokens>,
    /// Effective model.
    pub model: String,
    /// The later of the finalization time and the user message time plus 1 µs.
    pub created_at: OffsetDateTime,
}

/// The caller's writes of a finalization transaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeWrites {
    /// The assistant message, when one is persisted.
    pub assistant_message: Option<AssistantMessageWrite>,
    /// The turn audit event enqueued in the same transaction.
    pub audit: TurnAuditEvent,
    /// The thread-summary trigger evaluated in the same transaction after the
    /// audit enqueue; `None` evaluates nothing.
    pub summary: Option<SummaryTrigger>,
}

/// Why the finalization transaction failed.
#[domain_model]
#[derive(Debug)]
pub enum FinalizeError {
    /// The assistant message INSERT failed (the transaction rolled back).
    MessageInsert(DomainError),
    /// Anything else (the transaction rolled back, the turn stays `running`).
    Other(DomainError),
}

/// Counter updates of a running turn (single autocommit statement
/// `WHERE state = 'running'`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressUpdate {
    /// New `last_progress_at`.
    pub at: Option<OffsetDateTime>,
    /// New `web_search_completed_count`.
    pub web_search_done: Option<u32>,
    /// New `code_interpreter_completed_count`.
    pub code_interpreter_done: Option<u32>,
    /// New `file_search_completed_count`.
    pub file_search_done: Option<u32>,
}

// ---- the turn store -----------------------------------------------------------------------------

/// Storage the stream pipeline reads and writes. Reads take the compiled
/// `AccessScope` of the PEP decision and are always bound to the chat.
#[async_trait]
pub trait TurnRepository: Send + Sync {
    /// The turn with `(chat_id, request_id)`, soft-deleted rows included.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn find_by_request_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError>;

    /// A `running`, non-deleted turn of the chat (any request id).
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn find_running(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError>;

    /// The assistant message `message_id` of the chat, when live.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn load_assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError>;

    /// Live attachments of the chat among `ids` (any status).
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn request_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<AttachmentInfo>, DomainError>;

    /// Live `ready` attachments of the chat.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<AttachmentInfo>, DomainError>;

    /// The non-NULL `vector_store_id` of the chat's `chat_vector_stores` row.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError>;

    /// The latest live message with a request id.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn snapshot_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError>;

    /// The thread summary of the chat.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn thread_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryInfo>, DomainError>;

    /// The recent messages, chronological.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn recent_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &RecentQuery,
    ) -> Result<Vec<HistoryMessage>, DomainError>;

    /// The reserve transaction: quota reserve, user message, chat bump,
    /// in-transaction attachment check, `message_attachments`, the `running`
    /// turn.
    ///
    /// # Errors
    ///
    /// `QuotaExceeded(Tokens)` at the re-check, `InvalidAttachmentIds`,
    /// `UniqueViolation` for any unique constraint of either engine (never
    /// retried), `Database` / `Internal` otherwise. Nothing is left behind on
    /// an error.
    async fn create_turn(
        &self,
        input: &CreateTurnInput<'_>,
    ) -> Result<ReservedColumns, DomainError>;

    /// The finalization transaction: CAS, settlement, usage event, the
    /// assistant message and the audit event.
    ///
    /// # Errors
    ///
    /// [`FinalizeError`]; the transaction is rolled back.
    async fn finalize(
        &self,
        request: &FinalizeRequest,
        writes: &FinalizeWrites,
    ) -> Result<FinalizeResult, FinalizeError>;

    /// The state of a turn: tenant-scoped and bound to its chat (liveness
    /// check of the provider task).
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn turn_state(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<TurnState>, DomainError>;

    /// Progress write `WHERE state = 'running'`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn record_progress(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
        update: &ProgressUpdate,
    ) -> Result<(), DomainError>;
}

// ---- the provider -------------------------------------------------------------------------------

/// Where a provider request goes.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTarget {
    /// Adapter kind.
    pub kind: ProviderKind,
    /// OAGW alias.
    pub alias: String,
    /// Chat endpoint path with the `{model}` placeholder and any query.
    pub api_path: String,
    /// Provider `api_version`, when configured.
    pub api_version: Option<String>,
}

/// Maps a catalog `provider_id` and tenant to the provider target
/// (`cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`).
pub trait ProviderDirectory: Send + Sync {
    /// Resolves the provider.
    ///
    /// # Errors
    ///
    /// [`DomainError::ProviderResolution`] when the provider is not configured.
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError>;
}

/// A built provider request, independent of the transport.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltRequest {
    /// Adapter kind (selects the translator of the response).
    pub kind: ProviderKind,
    /// `/{alias}{api_path}` with `{model}` replaced, query kept.
    pub uri: String,
    /// Headers (never a credential).
    pub headers: Vec<(String, String)>,
    /// JSON body.
    pub body: Value,
}

/// A stream of translated provider events. `next_event` is cancel-safe: the
/// task races it against the cancellation token and the liveness timer, and
/// dropping the stream drops the response body (which closes the upstream
/// connection).
#[async_trait]
pub trait ProviderStream: Send {
    /// The next internal event; `None` at the end of the stream; `Err` ends
    /// the stream with a classified failure.
    async fn next_event(&mut self) -> Option<Result<ProviderEvent, ProviderFailure>>;
}

/// The provider adapters behind OAGW
/// (`cpt-cf-mini-chat-component-llm-provider`).
#[async_trait]
pub trait ProviderClient: Send + Sync {
    /// Builds the request of the adapter of `input.target.kind`
    /// (`cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] (an unsupported kind, a body that cannot be
    /// serialized).
    fn build_request(
        &self,
        input: &super::request::ProviderRequestInput<'_>,
    ) -> Result<BuiltRequest, DomainError>;

    /// Builds the next request of the knowledge-search loop: `previous` with
    /// the function calls of the iteration and their `outputs` appended to the
    /// provider input, in the format of the adapter of `previous.kind`
    /// (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`,
    /// `inst-append-output`). Tools and every other key stay as they were.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] (an unsupported kind, a body without the
    /// input array of its format).
    fn continue_request(
        &self,
        previous: &BuiltRequest,
        outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError>;

    /// Sends the request through OAGW and returns the event stream.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderFailure`] (`proxy_request` failure, non-2xx
    /// status, a response that is not `text/event-stream`).
    async fn open(
        &self,
        ctx: &SecurityContext,
        request: &BuiltRequest,
    ) -> Result<Box<dyn ProviderStream>, ProviderFailure>;
}

/// One function call of a `tool_use` outcome and the output the gear answers
/// it with.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallOutput {
    /// The call, as the provider issued it.
    pub call: FunctionCall,
    /// The function output sent back to the model (a JSON text).
    pub output: String,
}

// ---- knowledge retrieval ------------------------------------------------------------------------

/// One knowledge retrieval.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnowledgeSearchRequest<'a> {
    /// Tenant of the turn (selects the OAGW alias of the RAG provider).
    pub tenant_id: Uuid,
    /// The query the model supplied.
    pub query: &'a str,
    /// The number of chunks to return, already capped at
    /// `knowledge_search.top_k`.
    pub top_k: usize,
}

/// One retrieved chunk of the knowledge base.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeChunk {
    /// The chunk text (not yet trimmed to `knowledge_search.max_chunk_chars`).
    pub text: String,
}

/// The knowledge retriever
/// (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`, `inst-retrieve`):
/// one vector store search of the RAG provider through OAGW.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// Retrieves the chunks for `request`, best first. The call is cancel-safe:
    /// the provider task drops it when the client disconnects.
    ///
    /// # Errors
    ///
    /// Any [`DomainError`]; the loop answers the call with a failure output and
    /// continues.
    async fn search(
        &self,
        ctx: &SecurityContext,
        request: &KnowledgeSearchRequest<'_>,
    ) -> Result<Vec<KnowledgeChunk>, DomainError>;
}

// ---- metrics ------------------------------------------------------------------------------------

/// `before_first_token` or `mid_stream`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectStage {
    /// No `delta` was relayed yet.
    BeforeFirstToken,
    /// After the first `delta`.
    MidStream,
}

impl DisconnectStage {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeFirstToken => "before_first_token",
            Self::MidStream => "mid_stream",
        }
    }
}

/// The request group of the cost metrics
/// (`cpt-cf-mini-chat-algo-streaming-turns-record-metrics`). Only the labels
/// `provider`, `model`, `error_code`, `reason`, `stage`, `trigger` and
/// `result` exist; `trigger` is always `disconnect`.
pub trait StreamMetrics: Send + Sync {
    /// `mini_chat_stream_started{provider,model}`.
    fn stream_started(&self, provider: &str, model: &str);
    /// `mini_chat_active_streams` (up-down counter).
    fn active_streams_add(&self, delta: i64);
    /// `mini_chat_ttft_provider_ms{provider,model}`.
    fn ttft_provider_ms(&self, provider: &str, model: &str, millis: f64);
    /// `mini_chat_ttft_overhead_ms{provider,model}`.
    fn ttft_overhead_ms(&self, provider: &str, model: &str, millis: f64);
    /// `mini_chat_stream_completed{provider,model}`.
    fn stream_completed(&self, provider: &str, model: &str);
    /// `mini_chat_stream_incomplete{provider,model,reason}`.
    fn stream_incomplete(&self, provider: &str, model: &str, reason: &str);
    /// `mini_chat_stream_failed{provider,model,error_code}`.
    fn stream_failed(&self, provider: &str, model: &str, error_code: &str);
    /// `mini_chat_stream_total_latency_ms{provider,model}`.
    fn stream_total_latency_ms(&self, provider: &str, model: &str, millis: f64);
    /// `mini_chat_cancel_requested{trigger="disconnect"}`.
    fn cancel_requested(&self);
    /// `mini_chat_stream_disconnected{stage}`.
    fn stream_disconnected(&self, stage: DisconnectStage);
    /// `mini_chat_time_to_abort_ms{trigger="disconnect"}`.
    fn time_to_abort_ms(&self, millis: f64);
    /// `mini_chat_cancel_effective{trigger="disconnect"}`.
    fn cancel_effective(&self);
    /// `mini_chat_streams_aborted{trigger="client_disconnect"}`.
    fn streams_aborted(&self);
    /// `mini_chat_image_inputs_per_turn`.
    fn image_inputs_per_turn(&self, count: u32);
    /// `mini_chat_finalization_latency_ms`.
    fn finalization_latency_ms(&self, millis: f64);
    /// `mini_chat_knowledge_search{result}`.
    fn knowledge_search(&self, result: &str);
    /// `mini_chat_knowledge_search_latency_ms`.
    fn knowledge_search_latency_ms(&self, millis: f64);
    /// `mini_chat_knowledge_search_chunks`.
    fn knowledge_search_chunks(&self, chunks: u64);
    /// `mini_chat_thread_summary_trigger{result}` (`scheduled` or `not_needed`),
    /// recorded after the finalization commit
    /// (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`,
    /// `inst-return-trigger-result`). A sink that does not count it ignores it.
    fn thread_summary_trigger(&self, _result: &str) {}
}

/// Provider file id to `(attachment id, filename)`, the citation map of a turn.
pub type CitationMap = HashMap<String, CitationTarget>;

/// What a file annotation resolves to.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationTarget {
    /// Internal attachment id.
    pub attachment_id: Uuid,
    /// File name (the citation title).
    pub filename: String,
}
