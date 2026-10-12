//! The `OpenAI` and Azure `OpenAI` Responses API adapter (`openai_responses`).

use std::collections::HashMap;

use mini_chat_sdk::TokenUsage;
use serde_json::{Map, Value, json};

use super::adapter::{Adapter, EventParser};
use super::common::{
    failure_message, insert_sampling, merge_extra_body, normalize_incomplete_reason, parse_usage,
};
use crate::domain::llm::{
    Completion, InputMessage, InputRole, LlmCompletion, LlmEvent, LlmRequest, ProviderError,
    RawCitation, ToolSpec,
};

/// Maximum characters of the `output` of a code interpreter call sent to the client.
const MAX_CODE_OUTPUT_CHARS: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// The Responses API adapter.
pub struct OpenAiResponses;

/// The input array of a request: history and the current message, oldest first.
pub(super) fn input_items(messages: &[InputMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let role = match message.role {
                InputRole::User => "user",
                InputRole::Assistant => "assistant",
            };
            if message.image_file_ids.is_empty() {
                json!({ "role": role, "content": message.text })
            } else {
                let mut parts = vec![json!({ "type": "input_text", "text": message.text })];
                parts.extend(
                    message
                        .image_file_ids
                        .iter()
                        .map(|file_id| json!({ "type": "input_image", "file_id": file_id })),
                );
                json!({ "role": role, "content": parts })
            }
        })
        .collect()
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
        ToolSpec::WebSearch { context_size } => {
            json!({ "type": "web_search", "search_context_size": context_size })
        }
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": { "type": "auto", "file_ids": file_ids },
        }),
    }
}

/// The request body shared by the Responses adapters; `with_tools` and `with_metadata` differ per
/// adapter.
pub(super) fn build_responses_body(
    request: &LlmRequest,
    stream: bool,
    with_tools: bool,
    with_metadata: bool,
) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::from(request.model.clone()));
    body.insert("stream".to_owned(), Value::from(stream));
    body.insert(
        "max_output_tokens".to_owned(),
        Value::from(request.max_output_tokens),
    );
    if !request.system_prompt.is_empty() {
        body.insert(
            "instructions".to_owned(),
            Value::from(request.system_prompt.clone()),
        );
    }
    body.insert(
        "input".to_owned(),
        Value::Array(input_items(&request.input)),
    );
    body.insert(
        "user".to_owned(),
        Value::from(request.identity.provider_user()),
    );
    if with_metadata {
        body.insert(
            "metadata".to_owned(),
            json!({
                "tenant_id": request.identity.tenant_id.to_string(),
                "user_id": request.identity.user_id.to_string(),
                "chat_id": request.identity.chat_id.to_string(),
                "request_type": request.request_type.as_str(),
                "feature": request.feature,
            }),
        );
    }
    if with_tools && !request.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(request.tools.iter().map(tool_json).collect()),
        );
        if let Some(max_calls) = request.max_tool_calls {
            body.insert("max_tool_calls".to_owned(), Value::from(max_calls));
        }
        if request
            .tools
            .iter()
            .any(|tool| matches!(tool, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert(
                "include".to_owned(),
                json!(["code_interpreter_call.outputs"]),
            );
        }
    }
    insert_sampling(&mut body, &request.api_params);
    if let Some(effort) = &request.api_params.reasoning_effort {
        body.insert("reasoning".to_owned(), json!({ "effort": effort }));
    }
    merge_extra_body(&mut body, &request.api_params);
    Value::Object(body)
}

impl Adapter for OpenAiResponses {
    fn build_body(&self, request: &LlmRequest, stream: bool) -> Result<Value, ProviderError> {
        Ok(build_responses_body(request, stream, true, true))
    }

    fn parser(&self) -> Box<dyn EventParser> {
        Box::new(ResponsesParser::default())
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderError> {
        parse_responses_completion(body)
    }
}

/// The answer text of a non-streaming Responses body: every `output_text` part of every message
/// item.
pub(super) fn parse_responses_completion(body: &Value) -> Result<LlmCompletion, ProviderError> {
    let mut text = String::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if part.get("type").and_then(Value::as_str) == Some("output_text")
                && let Some(chunk) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(chunk);
            }
        }
    }
    if text.is_empty() && body.get("output").is_none() {
        return Err(ProviderError::provider(
            "the provider response has no output",
        ));
    }
    Ok(LlmCompletion {
        text,
        usage: body.get("usage").map(responses_usage),
    })
}

pub(super) fn responses_usage(usage: &Value) -> TokenUsage {
    parse_usage(
        usage,
        "input_tokens",
        "output_tokens",
        "input_tokens_details",
        "output_tokens_details",
    )
}

/// Translates the events of a Responses API stream (`OpenAI`, Azure `OpenAI` and `vLLM`).
#[derive(Default)]
pub struct ResponsesParser {
    response_id: Option<String>,
    /// Text of the answer parts, keyed by `(item_id, content_index)`, for annotation snippets.
    parts: HashMap<(String, i64), String>,
    /// Annotations seen as `annotation.added` events: `(item_id, content_index, annotation)`.
    streamed_annotations: Vec<(String, i64, Value)>,
}

fn event_name<'a>(event: Option<&'a str>, json: &'a Value) -> Option<&'a str> {
    match event {
        Some(name) if name != "message" && !name.is_empty() => Some(name),
        _ => json.get("type").and_then(Value::as_str),
    }
}

