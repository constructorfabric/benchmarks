//! Provider resolution results and the LLM client interface (request, events,
//! errors) shared by the adapters and, via `domain::ports`, the domain.

use async_trait::async_trait;
use futures::stream::BoxStream;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{ProviderKind, StorageKind};

/// A catalog `provider_id` resolved for one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    /// OAGW upstream alias (tenant override alias when one applies).
    pub alias: String,
    /// Chat path, possibly with a `{model}` placeholder and a query string.
    pub api_path: String,
    /// File / vector-store target (`None` when the provider has no storage).
    pub storage: Option<ResolvedStorage>,
}

/// The provider entry that serves file and vector-store operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStorage {
    /// Entry used for storage: the `rag_provider` target, or the provider itself.
    pub provider_id: String,
    pub kind: StorageKind,
    pub alias: String,
    /// `api-version` query value (required for `azure`).
    pub api_version: Option<String>,
    /// Value stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub backend_label: String,
}

// ── LLM client interface (re-exported via `domain::ports`) ───────────────────

/// Provider-neutral chat request; each adapter shapes it for its protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// The catalog `provider_model_id`.
    pub model: String,
    /// System prompt (sent as `instructions` by the Responses adapter).
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub api_params: ModelApiParams,
    pub max_tool_calls: Option<u32>,
    /// Provider `user` field, see [`provider_user`].
    pub user: String,
    pub metadata: RequestMetadata,
    /// Overridden by [`LlmClient::stream`] (`true`) and [`LlmClient::complete`] (`false`).
    pub stream: bool,
}

/// Observability metadata (DESIGN §4 "Provider Request Metadata").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    /// `chat` or `summary`.
    pub request_type: &'static str,
    /// See [`feature_label`].
    pub feature: String,
}

/// One conversation input item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputItem {
    Message {
        /// `user`, `assistant` or `system`.
        role: &'static str,
        content: Vec<ContentPart>,
    },
    /// A function tool call the model made in an earlier agentic iteration.
    FunctionCall {
        call_id: String,
        name: String,
        /// The JSON arguments as the model sent them.
        arguments: String,
    },
    /// The gear's output for [`InputItem::FunctionCall`] `call_id`.
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    InputText(String),
    OutputText(String),
    /// A provider file id (internal only, never exposed to clients).
    InputImage {
        file_id: String,
    },
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
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
    Function {
        name: String,
        description: String,
        parameters: Value,
    },
}

/// Provider-neutral stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart {
        name: String,
        details: Value,
    },
    ToolDone {
        name: String,
        details: Value,
    },
    Citations(Vec<RawCitation>),
    /// The model requested a client-side function tool. The provider request
    /// then ends with a tool-use outcome (`Completed`); the stream service
    /// runs the tool and issues the next request (knowledge search loop).
    FunctionCall {
        call_id: String,
        name: String,
        /// JSON arguments as sent by the model (may be empty or invalid).
        arguments: String,
    },
    /// Normal or truncated (`incomplete_reason` set) completion.
    Completed {
        usage: Option<UsageTokens>,
        /// Provider response id (internal only).
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    /// Provider-reported failure; `usage` kept when the provider sent it.
    Failed {
        error: ProviderError,
        usage: Option<UsageTokens>,
    },
}

/// Citation as reported by the provider (file ids not yet resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    File {
        provider_file_id: String,
        filename: Option<String>,
    },
    Web {
        url: String,
        title: String,
        snippet: String,
        /// `(start_index, end_index)` when the annotation has both.
        span: Option<(u64, u64)>,
    },
}

/// Provider failure mapped to a streaming error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// `provider_error`, `provider_timeout` or `rate_limited`.
    pub code: &'static str,
    /// Sanitized provider message (safe for clients).
    pub message: String,
    /// The provider reported `context_length_exceeded`.
    pub context_length_exceeded: bool,
    /// Numeric `Retry-After` of a 429.
    pub retry_after_secs: Option<u64>,
}

pub const PROVIDER_ERROR: &str = "provider_error";
pub const PROVIDER_TIMEOUT: &str = "provider_timeout";
pub const RATE_LIMITED: &str = "rate_limited";

impl ProviderError {
    /// A `provider_error` with an already-safe message.
    #[must_use]
    pub fn provider(message: impl Into<String>) -> Self {
        Self {
            code: PROVIDER_ERROR,
            message: message.into(),
            context_length_exceeded: false,
            retry_after_secs: None,
        }
    }

    /// A `provider_timeout` with an already-safe message.
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            code: PROVIDER_TIMEOUT,
            ..Self::provider(message)
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Result of a non-streaming call (thread summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub text: String,
    pub usage: Option<UsageTokens>,
}

/// LLM port: provider calls through OAGW.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Start a streaming call. Errors before the stream opens (HTTP status,
    /// gateway) are `Err`; provider failures afterwards are [`LlmEvent::Failed`].
    /// When `cancel` fires the stream ends and the upstream request is dropped.
    /// A provider stream that ends without a terminal event just ends.
    async fn stream(
        &self,
        provider: &ResolvedProvider,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, ProviderError>;

    /// Non-streaming call.
    async fn complete(
        &self,
        provider: &ResolvedProvider,
        req: LlmRequest,
    ) -> Result<CompletionResult, ProviderError>;
}

/// Provider `user`: tenant and user ids in simple (32 hex) form, tenant first
/// (64 chars); `{tenant}:{user}` when either is not a UUID.
#[must_use]
pub fn provider_user(tenant_id: &str, user_id: &str) -> String {
    match (Uuid::parse_str(tenant_id), Uuid::parse_str(user_id)) {
        (Ok(t), Ok(u)) => format!("{}{}", t.simple(), u.simple()),
        _ => format!("{tenant_id}:{user_id}"),
    }
}

/// `metadata.feature`: built-in tools joined with `+` in the order
/// `file_search`, `web_search`, `code_interpreter`; `none` without them.
#[must_use]
pub fn feature_label(tools: &[ToolSpec]) -> String {
    let has = |pred: fn(&ToolSpec) -> bool| tools.iter().any(pred);
    let parts: Vec<&str> = [
        (
            has(|t| matches!(t, ToolSpec::FileSearch { .. })),
            "file_search",
        ),
        (
            has(|t| matches!(t, ToolSpec::WebSearch { .. })),
            "web_search",
        ),
        (
            has(|t| matches!(t, ToolSpec::CodeInterpreter { .. })),
            "code_interpreter",
        ),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect();
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join("+")
    }
}
