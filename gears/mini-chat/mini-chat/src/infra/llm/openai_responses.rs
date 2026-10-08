//! `OpenAI` / Azure `OpenAI` Responses API adapter (`kind: openai_responses`).

use std::collections::HashMap;

use serde_json::{Value, json};

use super::sse_parser::SseEvent;
use super::{
    Adapter, CompletionResult, DeltaKind, LlmRequest, ProviderEvent, RawCitation, Role, ToolSpec, apply_sampling,
    merge_extra_body, parse_error_payload, parse_usage,
};

/// Maximum length of the code-interpreter output carried in a `tool` event.
const MAX_TOOL_OUTPUT: usize = 8192;

/// Responses adapter with per-stream translation state.
#[derive(Debug, Default)]
pub struct OpenAiResponses {
    /// Text of each `output_text` part, keyed by `(item_id, content_index)`.
    parts: HashMap<(String, u64), String>,
    /// Annotations received through `response.output_text.annotation.added`.
    streamed: Vec<(Value, String)>,
}

/// Build the Responses `input` array.
#[must_use]
pub fn build_input(req: &LlmRequest) -> Vec<Value> {
    let mut items: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            if m.image_file_ids.is_empty() || m.role != Role::User {
                json!({ "role": m.role.as_str(), "content": m.text })
            } else {
                let mut content = vec![json!({ "type": "input_text", "text": m.text })];
                for id in &m.image_file_ids {
                    content.push(json!({ "type": "input_image", "file_id": id }));
                }
                json!({ "role": "user", "content": content })
            }
        })
        .collect();
    for x in &req.tool_exchanges {
        items.push(json!({ "type": "function_call", "call_id": x.call_id, "name": x.name, "arguments": x.arguments }));
        items.push(json!({ "type": "function_call_output", "call_id": x.call_id, "output": x.output }));
    }
    items
}

/// Build the Responses `tools` array.
#[must_use]
pub fn build_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| match t {
            ToolSpec::FileSearch {
                vector_store_id,
                max_num_results,
            } => json!({
                "type": "file_search",
                "vector_store_ids": [vector_store_id],
                "max_num_results": max_num_results,
            }),
            ToolSpec::WebSearch { context_size } => json!({
                "type": "web_search",
                "search_context_size": context_size,
            }),
            ToolSpec::CodeInterpreter { file_ids } => json!({
                "type": "code_interpreter",
                "container": { "type": "auto", "file_ids": file_ids },
            }),
            ToolSpec::SearchKnowledge => json!({
                "type": "function",
                "name": "search_knowledge",
                "description": "Search the organization knowledge base.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "top_k": { "type": "integer" }
                    },
                    "required": ["query"]
                }
            }),
        })
        .collect()
}

fn truncate_output(s: &str) -> String {
    if s.chars().count() <= MAX_TOOL_OUTPUT {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(MAX_TOOL_OUTPUT).collect();
    out.push_str("...[truncated]");
    out
}

fn char_slice(text: &str, start: u64, end: u64) -> String {
    let (Ok(s), Ok(e)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    if s >= e || e > text.chars().count() {
        return String::new();
    }
    text.chars().skip(s).take(e - s).collect()
}

/// Map one provider annotation to a raw citation (`text` = the `output_text`
/// part that carries it).
#[must_use]
pub fn map_annotation(a: &Value, text: &str) -> Option<RawCitation> {
    let ty = a.get("type").and_then(Value::as_str).unwrap_or_default();
    let start = a.get("start_index").and_then(Value::as_u64);
    let end = a.get("end_index").and_then(Value::as_u64);
    let span = match (start, end) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    match ty {
        "url_citation" => {
            let url = a.get("url").and_then(Value::as_str)?.to_owned();
            let title = a
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(url.as_str())
                .to_owned();
            let snippet = a
                .get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| span.map(|(s, e)| char_slice(text, s, e)))
                .unwrap_or_default();
            Some(RawCitation::Web {
                url,
                title,
                snippet,
                span,
            })
        }
        "file_citation" | "container_file_citation" => {
            let file_id = a.get("file_id").and_then(Value::as_str)?.to_owned();
            Some(RawCitation::File { file_id })
        }
        _ => None,
    }
}

impl OpenAiResponses {
    fn citations_from_response(&self, response: &Value) -> Vec<RawCitation> {
        let mut out = Vec::new();
        if let Some(items) = response.get("output").and_then(Value::as_array) {
            for item in items {
                let Some(content) = item.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for part in content {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                        out.extend(anns.iter().filter_map(|a| map_annotation(a, text)));
                    }
                }
            }
        }
        if out.is_empty() {
            out = self
                .streamed
                .iter()
                .filter_map(|(a, t)| map_annotation(a, t))
                .collect();
        }
        out
    }
}

fn str_field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