/// `chars[start..end]` of `text`; empty when the range is outside the text.
fn char_range(text: &str, start: u64, end: u64) -> String {
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    if start >= end {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if end > chars.len() {
        return String::new();
    }
    chars[start..end].iter().collect()
}

fn citation_of(annotation: &Value, part_text: &str) -> Option<RawCitation> {
    match annotation.get("type").and_then(Value::as_str)? {
        "url_citation" => {
            let start = annotation.get("start_index").and_then(Value::as_u64);
            let end = annotation.get("end_index").and_then(Value::as_u64);
            Some(RawCitation::Web {
                url: annotation.get("url")?.as_str()?.to_owned(),
                title: annotation
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                snippet: start
                    .zip(end)
                    .map(|(start, end)| char_range(part_text, start, end))
                    .unwrap_or_default(),
                start,
                end,
            })
        }
        "file_citation" => Some(RawCitation::File {
            provider_file_id: annotation.get("file_id")?.as_str()?.to_owned(),
            filename: annotation
                .get("filename")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        _ => None,
    }
}

impl ResponsesParser {
    fn part_key(json: &Value) -> (String, i64) {
        (
            json.get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            json.get("content_index")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        )
    }

    /// The citations of a finished response: the annotations of the final output when it carries
    /// them, otherwise the ones streamed as `annotation.added` events.
    fn citations(&self, response: &Value) -> Vec<RawCitation> {
        fn push(found: &mut Vec<RawCitation>, citation: RawCitation) {
            if !found.contains(&citation) {
                found.push(citation);
            }
        }
        let mut found: Vec<RawCitation> = Vec::new();
        for item in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                for annotation in part
                    .get("annotations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(citation) = citation_of(annotation, text) {
                        push(&mut found, citation);
                    }
                }
            }
        }
        if found.is_empty() {
            for (item_id, index, annotation) in &self.streamed_annotations {
                let text = self
                    .parts
                    .get(&(item_id.clone(), *index))
                    .map_or("", String::as_str);
                if let Some(citation) = citation_of(annotation, text) {
                    push(&mut found, citation);
                }
            }
        }
        found
    }

    fn code_interpreter_output(item: &Value) -> String {
        let logs: Vec<&str> = item
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|output| output.get("type").and_then(Value::as_str) == Some("logs"))
            .filter_map(|output| output.get("logs").and_then(Value::as_str))
            .collect();
        let joined = logs.join("\n");
        if joined.chars().count() > MAX_CODE_OUTPUT_CHARS {
            let cut: String = joined.chars().take(MAX_CODE_OUTPUT_CHARS).collect();
            format!("{cut}{TRUNCATED_SUFFIX}")
        } else {
            joined
        }
    }
}

impl EventParser for ResponsesParser {
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per provider event of the translation table"
    )]
    fn push(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            return match event {
                Some("error") => vec![LlmEvent::Failed {
                    message: failure_message(data),
                    usage: None,
                }],
                _ => Vec::new(),
            };
        };
        let Some(name) = event_name(event, &json) else {
            return Vec::new();
        };
        match name {
            "response.created" => {
                self.response_id = json
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Vec::new()
            }
            "response.output_text.delta" => {
                let delta = json
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.parts
                    .entry(Self::part_key(&json))
                    .or_default()
                    .push_str(delta);
                if delta.is_empty() {
                    Vec::new()
                } else {
                    vec![LlmEvent::TextDelta(delta.to_owned())]
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(annotation) = json.get("annotation") {
                    let (item, index) = Self::part_key(&json);
                    self.streamed_annotations
                        .push((item, index, annotation.clone()));
                }
                Vec::new()
            }
            "response.file_search_call.searching" => vec![started("file_search")],
            "response.file_search_call.completed" => {
                let searched = json
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![LlmEvent::ToolDone {
                    name: "file_search".to_owned(),
                    details: json!({ "files_searched": searched }),
                }]
            }
            "response.web_search_call.searching" => vec![started("web_search")],
            "response.web_search_call.completed" => vec![LlmEvent::ToolDone {
                name: "web_search".to_owned(),
                details: json!({}),
            }],
            "response.code_interpreter_call.in_progress" => vec![started("code_interpreter")],
            "response.output_item.done" => {
                let item = json.get("item");
                match item
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("code_interpreter_call") => vec![LlmEvent::ToolDone {
                        name: "code_interpreter".to_owned(),
                        details: json!({
                            "output": item.map(Self::code_interpreter_output).unwrap_or_default()
                        }),
                    }],
                    _ => Vec::new(),
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = json.get("response").unwrap_or(&Value::Null);
                let completion = if name == "response.incomplete" {
                    Completion::Incomplete {
                        reason: normalize_incomplete_reason(
                            response
                                .pointer("/incomplete_details/reason")
                                .and_then(Value::as_str),
                        ),
                    }
                } else {
                    Completion::Complete
                };
                vec![LlmEvent::Completed {
                    completion,
                    response_id: response
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| self.response_id.clone()),
                    usage: response
                        .get("usage")
                        .map(super::openai_responses::responses_usage),
                    citations: self.citations(response),
                }]
            }
            "response.failed" => {
                let usage = json
                    .pointer("/response/usage")
                    .filter(|usage| usage.is_object())
                    .map(responses_usage);
                vec![LlmEvent::Failed {
                    message: failure_message(data),
                    usage,
                }]
            }
            "error" => vec![LlmEvent::Failed {
                message: failure_message(data),
                usage: None,
            }],
            _ => Vec::new(),
        }
    }
}

fn started(name: &str) -> LlmEvent {
    LlmEvent::ToolStarted {
        name: name.to_owned(),
    }
}
