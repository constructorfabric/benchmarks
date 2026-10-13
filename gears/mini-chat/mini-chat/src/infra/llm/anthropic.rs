//! The Anthropic Messages API adapter (`anthropic_messages`): `file_search` is dropped,
//! `web_search` and `code_interpreter` map to the server tools `web_search` and `code_execution`.

use std::collections::HashMap;

use mini_chat_sdk::TokenUsage;
use serde_json::{Map, Value, json};
use tracing::debug;

use super::adapter::{Adapter, EventParser};
use super::common::{failure_message, insert_sampling, normalize_incomplete_reason};
use crate::domain::llm::{
    Completion, InputMessage, InputRole, LlmCompletion, LlmEvent, LlmRequest, ProviderError,
    RawCitation, ToolSpec,
};

const API_VERSION: &str = "2023-06-01";
/// The beta of the Files API: a `file` image source needs it.
const FILES_BETA: &str = "files-api-2025-04-14";

/// The Messages API adapter.
pub struct AnthropicMessages;

fn tools_json(request: &LlmRequest) -> Vec<Value> {
    request
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolSpec::FileSearch { .. } => None,
            ToolSpec::WebSearch { .. } => {
                let mut tool = json!({ "type": "web_search_20250305", "name": "web_search" });
                if let Some(max_uses) = request.max_tool_calls {
                    tool["max_uses"] = Value::from(max_uses);
                }
                Some(tool)
            }
            ToolSpec::CodeInterpreter { .. } => {
                Some(json!({ "type": "code_execution_20250522", "name": "code_execution" }))
            }
        })
        .collect()
}

/// The `content` of a message: the plain text, or text and image blocks when the message carries
/// images. An image is sent by its secondary copy in the Anthropic Files API; one without such a
/// copy cannot be expressed and is dropped.
fn message_content(message: &InputMessage, secondary_file_ids: &HashMap<String, String>) -> Value {
    let images: Vec<Value> = message
        .image_file_ids
        .iter()
        .filter_map(|file_id| {
            let secondary = secondary_file_ids.get(file_id);
            if secondary.is_none() {
                debug!("an image without an Anthropic file id is dropped from the request");
            }
            secondary
        })
        .map(|file_id| json!({ "type": "image", "source": { "type": "file", "file_id": file_id } }))
        .collect();
    if images.is_empty() {
        return Value::from(message.text.clone());
    }
    let mut blocks = vec![json!({ "type": "text", "text": message.text })];
    blocks.extend(images);
    Value::Array(blocks)
}

impl Adapter for AnthropicMessages {
    fn build_body(&self, request: &LlmRequest, stream: bool) -> Result<Value, ProviderError> {
        let messages: Vec<Value> = request
            .input
            .iter()
            .map(|message| {
                let role = match message.role {
                    InputRole::User => "user",
                    InputRole::Assistant => "assistant",
                };
                json!({ "role": role, "content": message_content(message, &request.secondary_file_ids) })
            })
            .collect();
        let mut body = Map::new();
        body.insert("model".to_owned(), Value::from(request.model.clone()));
        body.insert(
            "max_tokens".to_owned(),
            Value::from(request.max_output_tokens),
        );
        body.insert("stream".to_owned(), Value::from(stream));
        if !request.system_prompt.is_empty() {
            body.insert(
                "system".to_owned(),
                Value::from(request.system_prompt.clone()),
            );
        }
        body.insert("messages".to_owned(), Value::Array(messages));
        body.insert(
            "metadata".to_owned(),
            json!({ "user_id": request.identity.provider_user() }),
        );
        let tools = tools_json(request);
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        // Anthropic rejects a request that sets both temperature and top_p alongside penalties it
        // does not know; only the parameters of its API are sent.
        let mut sampling = Map::new();
        insert_sampling(&mut sampling, &request.api_params);
        for key in ["temperature", "top_p"] {
            if let Some(value) = sampling.remove(key) {
                body.insert(key.to_owned(), value);
            }
        }
        if !request.api_params.stop.is_empty() {
            body.insert("stop_sequences".to_owned(), json!(request.api_params.stop));
        }
        Ok(Value::Object(body))
    }

