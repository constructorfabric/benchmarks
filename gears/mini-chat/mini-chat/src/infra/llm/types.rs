//! Provider-neutral request, event and error types shared by the chat adapters.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use mini_chat_sdk::{ModelApiParams, UsageTokens, WebSearchContextSize};
use serde_json::Value;
use uuid::Uuid;

use super::ChatTarget;

/// Author of an input message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    /// An uploaded image. `secondary_file_id` is the Anthropic Files copy, used only by the
    /// Anthropic adapter.
    Image {
        file_id: String,
        secondary_file_id: Option<String>,
    },
}

/// One item of the conversation input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputItem {
    Message {
        role: Role,
        parts: Vec<ContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
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
        search_context_size: WebSearchContextSize,
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

/// Caller identity and observability context attached to a provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Option<Uuid>,
    /// `chat` or `summary`.
    pub request_type: &'static str,
    /// `none`, or the tools of the request joined with `+` (e.g. `file_search+web_search`).
    pub feature: String,
}

/// A provider-neutral chat request.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    /// The provider's model id (`provider_model_id` of the catalog entry).
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub max_tool_calls: u32,
    pub api_params: ModelApiParams,
    /// `{tenant_id.simple()}{user_id.simple()}` (64 hex characters).
    pub user: String,
    pub metadata: RequestMetadata,
    /// Informational: `ChatAdapter::stream` always streams and `ChatAdapter::complete` never does.
    pub stream: bool,
}

/// Token usage reported by the provider.
#[allow(clippy::struct_field_names)] // names shared with the SDK `UsageTokens`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

impl From<ProviderUsage> for UsageTokens {
    fn from(u: ProviderUsage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_write_input_tokens: u.cache_write_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
        }
    }
}

/// A citation as the provider reported it, before it is mapped to the client schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        /// Character offsets into the assistant text.
        span: Option<(usize, usize)>,
    },
    File {
        provider_file_id: String,
        filename: String,
    },
}

/// Why a provider call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Provider,
    Timeout,
    RateLimited { retry_after_secs: Option<u64> },
    ContextLengthExceeded,
}

/// A failed provider call. `message` is sanitized (no provider identifiers or URLs).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    /// Usage reported together with the failure, when known.
    pub usage: Option<ProviderUsage>,
}

impl ProviderError {
    #[must_use]
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            usage: None,
        }
    }

    /// A [`ProviderErrorKind::Provider`] error.
    #[must_use]
    pub fn provider(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Provider, message)
    }

    #[must_use]
    pub fn with_usage(mut self, usage: Option<ProviderUsage>) -> Self {
        self.usage = usage;
        self
    }

    /// The SSE `error` event code of this failure.
    #[must_use]
    pub fn sse_code(&self) -> &'static str {
        match self.kind {
            ProviderErrorKind::Provider | ProviderErrorKind::ContextLengthExceeded => {
                "provider_error"
            }
            ProviderErrorKind::Timeout => "provider_timeout",
            ProviderErrorKind::RateLimited { .. } => "rate_limited",
        }
    }

    /// The message shown to the client.
    #[must_use]
    pub fn client_message(&self) -> String {
        match self.kind {
            ProviderErrorKind::RateLimited {
                retry_after_secs: Some(secs),
            } => format!("Provider rate limit exceeded, retry in {secs}s"),
            ProviderErrorKind::RateLimited {
                retry_after_secs: None,
            } => "Provider rate limit exceeded".to_owned(),
            _ => self.message.clone(),
        }
    }
}

/// An event of a provider response, translated from the provider's wire format.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
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
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Terminal.
    Completed {
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        citations: Vec<RawCitation>,
    },
    /// Terminal: a truncated but valid completion.
    Incomplete {
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        reason: String,
    },
    /// Terminal.
    Failed(ProviderError),
}

impl ProviderEvent {
    /// Whether no event follows this one.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Incomplete { .. } | Self::Failed(_)
        )
    }
}

/// Result of a non-streaming call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub text: String,
    pub usage: Option<ProviderUsage>,
}

/// Events of one streamed response, in provider order. Dropping the stream drops the OAGW
/// response body, which cancels the provider call.
pub type ProviderEventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// A chat provider protocol.
#[async_trait]
pub trait ChatAdapter: Send + Sync {
    /// Starts a streaming call; events are yielded as the provider sends them. The stream ends
    /// after its terminal event ([`ProviderEvent::is_terminal`]).
    ///
    /// # Errors
    /// The call failed before a stream was established (transport, gateway or HTTP error).
    async fn stream(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError>;

    /// Runs a non-streaming call (thread summary).
    ///
    /// # Errors
    /// The call failed or the response could not be understood.
    async fn complete(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<CompletionResult, ProviderError>;
}
