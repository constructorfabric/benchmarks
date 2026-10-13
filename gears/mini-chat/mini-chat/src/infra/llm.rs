//! `llm_provider` library (ADR-0001/0005): provider resolution, adapter-neutral request and
//! event types, and the client ports used by the domain services.
//!
//! Ownership: the types and traits in this file are the contract between the domain layer
//! (stream / attachment / summary services) and the provider adapters in the submodules.

pub mod client;
pub mod resolver;
pub mod sanitize;

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use resolver::{ProviderResolver, ResolvedProvider};

/// Role of an input message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRole {
    User,
    Assistant,
    System,
}

impl InputRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }
}

/// One content part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    /// Image referenced by provider file id (never exposed to clients).
    Image { file_id: String },
}

/// One input message of the assembled context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: InputRole,
    pub content: Vec<ContentPart>,
}

impl InputMessage {
    #[must_use]
    pub fn text(role: InputRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text(text.into())],
        }
    }
}

/// Built-in / function tools of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    FileSearch {
        vector_store_ids: Vec<String>,
        max_num_results: u32,
    },
    WebSearch {
        search_context_size: String,
    },
    CodeInterpreter {
        file_ids: Vec<String>,
    },
    /// Client-side function tool (e.g. `search_knowledge`); `parameters` is a JSON schema.
    Function {
        name: String,
        description: String,
        parameters: Value,
    },
}

impl ToolSpec {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
            Self::Function { name, .. } => name,
        }
    }
}

/// A function call issued by the model in an earlier agentic iteration and its output,
/// appended after the input messages of the next provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExchange {
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the model.
    pub arguments: String,
    /// Tool output returned to the model (`function_call_output`).
    pub output: String,
}

/// `request_type` of the provider `metadata` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    Chat,
    Summary,
}

/// Adapter-neutral LLM request.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// `providers.<id>` key serving the model.
    pub provider_id: String,
    /// Tenant used for `tenant_overrides` resolution.
    pub tenant_id: Uuid,
    /// Provider-side model name (`provider_model_id`).
    pub model: String,
    /// System prompt + tool guards.
    pub instructions: String,
    pub input: Vec<InputMessage>,
    /// Function calls of earlier agentic iterations with their outputs (sent after `input`).
    pub tool_exchanges: Vec<ToolExchange>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    /// `max_tool_calls` (OpenAI Responses adapter only; sent when tools are present).
    pub max_tool_calls: Option<u32>,
    pub api_params: ModelApiParams,
    /// `{tenant_hex}{user_hex}` (64 chars).
    pub user: String,
    /// Provider `metadata` object (OpenAI Responses adapter only).
    pub metadata: BTreeMap<String, String>,
    pub request_type: RequestType,
    pub stream: bool,
}

/// Raw citation extracted by the adapter from provider annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u64, u64)>,
    },
    /// Provider file citation; the domain maps `file_id` to an attachment.
    File {
        file_id: String,
        filename: Option<String>,
        span: Option<(u64, u64)>,
    },
}

/// Terminal success data (`response.completed` / `response.incomplete`).
#[derive(Debug, Clone, Default)]
pub struct LlmCompletion {
    pub response_id: Option<String>,
    /// `None` when the provider reported no usage object.
    pub usage: Option<UsageTokens>,
    pub citations: Vec<RawCitation>,
    /// Full output text as reported in the terminal response (may be empty).
    pub output_text: String,
    /// `Some(reason)` for `response.incomplete`.
    pub incomplete_reason: Option<String>,
}

/// Terminal failure of a provider call.
#[derive(Debug, Clone)]
pub struct LlmFailure {
    /// Streaming error code: `provider_error`, `provider_timeout` or `rate_limited`.
    pub code: &'static str,
    /// Sanitized, client-safe message.
    pub message: String,
    /// Usage reported with the failure, if any.
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
    /// True when the provider reported a context-length-exceeded error.
    pub context_length_exceeded: bool,
}

