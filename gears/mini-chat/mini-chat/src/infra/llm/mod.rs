//! In-process LLM provider library (ADR-0001, ADR-0005): request building,
//! stream translation and error mapping per adapter kind.

pub mod anthropic;
pub mod chat_completions;
pub mod openai_responses;
pub mod sse;
pub mod transport;

use mini_chat_sdk::{ModelApiParams, UsageTokens};
use serde_json::{Map, Value, json};

use crate::config::ProviderKind;

/// Streaming error codes sent in SSE `error` events (DESIGN §3.3).
pub mod codes {
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const PROVIDER_TIMEOUT: &str = "provider_timeout";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
    pub const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
    pub const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
    pub const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
    pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
    pub const FINALIZATION_FAILED: &str = "finalization_failed";
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
}

/// Content part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    /// Provider file id of an image.
    Image(String),
}

/// Input message for the provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: String,
    pub parts: Vec<ContentPart>,
}

impl InputMessage {
    #[must_use]
    pub fn text(role: &str, text: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            parts: vec![ContentPart::Text(text.into())],
        }
    }

    #[must_use]
    pub fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                ContentPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Built-in tools.
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
}

impl ToolSpec {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
        }
    }
}

/// Normalized provider request.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub provider_model_id: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub max_output_tokens: u32,
    pub tools: Vec<ToolSpec>,
    pub max_tool_calls: u32,
    pub user: String,
    pub metadata: Map<String, Value>,
    pub api_params: ModelApiParams,
    pub stream: bool,
}

/// Keys the request controls; `extra_body` cannot override them.
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

/// Composite provider `user` value: tenant hex + user hex (64 chars).
#[must_use]
pub fn provider_user(tenant: uuid::Uuid, user: uuid::Uuid) -> String {
    format!("{}{}", tenant.as_simple(), user.as_simple())
}

/// `feature` metadata value from the tool list.
#[must_use]
pub fn feature_label(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return "none".to_owned();
    }
    tools.iter().map(ToolSpec::name).collect::<Vec<_>>().join("+")
}

/// A citation as reported by the provider (before id mapping).
#[derive(Debug, Clone, PartialEq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u64, u64)>,
    },
    File {
        file_id: String,
        filename: String,
        span: Option<(u64, u64)>,
    },
}

/// Normalized provider stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart { name: String, details: Value },
    ToolDone { name: String, details: Value },
    Citation(RawCitation),
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: &'static str,
        message: String,
        usage: Option<UsageTokens>,
    },
}

/// Per-stream translation state.
#[derive(Debug, Default)]
pub struct TranslateState {
    /// Text of the current output part (for annotation snippets).
    pub part_text: String,
    pub saw_annotation_events: bool,
    pub think_open: bool,
    pub chat_tool_calls: Vec<(String, String, String)>,
    pub anthropic_usage: Option<UsageTokens>,
    pub anthropic_stop: Option<String>,
    pub done_emitted: bool,
    /// Open Anthropic server-tool blocks: (index, mapped name).
    pub open_blocks: Vec<(u64, String)>,
}

/// Adds API params and `extra_body` to an OpenAI-style body.
pub fn apply_api_params(body: &mut Map<String, Value>, p: &ModelApiParams, chat_style: bool) {
    if let Some(v) = p.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if let Some(v) = p.frequency_penalty {
        body.insert("frequency_penalty".into(), json!(v));
    }
    if let Some(v) = p.presence_penalty {
        body.insert("presence_penalty".into(), json!(v));
    }
    if !p.stop.is_empty() {
        body.insert("stop".into(), json!(p.stop));
    }
    if let Some(effort) = &p.reasoning_effort {
        if chat_style {
            body.insert("reasoning_effort".into(), json!(effort));
        } else {
            body.insert("reasoning".into(), json!({ "effort": effort }));
        }
    }
    if let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if RESERVED_BODY_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

/// Builds the provider JSON body for the adapter kind.
#[must_use]
pub fn build_body(kind: ProviderKind, req: &LlmRequest) -> Value {
    match kind {
        ProviderKind::OpenaiResponses => openai_responses::build_body(req, true),
        ProviderKind::VllmResponses => openai_responses::build_body(req, false),
        ProviderKind::OpenaiChatCompletions => chat_completions::build_body(req),
        ProviderKind::AnthropicMessages => anthropic::build_body(req),
    }
}

/// Translates one provider SSE frame.
pub fn translate(
    kind: ProviderKind,
    state: &mut TranslateState,
    event: Option<&str>,
    data: &str,
) -> Vec<ProviderEvent> {
    match kind {
        ProviderKind::OpenaiResponses => openai_responses::translate(state, event, data, false),
        ProviderKind::VllmResponses => openai_responses::translate(state, event, data, true),
        ProviderKind::OpenaiChatCompletions => chat_completions::translate(state, data),
        ProviderKind::AnthropicMessages => anthropic::translate(state, event, data),
    }
}

/// Text and usage of a non-streaming completion (thread summary call).
#[must_use]
pub fn parse_completion(kind: ProviderKind, body: &Value) -> (String, Option<UsageTokens>) {
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            openai_responses::parse_completion(body)
        }
        ProviderKind::OpenaiChatCompletions => chat_completions::parse_completion(body),
        ProviderKind::AnthropicMessages => anthropic::parse_completion(body),
    }
}

/// Extracts `{message, code}` from a provider error JSON body.
#[must_use]
pub fn error_message_from_body(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => {
            let err = v.get("error").unwrap_or(&v);
            err.get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| err.as_str().map(str::to_owned))
                .unwrap_or_else(|| "Provider returned an error".to_owned())
        }
        Err(_) => {
            let s = String::from_utf8_lossy(body).trim().to_owned();
            if s.is_empty() {
                "Provider returned an error".to_owned()
            } else {
                s.chars().take(500).collect()
            }
        }
    }
}

/// Usage from an OpenAI-style usage object (`input_tokens`/`prompt_tokens`).
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let u = v.as_object()?;
    let get = |k: &str| u.get(k).and_then(Value::as_i64);
    let input = get("input_tokens").or_else(|| get("prompt_tokens")).unwrap_or(0);
    let output = get("output_tokens")
        .or_else(|| get("completion_tokens"))
        .unwrap_or(0);
    let cached = u
        .get("input_tokens_details")
        .or_else(|| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| get("cache_read_input_tokens"))
        .unwrap_or(0);
    let reasoning = u
        .get("output_tokens_details")
        .or_else(|| u.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: get("cache_creation_input_tokens").unwrap_or(0),
        reasoning_tokens: reasoning,
    })
}

#[cfg(test)]
#[path = "llm_tests.rs"]
mod llm_tests;