    fn parser(&self) -> Box<dyn EventParser> {
        Box::new(AnthropicParser::default())
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderError> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::provider("the provider response has no content"))?;
        let text: String = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect();
        Ok(LlmCompletion {
            text,
            usage: body.get("usage").map(anthropic_usage),
        })
    }

    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("anthropic-version", API_VERSION),
            ("anthropic-beta", FILES_BETA),
        ]
    }
}

fn anthropic_usage(usage: &Value) -> TokenUsage {
    let read = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0).max(0);
    let cache_read = read("cache_read_input_tokens");
    let cache_write = read("cache_creation_input_tokens");
    TokenUsage {
        // Anthropic reports cached tokens beside `input_tokens`; the cache counts are a subset of
        // the total in the usage event, so the total includes them.
        input_tokens: read("input_tokens") + cache_read + cache_write,
        output_tokens: read("output_tokens"),
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: 0,
    }
}

#[derive(Default)]
struct AnthropicParser {
    id: Option<String>,
    input_usage: Option<TokenUsage>,
    output_tokens: i64,
    stop_reason: Option<String>,
    /// Open server-tool blocks by index: the shared tool name.
    open_tools: HashMap<i64, &'static str>,
    citations: Vec<RawCitation>,
    finished: bool,
}

/// A `web_search_result_location` citation of a `citations_delta`.
fn web_citation(citation: Option<&Value>) -> Option<RawCitation> {
    let citation = citation?;
    if citation.get("type").and_then(Value::as_str) != Some("web_search_result_location") {
        return None;
    }
    let text = |key: &str| {
        citation
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(RawCitation::Web {
        url: citation.get("url").and_then(Value::as_str)?.to_owned(),
        title: text("title"),
        snippet: text("cited_text"),
        start: None,
        end: None,
    })
}

fn shared_tool_name(name: &str) -> Option<&'static str> {
    match name {
        "web_search" => Some("web_search"),
        "code_execution" => Some("code_interpreter"),
        _ => None,
    }
}

impl AnthropicParser {
    fn completed(&mut self) -> Vec<LlmEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let completion = match self.stop_reason.as_deref() {
            Some("max_tokens") => Completion::Incomplete {
                reason: normalize_incomplete_reason(Some("max_tokens")),
            },
            _ => Completion::Complete,
        };
        let mut usage = self.input_usage.unwrap_or_default();
        usage.output_tokens = self.output_tokens;
        vec![LlmEvent::Completed {
            completion,
            response_id: self.id.clone(),
            usage: Some(usage),
            citations: std::mem::take(&mut self.citations),
        }]
    }
}

impl EventParser for AnthropicParser {
    fn push(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let name = event
            .filter(|name| !name.is_empty())
            .or_else(|| json.get("type").and_then(Value::as_str));
        let index = json.get("index").and_then(Value::as_i64).unwrap_or(0);
        match name {
            Some("message_start") => {
                self.id = json
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.input_usage = json.pointer("/message/usage").map(anthropic_usage);
                if let Some(output) = json
                    .pointer("/message/usage/output_tokens")
                    .and_then(Value::as_i64)
                {
                    self.output_tokens = output;
                }
                Vec::new()
            }
            Some("content_block_start") => {
                let block = json.get("content_block").unwrap_or(&Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("server_tool_use")
                    && let Some(shared) = block
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(shared_tool_name)
                {
                    self.open_tools.insert(index, shared);
                    return vec![LlmEvent::ToolStarted {
                        name: shared.to_owned(),
                    }];
                }
                Vec::new()
            }
            Some("content_block_delta") => {
                let delta = json.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| vec![LlmEvent::TextDelta(text.to_owned())])
                        .unwrap_or_default(),
                    Some("citations_delta") => {
                        if let Some(parsed) = web_citation(delta.get("citation"))
                            && !self.citations.contains(&parsed)
                        {
                            self.citations.push(parsed);
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            Some("content_block_stop") => self
                .open_tools
                .remove(&index)
                .map(|shared| {
                    vec![LlmEvent::ToolDone {
                        name: shared.to_owned(),
                        details: json!({}),
                    }]
                })
                .unwrap_or_default(),
            Some("message_delta") => {
                if let Some(reason) = json.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(output) = json.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                    self.output_tokens = output;
                }
                Vec::new()
            }
            Some("message_stop") => self.completed(),
            Some("error") => {
                self.finished = true;
                vec![LlmEvent::Failed {
                    message: failure_message(data),
                    usage: None,
                }]
            }
            _ => Vec::new(),
        }
    }
}
