//! Shared fixtures of the adapter unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::ModelApiParams;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::model::MessageRole;
use crate::domain::sanitize::user_field;
use crate::infra::llm::{
    FunctionCall, LlmEvent, LlmMessage, LlmRequest, LlmTool, ParseState, ProviderAdapter,
    RequestMetadata, RequestType, ToolResult, ToolRound,
};

pub const TENANT: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const USER: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const CHAT: Uuid = Uuid::from_u128(0x2222_2222_0000_4000_8000_0000_0000_0001);

/// A streaming chat request: system prompt, one earlier exchange, the current
/// user message `hi`, max 4096 output tokens, `max_tool_calls = 2`.
pub fn request(tools: Vec<LlmTool>) -> LlmRequest {
    let metadata = RequestMetadata::new(TENANT, USER, CHAT, RequestType::Chat, &tools);
    LlmRequest {
        model: "model-x".to_owned(),
        instructions: "Be helpful.".to_owned(),
        input: vec![
            LlmMessage::text(MessageRole::User, "earlier question"),
            LlmMessage::text(MessageRole::Assistant, "earlier answer"),
            LlmMessage::text(MessageRole::User, "hi"),
        ],
        max_output_tokens: 4096,
        tools,
        max_tool_calls: Some(2),
        api_params: ModelApiParams::default(),
        user: user_field(&TENANT.to_string(), &USER.to_string()),
        metadata,
        stream: true,
        tool_rounds: Vec::new(),
    }
}

/// One finished `search_knowledge` round: call `call_1` with `{"query":"q"}`
/// answered by `output`.
pub fn knowledge_round(output: &str) -> ToolRound {
    ToolRound {
        results: vec![ToolResult {
            call: FunctionCall {
                call_id: "call_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: r#"{"query":"q"}"#.to_owned(),
            },
            output: output.to_owned(),
        }],
    }
}

/// Feed `(event, data)` frames through `adapter` with one parse state.
pub fn feed(adapter: &dyn ProviderAdapter, frames: &[(&str, Value)]) -> Vec<LlmEvent> {
    let mut st = ParseState::default();
    frames
        .iter()
        .flat_map(|(name, data)| {
            let data = match data {
                Value::String(raw) => raw.clone(),
                other => other.to_string(),
            };
            adapter.parse_event(&mut st, name, &data)
        })
        .collect()
}
