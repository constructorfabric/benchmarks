//! `OpenAI` / Azure `OpenAI` Responses API adapter (also the base of the
//! `vLLM` Responses adapter).

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use crate::infra::llm::types::{
    ContentPart, InputItem, LlmRequest, ProviderErrorCode, ProviderEvent, ProviderFailure,
    RawCitation, ToolSpec,
};

/// Keys of `extra_body` the request controls; they are ignored.
pub const CONTROLLED_KEYS: &[&str] = &[
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

/// Cap of the code interpreter output carried by the `tool` done event.
const CODE_OUTPUT_CAP: usize = 8192;

pub fn input_json(input: &[InputItem]) -> Vec<Value> {
    input
        .iter()
        .map(|item| match item {
            InputItem::Message { role, content } => {
                let only_text = content.iter().all(|p| matches!(p, ContentPart::Text(_)));
                if only_text {
                    let text: String = content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text(t) => Some(t.as_str()),
                            ContentPart::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    json!({ "role": role.as_str(), "content": text })
                } else {
                    let parts: Vec<Value> = content
                        .iter()
                        .map(|p| match p {
                            ContentPart::Text(t) => json!({ "type": "input_text", "text": t }),
                            ContentPart::Image { file_id, .. } => {
                                json!({ "type": "input_image", "file_id": file_id })
                            }
                        })
                        .collect();
                    json!({ "role": role.as_str(), "content": parts })
                }
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }),
            InputItem::FunctionCallOutput { call_id, output } => json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }),
        })
        .collect()
}

pub fn tools_json(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| match t {
            ToolSpec::FileSearch {
                vector_store_ids,
                max_num_results,
            } => json!({
                "type": "file_search",
                "vector_store_ids": vector_store_ids,
                "max_num_results": max_num_results,
            }),
            ToolSpec::WebSearch { context_size } => json!({
                "type": "web_search",
                "search_context_size": context_size.as_str(),
            }),
            ToolSpec::CodeInterpreter { file_ids } => json!({
                "type": "code_interpreter",
                "container": { "type": "auto", "file_ids": file_ids },
            }),
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }),
        })
        .collect()
}

/// Apply sampling parameters and `extra_body` (except controlled keys).
pub fn apply_api_params(body: &mut Map<String, Value>, req: &LlmRequest, with_extra: bool) {
    let p = &req.api_params;
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
    if with_extra && let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if CONTROLLED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key is controlled by the request; ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

/// Build the Responses API request body.
pub fn build_body(req: &LlmRequest, include_tools: bool, include_metadata: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert("input".into(), Value::Array(input_json(&req.input)));
    body.insert("stream".into(), json!(req.stream));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    body.insert("store".into(), json!(false));
    body.insert("user".into(), json!(req.user));
    if include_tools && !req.tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools_json(&req.tools)));
        let has_builtin = req
            .tools
            .iter()
            .any(|t| !matches!(t, ToolSpec::Function { .. }));
        if has_builtin {
            body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
        }
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
    }
    if include_metadata {
        body.insert(
            "metadata".into(),
            json!({
                "tenant_id": req.metadata.tenant_id,
                "user_id": req.metadata.user_id,
                "chat_id": req.metadata.chat_id,
                "request_type": req.metadata.request_type,
                "feature": req.metadata.feature,
            }),
        );
    }
    if let Some(effort) = &req.api_params.reasoning_effort {
        body.insert("reasoning".into(), json!({ "effort": effort }));
    }
    if !req.api_params.stop.is_empty() {
        tracing::debug!("stop sequences are not supported by the Responses API; ignored");
    }
    apply_api_params(&mut body, req, true);
    Value::Object(body)
}

#[allow(clippy::cast_possible_truncation)] // a float token count saturates into i64
fn as_i64(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
        .or_else(|| v.as_f64().map(|f| f as i64))
        .unwrap_or(0)
}

