//! `llm_provider`: in-process provider adapters (ADR-0001, ADR-0005).
//!
//! The domain builds a provider-agnostic [`LlmRequest`]; an adapter selected by
//! the provider entry's `kind` shapes the HTTP body and translates the
//! provider SSE stream into [`ProviderEvent`]s.

pub mod anthropic_messages;
pub mod chat_completions;
pub mod client;
pub mod openai_responses;
pub mod resolver;
pub mod sse_parser;
pub mod storage;
pub mod vllm_responses;

use mini_chat_sdk::{ModelApiParams, UsageTokens};
use serde_json::Value;

use crate::config::ProviderKind;
use sse_parser::SseEvent;

/// Message role in the assembled input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: Role,
    pub text: String,
    /// Provider file ids of images attached to this message.
    pub image_file_ids: Vec<String>,
    /// Anthropic secondary file ids of the same images (Anthropic adapter only).
    pub secondary_image_file_ids: Vec<String>,
}

impl InputMessage {
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            image_file_ids: Vec::new(),
            secondary_image_file_ids: Vec::new(),
        }
    }
}

/// Built-in or function tool offered to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    FileSearch {
        vector_store_id: String,
        max_num_results: u32,
    },
    WebSearch {
        context_size: String,
    },
    CodeInterpreter {
        file_ids: Vec<String>,
    },
    SearchKnowledge,
}

impl ToolSpec {
    #[must_use]
    pub fn feature_name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
            Self::SearchKnowledge => "search_knowledge",
        }
    }
}

/// Request metadata (DESIGN §4 "Provider Request Metadata").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    /// `chat` | `summary`
    pub request_type: String,
    pub feature: String,
}

/// One completed function-tool round trip of the knowledge-search agentic
/// loop: the model's call and the gear's output, replayed in the next request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExchange {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub output: String,
}

/// Provider-agnostic request.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub provider_model_id: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub max_output_tokens: u32,
    pub tools: Vec<ToolSpec>,
    pub max_tool_calls: u32,
    pub api_params: ModelApiParams,
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
    /// Function-tool calls and outputs appended after `input` (agentic loop).
    pub tool_exchanges: Vec<ToolExchange>,
}

/// `feature` metadata value from the tool list.
#[must_use]
pub fn feature_label(tools: &[ToolSpec]) -> String {
    let names: Vec<&str> = tools
        .iter()
        .filter(|t| !matches!(t, ToolSpec::SearchKnowledge))
        .map(ToolSpec::feature_name)
        .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join("+")
    }
}

/// `user` field: `{tenant_hex}{user_hex}` (64 chars) or `tenant:user`.
#[must_use]
pub fn provider_user_field(tenant_id: &str, user_id: &str) -> String {
    match (uuid::Uuid::parse_str(tenant_id), uuid::Uuid::parse_str(user_id)) {
        (Ok(t), Ok(u)) => format!("{}{}", t.as_simple(), u.as_simple()),
        _ => format!("{tenant_id}:{user_id}"),
    }
}

/// Kind of a text delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// A citation as reported by the provider (before id resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    File {
        file_id: String,
    },
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u64, u64)>,
    },
}

/// Normalized provider stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    Delta {
        kind: DeltaKind,
        text: String,
    },
    ToolStart {
        name: String,
        details: Value,
    },
    ToolDone {
        name: String,
        details: Value,
    },
    Citations(Vec<RawCitation>),
    /// The model called a function tool (client-side tool).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Completed {
        response_id: Option<String>,
        usage: Option<UsageTokens>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: Option<String>,
        message: String,
        usage: Option<UsageTokens>,
    },
}

/// Result of a non-streaming completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub text: String,
    pub usage: Option<UsageTokens>,
}

/// Adapter contract.
pub trait Adapter: Send {
    /// JSON body for the request.
    fn build_body(&self, req: &LlmRequest) -> Value;
    /// Translate one SSE event into zero or more provider events.
    fn translate(&mut self, ev: &SseEvent) -> Vec<ProviderEvent>;
    /// Parse a non-streaming JSON response (thread summary).
    ///
    /// # Errors
    /// The provider error message when the body is an error.
    fn parse_completion(&self, body: &Value) -> Result<CompletionResult, String>;
}

/// Build the adapter for a provider kind.
#[must_use]
pub fn adapter_for(kind: ProviderKind) -> Box<dyn Adapter> {
    match kind {
        ProviderKind::OpenaiResponses => Box::new(openai_responses::OpenAiResponses::default()),
        ProviderKind::VllmResponses => Box::new(vllm_responses::VllmResponses::default()),
        ProviderKind::OpenaiChatCompletions => {
            Box::new(chat_completions::ChatCompletions::default())
        }
        ProviderKind::AnthropicMessages => Box::new(anthropic_messages::AnthropicMessages::default()),
    }
}

/// Keys the request controls; `extra_body` entries with these names are ignored.
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

/// Merge `extra_body` into the top level of `body`, skipping reserved keys.
pub fn merge_extra_body(body: &mut Value, params: &ModelApiParams) {
    let Some(extra) = &params.extra_body else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for (k, v) in extra {
        if RESERVED_BODY_KEYS.contains(&k.as_str()) {
            tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// Apply `api_params` sampling fields that are set.
pub fn apply_sampling(body: &mut Value, params: &ModelApiParams, include_stop: bool) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if let Some(t) = params.temperature {
        obj.insert("temperature".into(), Value::from(t));
    }
    if let Some(t) = params.top_p {
        obj.insert("top_p".into(), Value::from(t));
    }
    if let Some(t) = params.frequency_penalty {
        obj.insert("frequency_penalty".into(), Value::from(t));
    }
    if let Some(t) = params.presence_penalty {
        obj.insert("presence_penalty".into(), Value::from(t));
    }
    if include_stop && !params.stop.is_empty() {
        obj.insert("stop".into(), Value::from(params.stop.clone()));
    }
}

/// Parse an OpenAI-style usage object (`input_tokens` / `prompt_tokens` naming).
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let obj = v.as_object()?;
    let num = |k: &str| obj.get(k).and_then(Value::as_i64);
    let input = num("input_tokens").or_else(|| num("prompt_tokens")).unwrap_or(0);
    let output = num("output_tokens")
        .or_else(|| num("completion_tokens"))
        .unwrap_or(0);
    let cached = v
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| v.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| num("cache_read_input_tokens"))
        .unwrap_or(0);
    let cache_write = num("cache_creation_input_tokens").unwrap_or(0);
    let reasoning = v
        .pointer("/output_tokens_details/reasoning_tokens")
        .or_else(|| v.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(UsageTokens {
        input_tokens: input.max(0),
        output_tokens: output.max(0),
        cache_read_input_tokens: cached.max(0),
        cache_write_input_tokens: cache_write.max(0),
        reasoning_tokens: reasoning.max(0),
    })
}

/// Extract `(code, message)` from an error payload (`{error:{code,message}}`,
/// `{response:{error:{..}}}` or flat `{code,message}`).
#[must_use]
pub fn parse_error_payload(data: &str) -> (Option<String>, String) {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return (None, data.to_owned());
    };
    let err = v
        .pointer("/response/error")
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| e.is_object()))
        .unwrap_or(&v);
    let code = err
        .get("code")
        .and_then(|c| c.as_str().map(ToOwned::to_owned).or_else(|| Some(c.to_string())))
        .filter(|c| c != "null");
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| "Provider error".to_owned(), ToOwned::to_owned);
    (code, message)
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod openai_responses_tests;
