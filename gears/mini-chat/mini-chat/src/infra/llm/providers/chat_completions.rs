//! The `openai_chat_completions` adapter: the request body of the Chat
//! Completions API (`cpt-cf-mini-chat-dod-streaming-turns-other-adapters`) and
//! the translation of its chunk stream.

use std::collections::BTreeMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::openai_responses::merge_extra_body;
use super::{
    EventTranslator, ProviderAdapter, SEARCH_KNOWLEDGE_DESCRIPTION, base_headers, request_uri,
    search_knowledge_schema, user_value,
};
use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{BuiltRequest, HistoryRole, ToolCallOutput};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::tools::ToolSpec;
use crate::domain::service::stream::types::{FunctionCall, ProviderEvent, ToolName};
use crate::infra::llm::sse_parse::SseEvent;

/// The Chat Completions adapter.
#[derive(Debug, Default)]
pub struct ChatCompletionsAdapter;

impl ChatCompletionsAdapter {
    /// Creates the adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ProviderAdapter for ChatCompletionsAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenaiChatCompletions
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-chat-completions
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        let entry = input.entry;
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(entry.provider_model_id));
        body.insert("messages".to_owned(), Value::Array(messages(input)));
        let tools: Vec<Value> = input
            .tools
            .tools
            .iter()
            .filter_map(|tool| match tool {
                ToolSpec::SearchKnowledge => Some(json!({
                    "type": "function",
                    "function": {
                        "name": "search_knowledge",
                        "description": SEARCH_KNOWLEDGE_DESCRIPTION,
                        "parameters": search_knowledge_schema(),
                    },
                })),
                _ => None,
            })
            .collect();
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        body.insert("stream".to_owned(), json!(true));
        body.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );
        body.insert(
            "max_completion_tokens".to_owned(),
            json!(input.max_output_tokens_applied),
        );
        let params = &entry.general_config.api_params;
        if !params.stop.is_empty() {
            body.insert("stop".to_owned(), json!(params.stop));
        }
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
            body.insert("reasoning_effort".to_owned(), json!(effort));
        }
        body.insert("user".to_owned(), json!(user_value(input)));
        merge_extra_body(&mut body, params.extra_body.as_ref());
        Ok(BuiltRequest {
            kind: ProviderKind::OpenaiChatCompletions,
            uri: request_uri(input),
            headers: base_headers(),
            body: Value::Object(body),
        })
    }

    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-chat-completions
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
        let calls: Vec<Value> = outputs
            .iter()
            .map(|output| {
                json!({
                    "id": output.call.call_id,
                    "type": "function",
                    "function": { "name": output.call.name, "arguments": output.call.arguments },
                })
            })
            .collect();
        messages.push(json!({ "role": "assistant", "content": null, "tool_calls": calls }));
        for output in outputs {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": output.call.call_id,
                "content": output.output,
            }));
        }
        Ok(next)
    }

    fn translator(&self) -> Box<dyn EventTranslator> {
        Box::new(ChatTranslator::default())
    }
}

/// The messages: the system prompt, the summary, the history and the current
/// message, all with string content (images are not sent).
fn messages(input: &ProviderRequestInput<'_>) -> Vec<Value> {
    let plan = input.plan;
    let mut messages = Vec::new();
    if !plan.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": plan.instructions }));
    }
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
        messages.push(json!({ "role": "user", "content": current.content }));
    }
    messages
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-chat-completions
/// A tool call being accumulated from its fragments.
#[derive(Debug, Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// The outcome the finish reason marked, released with the usage.
#[derive(Debug)]
enum Marked {
    Completed(Option<String>),
    ToolUse(Vec<FunctionCall>),
}

/// Translates the Chat Completions chunks.
#[derive(Debug, Default)]
pub struct ChatTranslator {
    calls: BTreeMap<u64, PendingCall>,
    marked: Option<Marked>,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
}

fn parse_usage(usage: &Value) -> Option<UsageTokens> {
    usage.is_object().then(|| {
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        UsageTokens {
            input_tokens: count(&usage["prompt_tokens"]),
            output_tokens: count(&usage["completion_tokens"]),
            cache_read_input_tokens: count(&usage["prompt_tokens_details"]["cached_tokens"]),
            cache_write_input_tokens: 0,
            reasoning_tokens: count(&usage["completion_tokens_details"]["reasoning_tokens"]),
        }
    })
}