/// Parse a Responses `usage` object; `None` when absent.
pub fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v?;
    if !u.is_object() {
        return None;
    }
    let input = u.get("input_tokens").or_else(|| u.get("prompt_tokens"));
    let output = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"));
    let cached = u
        .get("input_tokens_details")
        .or_else(|| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"));
    let reasoning = u
        .get("output_tokens_details")
        .or_else(|| u.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"));
    Some(UsageTokens {
        input_tokens: input.map_or(0, as_i64),
        output_tokens: output.map_or(0, as_i64),
        cache_read_input_tokens: cached.map_or(0, as_i64),
        cache_write_input_tokens: 0,
        reasoning_tokens: reasoning.map_or(0, as_i64),
    })
}

/// Character-offset substring (offsets count chars, not bytes).
fn char_slice(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let len = text.chars().count();
    if end > len {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

fn as_usize(v: Option<&Value>) -> Option<usize> {
    v.and_then(Value::as_u64)
        .and_then(|u| usize::try_from(u).ok())
}

/// Convert one annotation of an `output_text` part into a raw citation.
pub fn annotation_to_citation(a: &Value, part_text: &str) -> Option<RawCitation> {
    let kind = a.get("type").and_then(Value::as_str).unwrap_or("");
    let start = as_usize(a.get("start_index"));
    let end = as_usize(a.get("end_index"));
    let span = match (start, end) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    match kind {
        "url_citation" => {
            // Some payloads nest the fields under `url_citation`.
            let src = a.get("url_citation").unwrap_or(a);
            let url = src.get("url").and_then(Value::as_str)?.to_owned();
            let title = src
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let snippet = match src.get("text").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => span.map_or_else(String::new, |(s, e)| char_slice(part_text, s, e)),
            };
            Some(RawCitation::Web {
                url,
                title,
                snippet,
                span,
            })
        }
        "file_citation" | "container_file_citation" | "file_path" => {
            let file_id = a.get("file_id").and_then(Value::as_str)?.to_owned();
            Some(RawCitation::File {
                file_id,
                filename: a.get("filename").and_then(Value::as_str).map(str::to_owned),
                span,
            })
        }
        _ => None,
    }
}

/// Citations of a completed response's `output`.
pub fn citations_from_output(response: &Value) -> Vec<RawCitation> {
    let mut out = Vec::new();
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                out.extend(anns.iter().filter_map(|a| annotation_to_citation(a, text)));
            }
        }
    }
    out
}

/// Text of a completed (non-streaming) response.
pub fn output_text(response: &Value) -> String {
    if let Some(t) = response.get("output_text").and_then(Value::as_str) {
        return t.to_owned();
    }
    let mut text = String::new();
    if let Some(items) = response.get("output").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(t) = part.get("text").and_then(Value::as_str)
                    {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    text
}

/// Error message of a `response.failed` / `error` payload.
pub fn failure_from_error_payload(data: &Value, raw: &str) -> ProviderFailure {
    let err = data
        .get("response")
        .and_then(|r| r.get("error"))
        .filter(|e| e.is_object())
        .or_else(|| data.get("error").filter(|e| e.is_object()));
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| data.get("message").and_then(Value::as_str))
        .map_or_else(
            || {
                if raw.trim().is_empty() {
                    "Provider returned an error".to_owned()
                } else {
                    raw.to_owned()
                }
            },
            str::to_owned,
        );
    let mut f = ProviderFailure::new(ProviderErrorCode::ProviderError, message);
    f.usage = parse_usage(data.get("response").and_then(|r| r.get("usage")));
    f
}

/// Streaming parser of Responses SSE events.
#[derive(Debug, Default)]
pub struct ResponsesParser {
    /// Accumulated text per `(output_index, content_index)` for annotation
    /// events that arrive without a completed `output`.
    part_text: std::collections::HashMap<(u64, u64), String>,
    streamed_annotations: Vec<(Value, (u64, u64))>,
    /// When set, text inside `<think>` blocks becomes reasoning (vLLM).
    pub think_split: Option<super::think::ThinkSplitter>,
    pub done: bool,
}

impl ResponsesParser {
    #[must_use]
    pub fn new(vllm_reasoning: bool) -> Self {
        Self {
            think_split: vllm_reasoning.then(super::think::ThinkSplitter::default),
            ..Self::default()
        }
    }

