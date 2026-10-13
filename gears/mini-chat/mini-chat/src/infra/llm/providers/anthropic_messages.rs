//! The `anthropic_messages` adapter: the request body of the Messages API
//! (`cpt-cf-mini-chat-dod-streaming-turns-other-adapters`) and the translation
//! of its `content_block_*` event stream.

use std::collections::BTreeMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::{
    EventTranslator, ProviderAdapter, SEARCH_KNOWLEDGE_DESCRIPTION, event_name, request_uri,
    search_knowledge_schema, user_value,
};
use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{BuiltRequest, HistoryRole, ToolCallOutput};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::tools::ToolSpec;
use crate::domain::service::stream::types::{Annotation, FunctionCall, ProviderEvent, ToolName};
use crate::infra::llm::sse_parse::SseEvent;

/// The `anthropic-version` header value sent with every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The beta of the Files API (image blocks with a `file` source).
const FILES_API_BETA: &str = "files-api-2025-04-14";

/// The beta of the code execution server tool.
const CODE_EXECUTION_BETA: &str = "code-execution-2025-05-22";

/// The server tool types.
const WEB_SEARCH_TOOL_TYPE: &str = "web_search_20250305";
const CODE_EXECUTION_TOOL_TYPE: &str = "code_execution_20250522";

/// The Anthropic Messages adapter.
#[derive(Debug, Default)]
pub struct AnthropicMessagesAdapter;

impl AnthropicMessagesAdapter {
    /// Creates the adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::AnthropicMessages
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-anthropic
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        let entry = input.entry;
        let (messages, has_image) = messages(input);
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(entry.provider_model_id));
        if !input.plan.instructions.is_empty() {
            body.insert("system".to_owned(), json!(input.plan.instructions));
        }
        body.insert("messages".to_owned(), Value::Array(messages));
        body.insert(
            "max_tokens".to_owned(),
            json!(input.max_output_tokens_applied),
        );
        body.insert("stream".to_owned(), json!(true));
        body.insert(
            "metadata".to_owned(),
            json!({ "user_id": user_value(input) }),
        );
        let tools: Vec<Value> = input.tools.tools.iter().filter_map(tool_json).collect();
        let code_execution = input
            .tools
            .tools
            .iter()
            .any(|tool| matches!(tool, ToolSpec::CodeInterpreter { .. }));
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        let params = &entry.general_config.api_params;
        if !params.stop.is_empty() {
            body.insert("stop_sequences".to_owned(), json!(params.stop));
        }
        for (key, value) in [("temperature", params.temperature), ("top_p", params.top_p)] {
            if let Some(value) = value {
                body.insert(key.to_owned(), json!(value));
            }
        }
        let mut headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "text/event-stream".to_owned()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
        ];
        let mut betas = Vec::new();
        if has_image {
            betas.push(FILES_API_BETA);
        }
        if code_execution {
            betas.push(CODE_EXECUTION_BETA);
        }
        if !betas.is_empty() {
            headers.push(("anthropic-beta".to_owned(), betas.join(",")));
        }
        Ok(BuiltRequest {
            kind: ProviderKind::AnthropicMessages,
            uri: request_uri(input),
            headers,
            body: Value::Object(body),
        })
    }

    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-anthropic
    fn continue_request(
        &self,
        previous: &BuiltRequest,
        outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError> {
        let mut next = previous.clone();
        let messages = next
            .body
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| DomainError::Internal("the request has no messages array".to_owned()))?;
        let uses: Vec<Value> = outputs
            .iter()
            .map(|output| {
                let arguments = serde_json::from_str::<Value>(&output.call.arguments)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                json!({
                    "type": "tool_use",
                    "id": output.call.call_id,
                    "name": output.call.name,
                    "input": arguments,
                })
            })
            .collect();
        let results: Vec<Value> = outputs
            .iter()
            .map(|output| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": output.call.call_id,
                    "content": output.output,
                })
            })
            .collect();
        messages.push(json!({ "role": "assistant", "content": uses }));
        messages.push(json!({ "role": "user", "content": results }));
        Ok(next)
    }

    fn translator(&self) -> Box<dyn EventTranslator> {
        Box::new(AnthropicTranslator::default())
    }
}