impl ChatTranslator {
    /// Releases the marked outcome when the usage is known, or when `force`.
    fn release(&mut self, force: bool) -> Vec<ProviderEvent> {
        if self.marked.is_none() || (self.usage.is_none() && !force) {
            return Vec::new();
        }
        let usage = self.usage;
        match self.marked.take() {
            Some(Marked::Completed(reason)) => vec![ProviderEvent::Completed {
                usage,
                response_id: self.response_id.clone(),
                incomplete_reason: reason,
            }],
            Some(Marked::ToolUse(calls)) => vec![ProviderEvent::ToolUse { calls, usage }],
            None => Vec::new(),
        }
    }

    fn fragments(&mut self, calls: &[Value], events: &mut Vec<ProviderEvent>) {
        for fragment in calls {
            let Some(index) = fragment["index"].as_u64() else {
                continue;
            };
            let function = &fragment["function"];
            let known = self.calls.contains_key(&index);
            let call = self.calls.entry(index).or_default();
            if call.call_id.is_empty() {
                fragment["id"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut call.call_id);
            }
            if call.name.is_empty() {
                function["name"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut call.name);
            }
            if let Some(arguments) = function["arguments"].as_str() {
                call.arguments.push_str(arguments);
            }
            if !known {
                events.push(ProviderEvent::ToolStart {
                    name: ToolName::FunctionCall,
                    details: json!({ "index": index, "call_id": call.call_id, "name": call.name }),
                });
            }
        }
    }

    fn finish_reason(&mut self, reason: &str, events: &mut Vec<ProviderEvent>) {
        if self.marked.is_some() {
            return;
        }
        if reason == "tool_calls" && !self.calls.is_empty() {
            let calls: Vec<FunctionCall> = std::mem::take(&mut self.calls)
                .into_values()
                .map(|call| FunctionCall {
                    call_id: call.call_id,
                    name: call.name,
                    arguments: call.arguments,
                })
                .collect();
            for call in &calls {
                events.push(ProviderEvent::ToolDone {
                    name: ToolName::FunctionCall,
                    details: json!({
                        "call_id": call.call_id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }),
                });
            }
            self.marked = Some(Marked::ToolUse(calls));
        } else {
            let reason = match reason {
                "stop" | "tool_calls" => None,
                "length" => Some("max_tokens".to_owned()),
                other => Some(other.to_owned()),
            };
            self.marked = Some(Marked::Completed(reason));
        }
    }
}

impl EventTranslator for ChatTranslator {
    fn translate(&mut self, event: &SseEvent) -> Vec<ProviderEvent> {
        if event.data.trim() == "[DONE]" {
            return self.release(true);
        }
        let Ok(json) = serde_json::from_str::<Value>(&event.data) else {
            return Vec::new();
        };
        if let Some(error) = json.get("error").filter(|error| error.is_object()) {
            return vec![ProviderEvent::ProviderError {
                code: error["code"]
                    .as_str()
                    .or_else(|| error["type"].as_str())
                    .map(str::to_owned),
                message: error["message"].as_str().unwrap_or_default().to_owned(),
                usage: None,
            }];
        }
        if let Some(id) = json["id"].as_str() {
            self.response_id = Some(id.to_owned());
        }
        if let Some(usage) = parse_usage(&json["usage"]) {
            self.usage = Some(usage);
        }
        let mut events = Vec::new();
        let choice = &json["choices"][0];
        if let Some(content) = choice["delta"]["content"]
            .as_str()
            .filter(|c| !c.is_empty())
        {
            events.push(ProviderEvent::TextDelta {
                text: content.to_owned(),
                item_id: None,
                content_index: None,
            });
        }
        if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
            self.fragments(calls, &mut events);
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason(reason, &mut events);
        }
        events.extend(self.release(false));
        events
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        self.release(true)
    }
}

// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-chat-completions
#[cfg(test)]
#[path = "chat_completions_tests.rs"]
mod tests;
