//! `llm_provider`: in-process provider adapters, file/vector-store dispatch,
//! Anthropic files client, knowledge retriever (ADR-0001, ADR-0005).

pub mod knowledge;
pub mod providers;
pub mod resolver;
pub mod storage;

use mini_chat_sdk::{ApiParams, UsageTokens, WebSearchContextSize};
use serde_json::Value;

use crate::api::rest::dto::CitationSource;

/// Errors of the LLM provider layer.
#[derive(Debug, thiserror::Error)]
pub enum LlmProviderError {
    /// The provider returned an HTTP error (message sanitized).
    #[error("provider error: {message}")]
    Provider {
        /// HTTP status.
        status: Option<u16>,
        /// Sanitized message.
        message: String,
        /// Numeric `Retry-After` when sent.
        retry_after: Option<u64>,
        /// Token usage reported with the error (rare).
        usage: Option<UsageTokens>,
        /// Provider response id when known.
        provider_response_id: Option<String>,
    },
    /// Provider throttling (429).
    #[error("rate limited")]
    RateLimited {
        /// Numeric `Retry-After` when sent.
        retry_after: Option<u64>,
    },
    /// Provider request timed out (gateway timeout or 504).
    #[error("provider timeout")]
    Timeout,
    /// Gateway or transport failure.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// The provider response could not be parsed.
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    /// The request was cancelled (client disconnect).
    #[error("cancelled")]
    Cancelled,
}

/// A raw citation extracted from provider annotations.
#[derive(Debug, Clone, PartialEq)]
pub struct RawCitation {
    /// File or web.
    pub source: CitationSource,
    /// Provider file id (file citations).
    pub provider_file_id: Option<String>,
    /// Provider-reported filename (file citations).
    pub filename: Option<String>,
    /// URL (web citations).
    pub url: Option<String>,
    /// Page title (web citations).
    pub title: Option<String>,
    /// Character span in the carrying `output_text` part.
    pub span: Option<(u32, u32)>,
    /// Snippet text (web citations: annotation text or ranged slice).
    pub snippet: Option<String>,
}

/// Internal provider events produced by the adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// Incremental text.
    TextDelta(String),
    /// Incremental reasoning (`vLLM` `<think>` blocks).
    ReasoningDelta(String),
    /// Tool call started.
    ToolStart {
        /// Tool name (`file_search`, `web_search`, `code_interpreter`,
        /// `function_call`, `search_knowledge`, ...).
        name: String,
        /// Details.
        details: Value,
    },
    /// Tool call finished.
    ToolDone {
        /// Tool name.
        name: String,
        /// Details.
        details: Value,
    },
    /// Citations extracted from the response annotations.
    Citations(Vec<RawCitation>),
    /// Terminal success (complete or incomplete).
    Completed {
        /// Provider-reported usage (absent = unknown).
        usage: Option<UsageTokens>,
        /// Provider response id.
        provider_response_id: Option<String>,
        /// Incomplete reason (normalized), when the response is incomplete.
        incomplete_reason: Option<String>,
    },
    /// Terminal success carrying extracted citations.
    CompletedWithCitations {
        /// Provider-reported usage (absent = unknown).
        usage: Option<UsageTokens>,
        /// Provider response id.
        provider_response_id: Option<String>,
        /// Incomplete reason (normalized), when the response is incomplete.
        incomplete_reason: Option<String>,
        /// Citations extracted from the response.
        citations: Vec<RawCitation>,
    },
    /// Terminal failure reported by the provider.
    Failed {
        /// Internal error code (`provider_error`, ...).
        code: String,
        /// Sanitized message.
        message: String,
        /// `Usage` when reported.
        usage: Option<UsageTokens>,
        /// Provider response id when known.
        provider_response_id: Option<String>,
    },
    /// The model requested a function tool the turn must dispatch
    /// (`search_knowledge` agentic loop).
    FunctionCall {
        /// Tool name.
        name: String,
        /// Call id.
        call_id: String,
        /// Raw JSON arguments.
        arguments: String,
    },
}

/// A chat message in the provider request input.
#[derive(Debug, Clone, PartialEq)]
pub struct InputMessage {
    /// `user` | `assistant`.
    pub role: String,
    /// Text content.
    pub text: String,
    /// Image file ids (user messages only).
    pub images: Vec<String>,
}

/// Tool definitions for a provider request.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolSpec {
    /// `file_search` over chat vector stores.
    FileSearch {
        /// Vector store ids.
        vector_store_ids: Vec<String>,
        /// Top-k results per call.
        max_num_results: u32,
    },
    /// `web_search`.
    WebSearch {
        /// Search context size hint.
        search_context_size: WebSearchContextSize,
    },
    /// `code_interpreter` with file inputs.
    CodeInterpreter {
        /// Provider file ids.
        file_ids: Vec<String>,
    },
    /// A client-side function tool (`search_knowledge`).
    Function {
        /// Tool name.
        name: String,
        /// Description.
        description: String,
        /// JSON Schema parameters.
        parameters: Value,
    },
}

/// A provider chat request (adapter-agnostic).
#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// Provider-side model name.
    pub provider_model_id: String,
    /// System instructions.
    pub instructions: String,
    /// Conversation input (chronological; the last entry is the current
    /// user message).
    pub input: Vec<InputMessage>,
    /// Raw trailing input items (`function_call` / `function_call_output`)
    /// appended by the knowledge-search agentic loop.
    pub trailing_items: Vec<serde_json::Value>,
    /// Tools.
    pub tools: Vec<ToolSpec>,
    /// Hard output cap.
    pub max_output_tokens: u32,
    /// Built-in tool call bound (sent only by the `OpenAI` `Responses`
    /// adapter).
    pub max_tool_calls: u32,
    /// Composite `{tenant_hex}{user_hex}` identity.
    pub user: String,
    /// Request metadata (sent only by the `OpenAI` `Responses` adapter).
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Optional API parameters.
    pub api_params: ApiParams,
    /// Streaming vs non-streaming.
    pub stream: bool,
}

/// The provider request outcome stream.
pub type ProviderEventStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<ProviderEvent, LlmProviderError>> + Send>>;

/// The composite `{tenant_hex}{user_hex}` provider `user` value (64 hex
/// chars; falls back to `{tenant}:{user}` when either is not a UUID).
#[must_use]
pub fn provider_user(tenant_id: &str, user_id: &str) -> String {
    let parse = |s: &str| uuid::Uuid::parse_str(s).ok();
    match (parse(tenant_id), parse(user_id)) {
        (Some(t), Some(u)) => format!("{}{}", t.as_simple(), u.as_simple()),
        _ => format!("{tenant_id}:{user_id}"),
    }
}
