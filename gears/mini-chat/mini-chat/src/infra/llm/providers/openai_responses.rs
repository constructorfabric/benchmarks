//! The `openai_responses` adapter: the request body of the Responses API
//! (`cpt-cf-mini-chat-dod-streaming-turns-openai-responses-adapter`) and the
//! translation of its streaming events.

use mini_chat_sdk::{UsageTokens, WebSearchContextSize};
use serde_json::{Map, Value, json};

use super::{EventTranslator, ProviderAdapter};
use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{BuiltRequest, HistoryRole, ToolCallOutput};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::tools::ToolSpec;
use crate::domain::service::stream::types::{Annotation, FunctionCall, ProviderEvent, ToolName};
use crate::infra::llm::sse_parse::SseEvent;

/// Output of a `code_interpreter` `done` event is cut to this many characters.
pub const CODE_OUTPUT_MAX_CHARS: usize = 8192;

/// Appended to a cut code interpreter output.
pub const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// The Responses API adapter.
#[derive(Debug, Default)]
pub struct OpenAiResponsesAdapter;

impl OpenAiResponsesAdapter {
    /// Creates the adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Keys of the request that `extra_body` may not override.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-never-send
pub(super) const CONTROLLED_KEYS: [&str; 18] = [
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
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-never-send

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-openai-responses-adapter:p1
impl ProviderAdapter for OpenAiResponsesAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenaiResponses
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-openai-responses
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        let entry = input.entry;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-build-uri
        let uri = super::request_uri(input);
        let headers = super::base_headers();
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-build-uri
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(entry.provider_model_id));
        if !input.plan.instructions.is_empty() {
            body.insert("instructions".to_owned(), json!(input.plan.instructions));
        }
        body.insert("input".to_owned(), Value::Array(input_items(input)));
        let tools: Vec<Value> = input.tools.tools.iter().map(tool_json).collect();
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        if input
            .tools
            .tools
            .iter()
            .any(|tool| matches!(tool, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert(
                "include".to_owned(),
                json!(["code_interpreter_call.outputs"]),
            );
        }
        if input.tools.tools.iter().any(ToolSpec::is_builtin) {
            body.insert("max_tool_calls".to_owned(), json!(entry.max_tool_calls));
        }
        body.insert(
            "max_output_tokens".to_owned(),
            json!(input.max_output_tokens_applied),
        );
        body.insert("stream".to_owned(), json!(true));
        body.insert("store".to_owned(), json!(false));
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-user-value
        let user = super::user_value(input);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-user-value
        body.insert("user".to_owned(), json!(user));
        let feature = if input.tools.feature_label.is_empty() {
            "none"
        } else {
            input.tools.feature_label.as_str()
        };
        body.insert(
            "metadata".to_owned(),
            json!({
                "tenant_id": input.tenant_id.to_string(),
                "user_id": input.user_id.to_string(),
                "chat_id": input.chat_id.to_string(),
                "request_type": "chat",
                "feature": feature,
            }),
        );
        let params = &entry.general_config.api_params;
        for (key, value) in [
            ("temperature", params.temperature),
            ("top_p", params.top_p),
            ("frequency_penalty", params.frequency_penalty),
            ("presence_penalty", params.presence_penalty),
        ] {
            if let Some(value) = value {
                body.insert(key.to_owned(), json!(value));
            }
        }
        if let Some(effort) = &params.reasoning_effort {
            body.insert("reasoning".to_owned(), json!({ "effort": effort }));
        }
        merge_extra_body(&mut body, params.extra_body.as_ref());
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-return-request
        Ok(BuiltRequest {
            kind: ProviderKind::OpenaiResponses,
            uri,
            headers,
            body: Value::Object(body),
        })
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-return-request
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-openai-responses

    fn continue_request(
        &self,
        previous: &BuiltRequest,
        outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError> {
        continue_responses(previous, outputs)
    }

    fn translator(&self) -> Box<dyn EventTranslator> {
        Box::new(ResponsesTranslator::default())
    }
}

/// Appends, per call, a `function_call` item and its `function_call_output`
/// item to the `input` array (`inst-append-output`).
pub(super) fn continue_responses(
    previous: &BuiltRequest,
    outputs: &[ToolCallOutput],
) -> Result<BuiltRequest, DomainError> {
    let mut next = previous.clone();
    let input = next
        .body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| DomainError::Internal("the request has no input array".to_owned()))?;
    for output in outputs {
        input.push(json!({
            "type": "function_call",
            "call_id": output.call.call_id,
            "name": output.call.name,
            "arguments": output.call.arguments,
        }));
        input.push(json!({
            "type": "function_call_output",
            "call_id": output.call.call_id,
            "output": output.output,
        }));
    }
    Ok(next)
}

fn text_item(role: &str, part_type: &str, text: &str) -> Value {
    json!({ "role": role, "content": [ { "type": part_type, "text": text } ] })
}

pub(super) fn input_items(input: &ProviderRequestInput<'_>) -> Vec<Value> {
    let plan = input.plan;
    let mut items = Vec::new();
    if let Some(summary) = &plan.summary {
        items.push(text_item("user", "input_text", &summary.text));
    }
    for message in &plan.history {
        items.push(match message.role {
            HistoryRole::User => text_item("user", "input_text", &message.content),
            HistoryRole::Assistant => text_item("assistant", "output_text", &message.content),
        });
    }
    if let Some(current) = &plan.current {
        let mut content = vec![json!({ "type": "input_text", "text": current.content })];
        for image in &current.images {
            match &image.provider_file_id {
                Some(file_id) => content.push(json!({ "type": "input_image", "file_id": file_id })),
                None => {
                    tracing::warn!(attachment_id = %image.id, "an image has no provider file id and is not sent");
                }
            }
        }
        items.push(json!({ "role": "user", "content": content }));
    }
    items
}

fn tool_json(tool: &ToolSpec) -> Value {
    match tool {
        ToolSpec::FileSearch {
            vector_store_id,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": [vector_store_id],
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({
            "type": "web_search",
            "search_context_size": match search_context_size {
                WebSearchContextSize::Low => "low",
                WebSearchContextSize::Medium => "medium",
                WebSearchContextSize::High => "high",
            },
        }),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": { "type": "auto", "file_ids": file_ids },
        }),
        ToolSpec::SearchKnowledge => json!({
            "type": "function",
            "name": "search_knowledge",
            "description": super::SEARCH_KNOWLEDGE_DESCRIPTION,
            "parameters": super::search_knowledge_schema(),
        }),
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-merge-extra-body
pub(super) fn merge_extra_body(body: &mut Map<String, Value>, extra: Option<&Value>) {
    match extra {
        None | Some(Value::Null) => {}
        Some(Value::Object(extra)) => {
            for (key, value) in extra {
                if CONTROLLED_KEYS.contains(&key.as_str()) {
                    tracing::warn!(key = %key, "extra_body key {key} is controlled by the request and ignored");
                } else {
                    body.insert(key.clone(), value.clone());
                }
            }
        }
        Some(_) => tracing::warn!("extra_body is not an object and is ignored"),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-merge-extra-body

/// Translates the Responses streaming events.
#[derive(Debug, Default)]
pub struct ResponsesTranslator {
    saw_annotation_event: bool,
    function_calls: Vec<FunctionCall>,
}

fn parse_usage(response: &Value) -> Option<UsageTokens> {
    let usage = response.get("usage").filter(|usage| usage.is_object())?;
    let count = |value: &Value| value.as_u64().unwrap_or(0);
    Some(UsageTokens {
        input_tokens: count(&usage["input_tokens"]),
        output_tokens: count(&usage["output_tokens"]),
        cache_read_input_tokens: count(&usage["input_tokens_details"]["cached_tokens"]),
        cache_write_input_tokens: 0,
        reasoning_tokens: count(&usage["output_tokens_details"]["reasoning_tokens"]),
    })
}

fn function_call_of(item: &Value) -> Option<FunctionCall> {
    (item["type"] == "function_call").then(|| FunctionCall {
        call_id: item["call_id"].as_str().unwrap_or_default().to_owned(),
        name: item["name"].as_str().unwrap_or_default().to_owned(),
        arguments: item["arguments"].as_str().unwrap_or_default().to_owned(),
    })
}

/// The `logs` outputs of a code interpreter item, joined and capped.
fn code_output(item: &Value) -> String {
    let logs: Vec<&str> = item["outputs"]
        .as_array()
        .map(|outputs| {
            outputs
                .iter()
                .filter(|output| output["type"] == "logs")
                .filter_map(|output| output["logs"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let joined = logs.join("\n");
    if joined.chars().count() > CODE_OUTPUT_MAX_CHARS {
        let cut: String = joined.chars().take(CODE_OUTPUT_MAX_CHARS).collect();
        format!("{cut}{TRUNCATED_SUFFIX}")
    } else {
        joined
    }
}

/// The error object of a failed response or an `error` event: the nested
/// `response.error`, then a top-level `error`, then a flat `{code, message}`.
fn parse_error(json: &Value) -> (Option<String>, String) {
    let candidate = [&json["response"]["error"], &json["error"], json]
        .into_iter()
        .find(|candidate| candidate.get("message").and_then(Value::as_str).is_some());
    match candidate {
        Some(error) => (
            error["code"].as_str().map(str::to_owned),
            error["message"].as_str().unwrap_or_default().to_owned(),
        ),
        None => (None, String::new()),
    }
}

impl ResponsesTranslator {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-completed
    fn completed(&mut self, response: &Value) -> Vec<ProviderEvent> {
        let usage = parse_usage(response);
        let mut events = Vec::new();
        let mut calls = Vec::new();
        for item in response["output"].as_array().into_iter().flatten() {
            if let Some(call) = function_call_of(item) {
                calls.push(call);
            } else if item["type"] == "message" && !self.saw_annotation_event {
                for (index, part) in item["content"].as_array().into_iter().flatten().enumerate() {
                    if part["type"] != "output_text" {
                        continue;
                    }
                    for annotation in part["annotations"].as_array().into_iter().flatten() {
                        events.push(ProviderEvent::Annotation(Annotation {
                            value: annotation.clone(),
                            item_id: item["id"].as_str().map(str::to_owned),
                            content_index: u32::try_from(index).ok(),
                            part_text: part["text"].as_str().map(str::to_owned),
                        }));
                    }
                }
            }
        }
        if calls.is_empty() {
            calls = std::mem::take(&mut self.function_calls);
        }
        if calls.is_empty() {
            events.push(ProviderEvent::Completed {
                usage,
                response_id: response["id"].as_str().map(str::to_owned),
                incomplete_reason: None,
            });
        } else {
            events.push(ProviderEvent::ToolUse { calls, usage });
        }
        events
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-completed
}

impl EventTranslator for ResponsesTranslator {
    fn translate(&mut self, event: &SseEvent) -> Vec<ProviderEvent> {
        let json: Option<Value> = serde_json::from_str(&event.data).ok();
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-event-name
        let Some(name) = super::event_name(event, json.as_ref()) else {
            return Vec::new();
        };
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-event-name
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-error-event
        if name == "error" {
            let (code, message) = match &json {
                Some(json) => {
                    let (code, message) = parse_error(json);
                    if message.is_empty() {
                        (code, event.data.clone())
                    } else {
                        (code, message)
                    }
                }
                None => (None, event.data.clone()),
            };
            return vec![ProviderEvent::ProviderError {
                code,
                message,
                usage: None,
            }];
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-error-event
        let Some(json) = json else {
            return Vec::new();
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-match-responses
        match name.as_str() {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-text-delta
            "response.output_text.delta" => match json["delta"].as_str() {
                Some(text) => vec![ProviderEvent::TextDelta {
                    text: text.to_owned(),
                    item_id: json["item_id"].as_str().map(str::to_owned),
                    content_index: json["content_index"]
                        .as_u64()
                        .and_then(|i| u32::try_from(i).ok()),
                }],
                None => Vec::new(),
            },
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-text-delta
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-fs-start
            "response.file_search_call.searching" => start(ToolName::FileSearch),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-fs-start
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-fs-done
            "response.file_search_call.completed" => vec![ProviderEvent::ToolDone {
                name: ToolName::FileSearch,
                details: json!({ "files_searched": json["results"].as_array().map_or(0, Vec::len) }),
            }],
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-fs-done
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ws-start
            "response.web_search_call.searching" => start(ToolName::WebSearch),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ws-start
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ws-done
            "response.web_search_call.completed" => vec![ProviderEvent::ToolDone {
                name: ToolName::WebSearch,
                details: json!({}),
            }],
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ws-done
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ci-start
            "response.code_interpreter_call.in_progress" => start(ToolName::CodeInterpreter),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ci-start
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ci-done
            "response.output_item.done" => {
                let item = &json["item"];
                if item["type"] == "code_interpreter_call" {
                    vec![ProviderEvent::ToolDone {
                        name: ToolName::CodeInterpreter,
                        details: json!({ "output": code_output(item) }),
                    }]
                } else {
                    if let Some(call) = function_call_of(item)
                        && !self
                            .function_calls
                            .iter()
                            .any(|known| known.call_id == call.call_id)
                    {
                        self.function_calls.push(call);
                    }
                    Vec::new()
                }
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ci-done
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-annotation
            "response.output_text.annotation.added" => {
                self.saw_annotation_event = true;
                vec![ProviderEvent::Annotation(Annotation {
                    value: json["annotation"].clone(),
                    item_id: json["item_id"].as_str().map(str::to_owned),
                    content_index: json["content_index"]
                        .as_u64()
                        .and_then(|i| u32::try_from(i).ok()),
                    part_text: None,
                })]
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-annotation
            "response.completed" => self.completed(&json["response"]),
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-incomplete
            "response.incomplete" => {
                let response = &json["response"];
                vec![ProviderEvent::Completed {
                    usage: parse_usage(response),
                    response_id: response["id"].as_str().map(str::to_owned),
                    incomplete_reason: Some(
                        response["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                    ),
                }]
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-incomplete
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-failed
            "response.failed" => {
                let (code, message) = parse_error(&json);
                vec![ProviderEvent::ProviderError {
                    code,
                    message,
                    usage: parse_usage(&json["response"]),
                }]
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ignored
            _ => Vec::new(),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-case-ignored
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-match-responses
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        Vec::new()
    }
}

fn start(name: ToolName) -> Vec<ProviderEvent> {
    vec![ProviderEvent::ToolStart {
        name,
        details: json!({}),
    }]
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod tests;
