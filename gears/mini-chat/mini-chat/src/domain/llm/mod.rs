//! The `llm_provider` port (ADR-0001, ADR-0005): neutral request and event types and the traits
//! the adapters in the infrastructure layer implement.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use mini_chat_sdk::{ModelCatalogEntry, TokenUsage};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::error::DomainError;

/// The role of an input message.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRole {
    /// A user message.
    User,
    /// An assistant message.
    Assistant,
}

/// One message of the provider input.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    /// Who wrote the message.
    pub role: InputRole,
    /// The text.
    pub text: String,
    /// Provider file ids of the images attached to the message (user messages of the current turn).
    pub image_file_ids: Vec<String>,
}

/// A built-in tool of the request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    /// `file_search` over the chat vector store.
    FileSearch {
        /// The provider vector store of the chat.
        vector_store_id: String,
        /// Top-k chunks per call.
        max_num_results: u32,
    },
    /// `web_search`.
    WebSearch {
        /// The search context size: `low`, `medium` or `high`.
        context_size: String,
    },
    /// `code_interpreter` with the ready spreadsheet files.
    CodeInterpreter {
        /// Provider ids of the files made available to the interpreter.
        file_ids: Vec<String>,
    },
}

/// Sampling and passthrough parameters of a model (`general_config.api_params`).
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApiParams {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Frequency penalty.
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    pub presence_penalty: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Keys merged into the top level of the request body.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Reasoning effort hint.
    pub reasoning_effort: Option<String>,
}

impl ApiParams {
    /// The parameters of a catalog model (`general_config.api_params`).
    #[must_use]
    pub fn from_entry(entry: &ModelCatalogEntry) -> Self {
        let params = &entry.general_config.api_params;
        Self {
            temperature: params.temperature,
            top_p: params.top_p,
            frequency_penalty: params.frequency_penalty,
            presence_penalty: params.presence_penalty,
            stop: params.stop.clone(),
            extra_body: params.extra_body.clone(),
            reasoning_effort: params.reasoning_effort.clone(),
        }
    }
}

/// What a request is for (`metadata.request_type`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    /// A chat turn.
    Chat,
    /// A thread-summary call.
    Summary,
}

impl RequestType {
    /// The wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Summary => "summary",
        }
    }
}

/// The identity attached to a provider request (`user` and `metadata`).
#[domain_model]
#[allow(
    clippy::struct_field_names,
    reason = "the fields are the ids of the metadata keys"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestIdentity {
    /// The tenant.
    pub tenant_id: Uuid,
    /// The user (the platform system subject for system tasks).
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
}

impl RequestIdentity {
    /// The `user` field: tenant and user ids in simple form, 64 characters.
    #[must_use]
    pub fn provider_user(&self) -> String {
        format!("{}{}", self.tenant_id.simple(), self.user_id.simple())
    }
}

/// A provider request, independent of the provider API.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// The model name on the provider side.
    pub model: String,
    /// System instructions (system prompt and tool guards).
    pub system_prompt: String,
    /// History and the current message, oldest first, the current message last.
    pub input: Vec<InputMessage>,
    /// Hard cap on the generated tokens.
    pub max_output_tokens: u32,
    /// Built-in tools.
    pub tools: Vec<ToolSpec>,
    /// Maximum built-in tool calls per request.
    pub max_tool_calls: Option<u32>,
    /// Identity for the `user` and `metadata` fields.
    pub identity: RequestIdentity,
    /// What the request is for.
    pub request_type: RequestType,
    /// Model parameters.
    pub api_params: ApiParams,
    /// The `metadata.feature` value: `none` or the tools joined with `+`.
    pub feature: String,
    /// The ids of the secondary (Anthropic Files API) copies of the images of `input`, by the
    /// primary provider file id. Only the `anthropic_messages` adapter reads it: it sends the
    /// image by the secondary id and drops an image without one.
    pub secondary_file_ids: std::collections::HashMap<String, String>,
}

/// A citation extracted from a provider annotation.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RawCitation {
    /// A web source (`url_citation`).
    Web {
        /// The page URL.
        url: String,
        /// The page title.
        title: String,
        /// The answer text in the annotated range (empty when unknown).
        snippet: String,
        /// Start of the annotated range in the answer text, when given.
        start: Option<u64>,
        /// End of the annotated range in the answer text, when given.
        end: Option<u64>,
    },
    /// A file source (`file_citation`), by the provider file id.
    File {
        /// The provider file id.
        provider_file_id: String,
        /// The provider-side filename.
        filename: Option<String>,
    },
}