/// Internal streaming event produced by an adapter.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart { name: String, details: Value },
    ToolDone { name: String, details: Value },
    /// The model requested a client-side function call. Emitted before the terminal event
    /// of the provider request that requested it.
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Completed(LlmCompletion),
    Failed(LlmFailure),
}

/// Result of a non-streaming call (thread summary).
#[derive(Debug, Clone, Default)]
pub struct LlmTextResult {
    pub text: String,
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
}

/// Chat/summary client port. The stream ends after exactly one terminal event
/// (`Completed` or `Failed`); a transport end without terminal event is reported by the
/// adapter as `Failed { code: provider_error }`. When `cancel` fires the adapter drops the
/// upstream connection and ends the stream without a terminal event.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Opens a streaming call.
    ///
    /// # Errors
    /// Pre-stream failure (provider resolution, HTTP error status, gateway failure).
    async fn stream(
        &self,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, LlmFailure>;

    /// Non-streaming call (thread summary).
    ///
    /// # Errors
    /// Provider failure.
    async fn complete(&self, req: LlmRequest) -> Result<LlmTextResult, LlmFailure>;
}

/// One chunk returned by the knowledge retriever.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub text: String,
    pub filename: Option<String>,
    pub score: Option<f64>,
}

/// Knowledge search port (DESIGN §4 "Knowledge Search"): searches the organization-level
/// vector store configured in `knowledge_search`.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// Searches `vector_store_id` through the provider entry `provider_id` for `tenant_id`.
    ///
    /// # Errors
    /// Provider / transport failure or misconfiguration.
    async fn search(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError>;
}

/// Error of a storage (Files / Vector Stores API) operation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StorageError {
    /// Provider answered with a non-success HTTP status.
    #[error("provider returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    /// Gateway / transport failure (transient).
    #[error("transport error: {0}")]
    Transport(String),
    /// Misconfiguration (unknown provider, no storage kind, ...).
    #[error("storage misconfigured: {0}")]
    Config(String),
}

impl StorageError {
    /// 404 from the provider (idempotent delete success).
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::Http { status: 404, .. })
    }

    /// 5xx / transport failures that keep a poll loop going.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(_)) || matches!(self, Self::Http { status, .. } if *status >= 500)
    }
}

/// Indexing status of a vector store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorFileStatus {
    InProgress,
    Completed,
    /// `failed`, `cancelled` or an unknown value.
    Failed(String),
}

/// Files and Vector Stores API port. `provider_id` is the storage-capable provider entry
/// (the chat provider's `rag_provider`, or the provider itself).
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Uploads a file (`purpose=assistants`); returns the provider file id.
    ///
    /// # Errors
    /// Provider / transport failure.
    async fn upload_file(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        filename: &str,
        content_type: &str,
        data: bytes::Bytes,
    ) -> Result<String, StorageError>;

    /// Deletes a provider file (404 is returned as `Http{404}`; callers treat it as success).
    ///
    /// # Errors
    /// Provider / transport failure.
    async fn delete_file(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        file_id: &str,
    ) -> Result<(), StorageError>;

    /// Creates a vector store; returns its id.
    ///
    /// # Errors
    /// Provider / transport failure.
    async fn create_vector_store(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<String, StorageError>;

    /// Adds a file to a vector store with the given attributes; returns the indexing status.
    ///
    /// # Errors
    /// Provider / transport failure.
    async fn add_file_to_vector_store(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
        file_id: &str,
        attributes: BTreeMap<String, String>,
    ) -> Result<VectorFileStatus, StorageError>;

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// Provider / transport failure.
    async fn get_vector_store_file_status(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<VectorFileStatus, StorageError>;

    /// Deletes a vector store.
    ///
    /// # Errors
    /// Provider / transport failure.
    async fn delete_vector_store(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
    ) -> Result<(), StorageError>;
}