fn tool_json(tool: &ToolSpec) -> Option<Value> {
    match tool {
        ToolSpec::FileSearch { .. } => None,
        ToolSpec::WebSearch { .. } => Some(json!({
            "type": WEB_SEARCH_TOOL_TYPE,
            "name": "web_search",
        })),
        ToolSpec::CodeInterpreter { .. } => Some(json!({
            "type": CODE_EXECUTION_TOOL_TYPE,
            "name": "code_execution",
        })),
        ToolSpec::SearchKnowledge => Some(json!({
            "name": "search_knowledge",
            "description": SEARCH_KNOWLEDGE_DESCRIPTION,
            "input_schema": search_knowledge_schema(),
        })),
    }
}

/// The turns of the request and whether an image block was added.
fn messages(input: &ProviderRequestInput<'_>) -> (Vec<Value>, bool) {
    let plan = input.plan;
    let mut messages = Vec::new();
    let mut has_image = false;
    if let Some(summary) = &plan.summary {
        messages.push(json!({ "role": "user", "content": summary.text }));
    }
    for message in &plan.history {
        let role = match message.role {
            HistoryRole::User => "user",
            HistoryRole::Assistant => "assistant",
        };
        messages.push(json!({ "role": role, "content": message.content }));
    }
    if let Some(current) = &plan.current {
        let mut blocks = vec![json!({ "type": "text", "text": current.content })];
        for image in &current.images {
            match &image.secondary_file_id {
                Some(file_id) => blocks.push(json!({
                    "type": "image",
                    "source": { "type": "file", "file_id": file_id },
                })),
                None => {
                    tracing::warn!(attachment_id = %image.id, "an image has no secondary copy and is not sent");
                }
            }
        }
        if blocks.len() > 1 {
            has_image = true;
            messages.push(json!({ "role": "user", "content": blocks }));
        } else {
            messages.push(json!({ "role": "user", "content": current.content }));
        }
    }
    (messages, has_image)
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-anthropic
/// A `tool_use` block of the response: the call being accumulated.
#[derive(Debug)]
struct Block {
    id: String,
    name: String,
    arguments: String,
}

/// Translates the Messages stream.
#[derive(Debug, Default)]
pub struct AnthropicTranslator {
    message_id: Option<String>,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    have_usage: bool,
    stop_reason: Option<String>,
    blocks: BTreeMap<u64, Block>,
}

fn start(name: ToolName) -> ProviderEvent {
    ProviderEvent::ToolStart {
        name,
        details: json!({}),
    }
}

fn done(name: ToolName) -> ProviderEvent {
    ProviderEvent::ToolDone {
        name,
        details: json!({}),
    }
}

impl AnthropicTranslator {
    /// The usage with the input normalized: the cache counts are subsets of
    /// the input tokens, Anthropic reports them separately.
    fn usage(&self) -> Option<UsageTokens> {
        self.have_usage.then(|| UsageTokens {
            input_tokens: self.input + self.cache_read + self.cache_write,
            output_tokens: self.output,
            cache_read_input_tokens: self.cache_read,
            cache_write_input_tokens: self.cache_write,
            reasoning_tokens: 0,
        })
    }

    fn message_start(&mut self, json: &Value) {
        let message = &json["message"];
        self.message_id = message["id"].as_str().map(str::to_owned);
        let usage = &message["usage"];
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        self.input = count(&usage["input_tokens"]);
        self.cache_read = count(&usage["cache_read_input_tokens"]);
        self.cache_write = count(&usage["cache_creation_input_tokens"]);
        self.output = count(&usage["output_tokens"]);
        self.have_usage = true;
    }

    fn block_start(&mut self, json: &Value) -> Vec<ProviderEvent> {
        let index = json["index"].as_u64().unwrap_or(0);
        let block = &json["content_block"];
        let kind = block["type"].as_str().unwrap_or_default();
        match kind {
            "server_tool_use" => match block["name"].as_str() {
                Some("web_search") => vec![start(ToolName::WebSearch)],
                Some("code_execution") => vec![start(ToolName::CodeInterpreter)],
                _ => Vec::new(),
            },
            "web_search_tool_result" => vec![done(ToolName::WebSearch)],
            kind if kind.ends_with("code_execution_tool_result") => {
                vec![done(ToolName::CodeInterpreter)]
            }
            "tool_use" => {
                let name = block["name"].as_str().unwrap_or_default().to_owned();
                let tool = match name.as_str() {
                    "search_knowledge" => ToolName::SearchKnowledge,
                    "load_files" => ToolName::LoadFiles,
                    _ => ToolName::UnknownTool,
                };
                self.blocks.insert(
                    index,
                    Block {
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name,
                        arguments: String::new(),
                    },
                );
                vec![start(tool)]
            }
            _ => Vec::new(),
        }
    }

    fn block_delta(&mut self, json: &Value) -> Vec<ProviderEvent> {
        let index = json["index"].as_u64().unwrap_or(0);
        let delta = &json["delta"];
        match delta["type"].as_str() {
            Some("text_delta") => match delta["text"].as_str() {
                Some(text) => vec![ProviderEvent::TextDelta {
                    text: text.to_owned(),
                    item_id: None,
                    content_index: u32::try_from(index).ok(),
                }],
                None => Vec::new(),
            },
            Some("citations_delta") => {
                let citation = &delta["citation"];
                if citation["type"] != "web_search_result_location" {
                    return Vec::new();
                }
                let mut value = citation.clone();
                if value.get("text").is_none()
                    && let Some(cited) = citation["cited_text"].as_str()
                {
                    value["text"] = json!(cited);
                }
                vec![ProviderEvent::Annotation(Annotation {
                    value,
                    item_id: None,
                    content_index: u32::try_from(index).ok(),
                    part_text: None,
                })]
            }
            Some("input_json_delta") => {
                if let Some(block) = self.blocks.get_mut(&index)
                    && let Some(fragment) = delta["partial_json"].as_str()
                {
                    block.arguments.push_str(fragment);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn message_delta(&mut self, json: &Value) {
        if let Some(reason) = json["delta"]["stop_reason"].as_str() {
            self.stop_reason = Some(reason.to_owned());
        }
        if let Some(output) = json["usage"]["output_tokens"].as_u64() {
            self.output = output;
            self.have_usage = true;
        }
    }

    fn message_stop(&mut self) -> Vec<ProviderEvent> {
        let usage = self.usage();
        let calls: Vec<FunctionCall> = std::mem::take(&mut self.blocks)
            .into_values()
            .map(|block| FunctionCall {
                call_id: block.id,
                name: block.name,
                arguments: if block.arguments.is_empty() {
                    "{}".to_owned()
                } else {
                    block.arguments
                },
            })
            .collect();
        if self.stop_reason.as_deref() == Some("tool_use") && !calls.is_empty() {
            return vec![ProviderEvent::ToolUse { calls, usage }];
        }
        vec![ProviderEvent::Completed {
            usage,
            response_id: self.message_id.clone(),
            incomplete_reason: (self.stop_reason.as_deref() == Some("max_tokens"))
                .then(|| "max_tokens".to_owned()),
        }]
    }
}

impl EventTranslator for AnthropicTranslator {
    fn translate(&mut self, event: &SseEvent) -> Vec<ProviderEvent> {
        let json: Option<Value> = serde_json::from_str(&event.data).ok();
        let Some(name) = event_name(event, json.as_ref()) else {
            return Vec::new();
        };
        if name == "error" {
            let error = json.as_ref().map(|json| &json["error"]);
            return vec![match error.filter(|error| error["message"].is_string()) {
                Some(error) => ProviderEvent::ProviderError {
                    code: error["type"].as_str().map(str::to_owned),
                    message: error["message"].as_str().unwrap_or_default().to_owned(),
                    usage: None,
                },
                None => ProviderEvent::ProviderError {
                    code: None,
                    message: event.data.clone(),
                    usage: None,
                },
            }];
        }
        let Some(json) = json else {
            return Vec::new();
        };
        match name.as_str() {
            "message_start" => {
                self.message_start(&json);
                Vec::new()
            }
            "content_block_start" => self.block_start(&json),
            "content_block_delta" => self.block_delta(&json),
            "message_delta" => {
                self.message_delta(&json);
                Vec::new()
            }
            "message_stop" => self.message_stop(),
            _ => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        Vec::new()
    }
}

// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-anthropic
#[cfg(test)]
#[path = "anthropic_messages_tests.rs"]
mod tests;