/// How a provider response ended.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// `response.completed` or an equivalent.
    Complete,
    /// A truncated but valid answer; the reason is for metrics only.
    Incomplete {
        /// Normalized reason (`max_tokens`, `content_filter`, `other`).
        reason: String,
    },
}

/// A translated provider event.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// Incremental answer text.
    TextDelta(String),
    /// Incremental reasoning text (vLLM `<think>` blocks).
    ReasoningDelta(String),
    /// A built-in tool call started.
    ToolStarted {
        /// `file_search`, `web_search` or `code_interpreter`.
        name: String,
    },
    /// A built-in tool call finished.
    ToolDone {
        /// `file_search`, `web_search` or `code_interpreter`.
        name: String,
        /// Tool-specific details for the client.
        details: serde_json::Value,
    },
    /// The terminal success event.
    Completed {
        /// How the response ended.
        completion: Completion,
        /// The provider response id (internal only).
        response_id: Option<String>,
        /// The provider-reported usage.
        usage: Option<TokenUsage>,
        /// The citations found in the answer annotations, without duplicates.
        citations: Vec<RawCitation>,
    },
    /// The terminal failure event.
    Failed {
        /// The provider's message (unsanitized, internal).
        message: String,
        /// Usage reported with the failure.
        usage: Option<TokenUsage>,
    },
}

/// The class of a provider failure.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// `provider_error`.
    Provider,
    /// `provider_timeout`.
    Timeout,
    /// `rate_limited`.
    RateLimited,
}

impl ProviderErrorKind {
    /// The streaming error code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Provider => "provider_error",
            Self::Timeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// A provider failure before or during the stream.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// The class of the failure.
    pub kind: ProviderErrorKind,
    /// The provider's message (unsanitized, internal).
    pub message: String,
    /// The numeric `Retry-After` of a 429, in seconds.
    pub retry_after_secs: Option<u64>,
}

impl ProviderError {
    /// A `provider_error`.
    #[must_use]
    pub fn provider(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Provider,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// A `provider_timeout`.
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Timeout,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// A `rate_limited` failure; the message mentions the delay when it is known.
    #[must_use]
    pub fn rate_limited(retry_after_secs: Option<u64>) -> Self {
        let message = match retry_after_secs {
            Some(secs) => {
                format!("The provider rate limit was reached; retry after {secs} seconds")
            }
            None => "The provider rate limit was reached".to_owned(),
        };
        Self {
            kind: ProviderErrorKind::RateLimited,
            message,
            retry_after_secs,
        }
    }
}

/// The result of a non-streaming call.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct LlmCompletion {
    /// The generated text.
    pub text: String,
    /// The provider-reported usage.
    pub usage: Option<TokenUsage>,
}

/// A stream of translated provider events.
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent, ProviderError>> + Send>>;

/// A provider entry resolved for a tenant: the adapter and the OAGW alias.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// The provider id (`providers.<id>`), for metric labels.
    fn provider_id(&self) -> &str;

    /// Starts a streaming call. Dropping the returned stream aborts the HTTP request.
    ///
    /// # Errors
    /// A [`ProviderError`] when the request cannot be sent or the provider refuses it.
    async fn stream(
        &self,
        ctx: &SecurityContext,
        request: &LlmRequest,
    ) -> Result<LlmStream, ProviderError>;

    /// A non-streaming call (the thread-summary request).
    ///
    /// # Errors
    /// A [`ProviderError`] when the call fails.
    async fn complete(
        &self,
        ctx: &SecurityContext,
        request: &LlmRequest,
    ) -> Result<LlmCompletion, ProviderError>;
}

/// Maps a catalog model's `provider_id` (and the tenant, through `tenant_overrides`) to a provider.
pub trait LlmProviderResolver: Send + Sync {
    /// Resolves the provider.
    ///
    /// # Errors
    /// `PolicyUnavailable` when no provider entry serves the id.
    fn resolve(
        &self,
        tenant_id: Uuid,
        provider_id: &str,
    ) -> Result<Arc<dyn LlmProvider>, DomainError>;
}
