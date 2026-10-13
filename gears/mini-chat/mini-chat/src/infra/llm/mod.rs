//! `llm_provider`: in-process provider library (ADR-0001, ADR-0005).
//!
//! Adapters build provider requests and parse provider SSE streams into
//! internal [`ProviderEvent`]s. All traffic goes through a
//! [`ProviderTransport`] (OAGW in production).

pub mod anthropic_messages;
pub mod chat_completions;
pub mod client;
pub mod openai_responses;
pub mod resolver;
pub mod sse;
pub mod storage;
pub mod transport;
pub mod vllm_responses;

use serde_json::Value;

pub use resolver::{ProviderResolver, ResolvedProvider};
pub use transport::{ProviderTransport, TransportError};

/// Provider-reported usage, normalized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// A citation as reported by the provider (before id mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    Url {
        url: String,
        title: String,
        start: Option<usize>,
        end: Option<usize>,
    },
    File {
        file_id: String,
        filename: Option<String>,
    },
}

/// Terminal error classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    ProviderError,
    Timeout,
    RateLimited,
    UnexpectedToolUse,
}

impl ProviderErrorKind {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::Timeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
            Self::UnexpectedToolUse => "unexpected_tool_use",
        }
    }
}

/// Internal streaming events.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    ResponseId(String),
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart { name: String, details: Value },
    ToolDone { name: String, details: Value },
    Citation(RawCitation),
    Completed {
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        incomplete_reason: Option<String>,
    },
    Failed {
        kind: ProviderErrorKind,
        message: String,
        usage: Option<ProviderUsage>,
    },
}

/// One input message of the provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: &'static str,
    pub text: String,
    /// Provider file ids of images (user messages of the current turn only).
    pub image_file_ids: Vec<String>,
}

/// Tools of a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestTools {
    pub file_search: Option<FileSearchTool>,
    pub web_search: Option<String>,
    pub code_interpreter: Option<Vec<String>>,
    pub max_tool_calls: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchTool {
    pub vector_store_id: String,
    pub max_num_results: u32,
}

impl RequestTools {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file_search.is_none() && self.web_search.is_none() && self.code_interpreter.is_none()
    }

    /// `feature` metadata value.
    #[must_use]
    pub fn feature(&self) -> String {
        let mut parts = Vec::new();
        if self.file_search.is_some() {
            parts.push("file_search");
        }
        if self.web_search.is_some() {
            parts.push("web_search");
        }
        if self.code_interpreter.is_some() {
            parts.push("code_interpreter");
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join("+")
        }
    }
}

/// Provider-agnostic chat request.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub tools: RequestTools,
    pub max_output_tokens: u32,
    pub user: String,
    pub metadata: serde_json::Map<String, Value>,
    pub api_params: mini_chat_sdk::ApiParams,
    pub stream: bool,
}

/// Composite `user` identifier: tenant and user in simple form (64 chars).
#[must_use]
pub fn provider_user(tenant_id: uuid::Uuid, user_id: uuid::Uuid) -> String {
    format!("{}{}", tenant_id.as_simple(), user_id.as_simple())
}

/// Keys of `extra_body` that the request controls.
pub const RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "input",
    "messages",
    "instructions",
    "system",
    "stream",
    "stream_options",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "max_tool_calls",
    "tools",
    "tool_choice",
    "include",
    "store",
    "previous_response_id",
    "user",
    "metadata",
];

pub(crate) fn apply_api_params(body: &mut serde_json::Map<String, Value>, p: &mini_chat_sdk::ApiParams, with_extra: bool) {
    if let Some(v) = p.temperature {
        body.insert("temperature".into(), Value::from(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".into(), Value::from(v));
    }
    if let Some(v) = p.frequency_penalty {
        body.insert("frequency_penalty".into(), Value::from(v));
    }
    if let Some(v) = p.presence_penalty {
        body.insert("presence_penalty".into(), Value::from(v));
    }
    if with_extra && let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if RESERVED_BODY_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}
