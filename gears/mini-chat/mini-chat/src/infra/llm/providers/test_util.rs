//! Shared fixtures of the adapter unit tests.

use mini_chat_sdk::ApiParams;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ParseState, ProviderAdapter};
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::sse_parser::SseEvent;
use crate::infra::llm::types::{
    InputMessage, LlmEvent, LlmRequest, RequestMetadata, Role, ToolSpec,
};

pub const TENANT: Uuid = Uuid::from_u128(0xa1);
pub const USER: Uuid = Uuid::from_u128(0xb2);
pub const CHAT: Uuid = Uuid::from_u128(0xc3);

pub fn no_params() -> ApiParams {
    ApiParams {
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stop: Vec::new(),
        extra_body: None,
        reasoning_effort: None,
    }
}

/// Chat request: two history messages and the current question.
pub fn req_with(tools: Vec<ToolSpec>) -> LlmRequest {
    let metadata = RequestMetadata::chat(TENANT, USER, CHAT, &tools);
    LlmRequest {
        model: "model-x".into(),
        instructions: "SYSTEM PROMPT".into(),
        input: vec![
            InputMessage::text(Role::User, "first question"),
            InputMessage::text(Role::Assistant, "first answer"),
            InputMessage::text(Role::User, "current question"),
        ],
        max_output_tokens: 4096,
        tools,
        max_tool_calls: 2,
        api_params: no_params(),
        user: provider_user_field(TENANT, USER),
        metadata,
        stream: true,
    }
}

/// One of each tool kind.
pub fn all_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_abcdefghijklmnop".into()],
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-abcdefghijklmnop".into()],
        },
        knowledge_tool(),
    ]
}

/// The `search_knowledge` function tool.
pub fn knowledge_tool() -> ToolSpec {
    ToolSpec::Function {
        name: "search_knowledge".into(),
        description: "Search the knowledge base".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }
}

/// SSE event with an optional `event:` line.
pub fn ev(name: Option<&str>, data: &Value) -> SseEvent {
    SseEvent {
        event: name.map(str::to_owned),
        data: data.to_string(),
    }
}

/// SSE event without `event:` line and raw (possibly non-JSON) data.
pub fn raw(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_owned(),
    }
}

/// Parse `events` in order with one parser state.
pub fn parse_all(adapter: &dyn ProviderAdapter, events: &[SseEvent]) -> Vec<LlmEvent> {
    let mut state = ParseState::default();
    events
        .iter()
        .flat_map(|e| adapter.parse_event(e, &mut state))
        .collect()
}