impl Adapter for OpenAiResponses {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = json!({
            "model": req.provider_model_id,
            "input": build_input(req),
            "stream": req.stream,
            "store": false,
            "max_output_tokens": req.max_output_tokens,
            "max_tool_calls": req.max_tool_calls,
            "user": req.user,
            "metadata": {
                "tenant_id": req.metadata.tenant_id,
                "user_id": req.metadata.user_id,
                "chat_id": req.metadata.chat_id,
                "request_type": req.metadata.request_type,
                "feature": req.metadata.feature,
            },
        });
        if !req.instructions.is_empty() {
            body["instructions"] = Value::String(req.instructions.clone());
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(build_tools(&req.tools));
        }
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            body["include"] = json!(["code_interpreter_call.outputs"]);
        }
        if let Some(effort) = &req.api_params.reasoning_effort {
            body["reasoning"] = json!({ "effort": effort });
        }
        apply_sampling(&mut body, &req.api_params, false);
        merge_extra_body(&mut body, &req.api_params);
        body
    }

    #[allow(clippy::too_many_lines)]
    fn translate(&mut self, ev: &SseEvent) -> Vec<ProviderEvent> {
        if ev.data.trim() == "[DONE]" {
            return Vec::new();
        }
        let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
        let name = match ev.event.as_deref() {
            Some(n) if !n.is_empty() && n != "message" => n.to_owned(),
            _ => str_field(&data, "type").to_owned(),
        };
        match name.as_str() {
            "response.output_text.delta" => {
                let delta = str_field(&data, "delta").to_owned();
                let key = (
                    str_field(&data, "item_id").to_owned(),
                    data.get("content_index").and_then(Value::as_u64).unwrap_or(0),
                );
                self.parts.entry(key).or_default().push_str(&delta);
                if delta.is_empty() {
                    return Vec::new();
                }
                vec![ProviderEvent::Delta {
                    kind: DeltaKind::Text,
                    text: delta,
                }]
            }
            "response.output_text.annotation.added" => {
                let key = (
                    str_field(&data, "item_id").to_owned(),
                    data.get("content_index").and_then(Value::as_u64).unwrap_or(0),
                );
                let text = self.parts.get(&key).cloned().unwrap_or_default();
                if let Some(a) = data.get("annotation") {
                    self.streamed.push((a.clone(), text));
                }
                Vec::new()
            }
            "response.file_search_call.searching" => vec![ProviderEvent::ToolStart {
                name: "file_search".into(),
                details: json!({}),
            }],
            "response.file_search_call.completed" => {
                let n = data
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![ProviderEvent::ToolDone {
                    name: "file_search".into(),
                    details: json!({ "files_searched": n }),
                }]
            }
            "response.web_search_call.searching" => vec![ProviderEvent::ToolStart {
                name: "web_search".into(),
                details: json!({}),
            }],
            "response.web_search_call.completed" => vec![ProviderEvent::ToolDone {
                name: "web_search".into(),
                details: json!({}),
            }],
            "response.code_interpreter_call.in_progress" => vec![ProviderEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({}),
            }],
            "response.output_item.done" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                if str_field(&item, "type") == "function_call" {
                    return vec![ProviderEvent::FunctionCall {
                        call_id: str_field(&item, "call_id").to_owned(),
                        name: str_field(&item, "name").to_owned(),
                        arguments: str_field(&item, "arguments").to_owned(),
                    }];
                }
                if str_field(&item, "type") != "code_interpreter_call" {
                    return Vec::new();
                }
                let logs: Vec<String> = item
                    .get("outputs")
                    .and_then(Value::as_array)
                    .map(|outs| {
                        outs.iter()
                            .filter(|o| str_field(o, "type") == "logs")
                            .map(|o| str_field(o, "logs").to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                vec![ProviderEvent::ToolDone {
                    name: "code_interpreter".into(),
                    details: json!({ "output": truncate_output(&logs.join("\n")) }),
                }]
            }
            "response.completed" | "response.incomplete" => {
                let response = data.get("response").cloned().unwrap_or(Value::Null);
                let mut out = Vec::new();
                let citations = self.citations_from_response(&response);
                if name == "response.completed" && !citations.is_empty() {
                    out.push(ProviderEvent::Citations(citations));
                }
                let incomplete_reason = (name == "response.incomplete").then(|| {
                    response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                out.push(ProviderEvent::Completed {
                    response_id: response
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    usage: response.get("usage").and_then(parse_usage),
                    incomplete_reason,
                });
                out
            }
            "response.failed" => {
                let (code, message) = parse_error_payload(&ev.data);
                let usage = data.pointer("/response/usage").and_then(parse_usage);
                vec![ProviderEvent::Failed {
                    code,
                    message,
                    usage,
                }]
            }
            "error" => {
                let (code, message) = parse_error_payload(&ev.data);
                vec![ProviderEvent::Failed {
                    code,
                    message,
                    usage: None,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn parse_completion(&self, body: &Value) -> Result<CompletionResult, String> {
        if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
            let (_, message) = parse_error_payload(&json!({ "error": err }).to_string());
            return Err(message);
        }
        let mut text = String::new();
        if let Some(t) = body.get("output_text").and_then(Value::as_str) {
            text.push_str(t);
        } else if let Some(items) = body.get("output").and_then(Value::as_array) {
            for item in items {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if matches!(str_field(part, "type"), "output_text" | "text") {
                            text.push_str(str_field(part, "text"));
                        }
                    }
                }
            }
        }
        Ok(CompletionResult {
            text,
            usage: body.get("usage").and_then(parse_usage),
        })
    }
}
