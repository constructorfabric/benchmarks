//! `llm_provider` library (ADR-0001, ADR-0005): provider resolution, the four
//! adapter kinds and the OAGW transport. Adapters translate provider wire
//! events into the internal [`LlmEvent`] model.

pub mod anthropic;
pub mod chat_completions;
pub mod client;
pub mod openai_responses;
pub mod resolver;
pub mod sse;
pub mod vllm;

use mini_chat_sdk::ModelApiParams;
use serde_json::Value;

use crate::config::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemRole {
    User,
    Assistant,
}

/// One conversation item sent to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatItem {
    pub role: ItemRole,
    pub text: String,
    /// Provider file ids of images (user items only).
    pub images: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchTool {
    pub vector_store_ids: Vec<String>,
    pub max_num_results: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolsSpec {
    pub file_search: Option<FileSearchTool>,
    /// Search context size (`low|medium|high`).
    pub web_search: Option<String>,
    /// Provider file ids available to the code interpreter.
    pub code_interpreter: Option<Vec<String>>,
    /// `search_knowledge` function tool (knowledge search).
    pub knowledge: bool,
}

impl ToolsSpec {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file_search.is_none() && self.web_search.is_none() && self.code_interpreter.is_none()
    }

    /// `feature` metadata value (`none`, `file_search+web_search`, …).
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
        if parts.is_empty() { "none".to_owned() } else { parts.join("+") }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    pub request_type: &'static str,
    pub feature: String,
}

/// Normalized provider request.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub instructions: String,
    pub items: Vec<ChatItem>,
    pub max_output_tokens: u32,
    pub tools: ToolsSpec,
    pub max_tool_calls: u32,
    pub api_params: ModelApiParams,
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
    /// Function call / output items of the knowledge-search loop
    /// (Responses API item format).
    pub extra_input: Vec<Value>,
}

pub const KNOWLEDGE_TOOL: &str = "search_knowledge";

/// JSON schema of the `search_knowledge` parameters.
#[must_use]
pub fn knowledge_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to search for in the knowledge base"},
            "top_k": {"type": "integer", "description": "Maximum number of excerpts to return"}
        },
        "required": ["query"]
    })
}

pub const KNOWLEDGE_DESCRIPTION: &str = "Search the organization knowledge base and return relevant excerpts.";

/// `user` field: tenant and user UUIDs in simple form (64 chars).
#[must_use]
pub fn user_field(tenant_id: uuid::Uuid, user_id: uuid::Uuid) -> String {
    format!("{}{}", tenant_id.simple(), user_id.simple())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

impl Usage {
    #[must_use]
    pub fn is_nonzero(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RawCitation {
    Url {
        url: String,
        title: String,
        start: Option<usize>,
        end: Option<usize>,
        text: Option<String>,
        /// Text of the output part that carries the annotation.
        part_text: Option<String>,
    },
    File {
        file_id: String,
        filename: String,
        start: Option<usize>,
        end: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub response_id: Option<String>,
    pub usage: Option<Usage>,
    pub incomplete_reason: Option<String>,
}

/// Stable streaming failure codes produced by the transport/adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmFailure {
    pub code: &'static str,
    pub message: String,
    pub usage: Option<Usage>,
}

impl LlmFailure {
    #[must_use]
    pub fn provider(message: impl Into<String>) -> Self {
        Self {
            code: "provider_error",
            message: crate::domain::sanitize::sanitize_provider_message(&message.into()),
            usage: None,
        }
    }
    #[must_use]
    pub fn timeout() -> Self {
        Self {
            code: "provider_timeout",
            message: "Provider request timed out".to_owned(),
            usage: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart { name: String, details: Value },
    ToolDone { name: String, details: Value },
    Citation(RawCitation),
    FunctionCall { name: String, call_id: String, arguments: String },
    Completed(Completion),
    Failed(LlmFailure),
}

/// Adapter contract (one implementation per provider kind).
pub trait Adapter: Send + Sync {
    fn build_body(&self, req: &LlmRequest) -> Value;
    /// Translate one SSE event (`event:` name, `data:` payload).
    fn parse_event(&self, state: &mut ParseState, event: Option<&str>, data: &str) -> Vec<LlmEvent>;
    /// Parse a non-streaming response body into `(text, usage)`.
    ///
    /// # Errors
    /// When the body carries an error or no text.
    fn parse_complete(&self, body: &Value) -> Result<(String, Option<Usage>), LlmFailure>;
    /// Extra request headers the provider requires.
    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}

/// Mutable per-stream parsing state shared by adapters.
#[derive(Debug, Default)]
pub struct ParseState {
    pub text: String,
    pub in_think: bool,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    pub response_id: Option<String>,
    pub seen_citations: Vec<String>,
    pub tool_index: std::collections::HashMap<u64, (String, String, String)>,
    pub block_kinds: std::collections::HashMap<u64, String>,
}

#[must_use]
pub fn adapter_for(kind: ProviderKind) -> Box<dyn Adapter> {
    match kind {
        ProviderKind::OpenaiResponses => Box::new(openai_responses::OpenAiResponses),
        ProviderKind::OpenaiChatCompletions => Box::new(chat_completions::ChatCompletions),
        ProviderKind::VllmResponses => Box::new(vllm::VllmResponses),
        ProviderKind::AnthropicMessages => Box::new(anthropic::AnthropicMessages),
    }
}

/// Usage object in Responses API format.
#[must_use]
pub fn parse_responses_usage(v: &Value) -> Option<Usage> {
    let u = v.as_object()?;
    let get = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(Usage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_input_tokens: u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        cache_write_input_tokens: 0,
        reasoning_tokens: u
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

/// Extract `(code, message)` from an error JSON (`{error:{code,message}}`,
/// `{response:{error:..}}` or flat `{code,message}`).
#[must_use]
pub fn error_message(v: &Value) -> Option<String> {
    let e = v
        .get("response")
        .and_then(|r| r.get("error"))
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()))
        .unwrap_or(v);
    if let Some(s) = e.as_str() {
        return Some(s.to_owned());
    }
    e.get("message").and_then(Value::as_str).map(str::to_owned)
}