    fn text_events(&mut self, delta: &str) -> Vec<ProviderEvent> {
        match &mut self.think_split {
            None => vec![ProviderEvent::TextDelta(delta.to_owned())],
            Some(split) => split.push(delta),
        }
    }

    /// Translate one SSE event (`event:` name and `data:` payload).
    pub fn push(&mut self, event_name: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return vec![];
        }
        let parsed: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        let name = match event_name {
            Some(n) if !n.is_empty() && n != "message" => n.to_owned(),
            _ => parsed
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        };
        let mut out = Vec::new();
        match name.as_str() {
            "response.output_text.delta" => {
                if let Some(d) = parsed.get("delta").and_then(Value::as_str) {
                    let key = (
                        parsed
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        parsed
                            .get("content_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    );
                    self.part_text.entry(key).or_default().push_str(d);
                    if !d.is_empty() {
                        out.extend(self.text_events(d));
                    }
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" => {
                if let Some(d) = parsed.get("delta").and_then(Value::as_str) {
                    out.push(ProviderEvent::ReasoningDelta(d.to_owned()));
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(a) = parsed.get("annotation") {
                    let key = (
                        parsed
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        parsed
                            .get("content_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    );
                    self.streamed_annotations.push((a.clone(), key));
                }
            }
            "response.file_search_call.searching" => out.push(ProviderEvent::ToolStart {
                name: "file_search".into(),
                details: json!({}),
            }),
            "response.file_search_call.completed" => {
                let n = parsed
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                out.push(ProviderEvent::ToolDone {
                    name: "file_search".into(),
                    details: json!({ "files_searched": n }),
                });
            }
            "response.web_search_call.searching" => out.push(ProviderEvent::ToolStart {
                name: "web_search".into(),
                details: json!({}),
            }),
            "response.web_search_call.completed" => out.push(ProviderEvent::ToolDone {
                name: "web_search".into(),
                details: json!({}),
            }),
            "response.code_interpreter_call.in_progress" => out.push(ProviderEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({}),
            }),
            "response.output_item.done" => {
                let item = parsed.get("item").cloned().unwrap_or(Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("code_interpreter_call") => {
                        let logs: Vec<String> = item
                            .get("outputs")
                            .and_then(Value::as_array)
                            .map(|outs| {
                                outs.iter()
                                    .filter(|o| {
                                        o.get("type").and_then(Value::as_str) == Some("logs")
                                    })
                                    .filter_map(|o| o.get("logs").and_then(Value::as_str))
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let mut output = logs.join("\n");
                        if output.chars().count() > CODE_OUTPUT_CAP {
                            output = output.chars().take(CODE_OUTPUT_CAP).collect::<String>();
                            output.push_str("...[truncated]");
                        }
                        out.push(ProviderEvent::ToolDone {
                            name: "code_interpreter".into(),
                            details: json!({ "output": output }),
                        });
                    }
                    Some("function_call") => {
                        out.push(ProviderEvent::FunctionCall {
                            call_id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            name: item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            arguments: item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_owned(),
                        });
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(split) = &mut self.think_split {
                    out.extend(split.flush());
                }
                let resp = parsed.get("response").cloned().unwrap_or(Value::Null);
                let mut citations = citations_from_output(&resp);
                if citations.is_empty() {
                    for (a, key) in &self.streamed_annotations {
                        let text = self.part_text.get(key).map_or("", String::as_str);
                        if let Some(c) = annotation_to_citation(a, text) {
                            citations.push(c);
                        }
                    }
                }
                let incomplete_reason = (name == "response.incomplete").then(|| {
                    resp.get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                out.push(ProviderEvent::Completed {
                    response_id: resp.get("id").and_then(Value::as_str).map(str::to_owned),
                    usage: parse_usage(resp.get("usage")),
                    citations,
                    incomplete_reason,
                });
                self.done = true;
            }
            "response.failed" | "error" => {
                out.push(ProviderEvent::Failed(failure_from_error_payload(
                    &parsed, data,
                )));
                self.done = true;
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod tests;
