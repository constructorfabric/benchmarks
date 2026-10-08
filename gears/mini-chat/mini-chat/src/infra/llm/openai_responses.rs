//! `OpenAI` / Azure `OpenAI` Responses API adapter (`openai_responses`), also the
//! base of the vLLM Responses adapter.

use serde_json::{Map, Value, json};

use super::types::{
    LlmMessage, LlmPart, LlmRequest, LlmRole, LlmTool, ProviderEvent, ProviderFailureKind,
    ProviderUsage, RawCitation,
};
use crate::domain::sanitize::sanitize_provider_message;

/// Request-controlled keys that `extra_body` may not override.
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

/// Maximum characters of a code interpreter `tool` done output.
pub const CODE_OUTPUT_CAP: usize = 8192;

/// Adapter options (vLLM drops tools and metadata).
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools, reason = "independent adapter feature toggles")]
pub struct ResponsesFlavor {
    pub send_tools: bool,
    pub send_metadata: bool,
    pub send_max_tool_calls: bool,
}

impl ResponsesFlavor {
    pub const OPENAI: Self = Self { send_tools: true, send_metadata: true, send_max_tool_calls: true };
    pub const VLLM: Self = Self { send_tools: false, send_metadata: false, send_max_tool_calls: false };
}

fn message_item(m: &LlmMessage) -> Value {
    match m.role {
        LlmRole::User => {
            let content: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    LlmPart::Text(t) => json!({"type": "input_text", "text": t}),
                    LlmPart::Image { file_id, .. } => json!({"type": "input_image", "file_id": file_id}),
                })
                .collect();
            json!({"role": "user", "content": content})
        }
        LlmRole::Assistant => json!({"role": "assistant", "content": m.joined_text()}),
    }
}

fn tool_item(t: &LlmTool) -> Value {
    match t {
        LlmTool::FileSearch { vector_store_ids, max_num_results } => {
            let mut v = json!({"type": "file_search", "vector_store_ids": vector_store_ids});
            if *max_num_results > 0 {
                v["max_num_results"] = json!(max_num_results);
            }
            v
        }
        LlmTool::WebSearch { context_size } => {
            json!({"type": "web_search", "search_context_size": context_size.as_str()})
        }
        LlmTool::CodeInterpreter { file_ids } => {
            json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": file_ids}})
        }
        LlmTool::Function { name, description, parameters } => json!({
            "type": "function",
            "name": name,
            "description": description,
            "parameters": parameters,
        }),
    }
}

/// Build the Responses API request body.
#[must_use]
pub fn build_body(req: &LlmRequest, flavor: ResponsesFlavor) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.provider_model_id));
    if !req.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(req.instructions));
    }
    let mut input: Vec<Value> = req.messages.iter().map(message_item).collect();
    input.extend(req.extra_input.iter().cloned());
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), json!(req.stream));
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    let tools: Vec<&LlmTool> = if flavor.send_tools { req.tools.iter().collect() } else { Vec::new() };
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools.iter().map(|t| tool_item(t)).collect()));
        if tools.iter().any(|t| matches!(t, LlmTool::CodeInterpreter { .. })) {
            body.insert("include".to_owned(), json!(["code_interpreter_call.outputs"]));
        }
    }
    if flavor.send_max_tool_calls && req.max_tool_calls > 0 {
        body.insert("max_tool_calls".to_owned(), json!(req.max_tool_calls));
    }
    let p = &req.api_params;
    if let Some(v) = p.temperature {
        body.insert("temperature".to_owned(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".to_owned(), json!(v));
    }
    if let Some(v) = p.frequency_penalty {
        body.insert("frequency_penalty".to_owned(), json!(v));
    }
    if let Some(v) = p.presence_penalty {
        body.insert("presence_penalty".to_owned(), json!(v));
    }
    if let Some(effort) = &p.reasoning_effort {
        body.insert("reasoning".to_owned(), json!({"effort": effort}));
    }
    body.insert("user".to_owned(), json!(req.user));
    if flavor.send_metadata {
        body.insert(
            "metadata".to_owned(),
            json!({
                "tenant_id": req.metadata.tenant_id,
                "user_id": req.metadata.user_id,
                "chat_id": req.metadata.chat_id,
                "request_type": req.metadata.request_type,
                "feature": req.metadata.feature,
            }),
        );
    }
    merge_extra_body(&mut body, p.extra_body.as_ref());
    Value::Object(body)
}

/// Merge `extra_body` keys into the top level, ignoring controlled keys.
pub fn merge_extra_body(body: &mut Map<String, Value>, extra: Option<&Map<String, Value>>) {
    if let Some(extra) = extra {
        for (k, v) in extra {
            if CONTROLLED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

/// Parse a Responses API usage object.
#[must_use]
pub fn parse_usage(v: &Value) -> Option<ProviderUsage> {
    let u = v.as_object()?;
    let num = |o: Option<&Value>| o.and_then(Value::as_i64).unwrap_or(0);
    Some(ProviderUsage {
        input_tokens: num(u.get("input_tokens").or_else(|| u.get("prompt_tokens"))),
        output_tokens: num(u.get("output_tokens").or_else(|| u.get("completion_tokens"))),
        cache_read_input_tokens: num(
            u.get("input_tokens_details")
                .or_else(|| u.get("prompt_tokens_details"))
                .and_then(|d| d.get("cached_tokens")),
        ),
        cache_write_input_tokens: 0,
        reasoning_tokens: num(
            u.get("output_tokens_details")
                .or_else(|| u.get("completion_tokens_details"))
                .and_then(|d| d.get("reasoning_tokens")),
        ),
    })
}

/// Extract an error `(code, message)` from a provider error payload:
/// `response.error`, then a top-level `error`, then flat `{code, message}`.
#[must_use]
pub fn extract_error(data: &Value) -> (Option<String>, Option<String>) {
    let pick = |e: &Value| {
        let code = e.get("code").and_then(|c| match c {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        let message = e.get("message").and_then(Value::as_str).map(str::to_owned);
        (code, message)
    };
    if let Some(e) = data.get("response").and_then(|r| r.get("error")).filter(|e| e.is_object()) {
        return pick(e);
    }
    if let Some(e) = data.get("error").filter(|e| e.is_object()) {
        return pick(e);
    }
    if data.get("message").is_some() || data.get("code").is_some() {
        return pick(data);
    }
    (None, None)
}

fn char_slice(text: &str, start: u64, end: u64) -> String {
    let (Ok(s), Ok(e)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    if s >= e {
        return String::new();
    }
    let count = text.chars().count();
    if e > count {
        return String::new();
    }
    text.chars().skip(s).take(e - s).collect()
}

/// Convert one annotation object into a raw citation; `part_text` is the
/// text of the `output_text` part carrying it.
#[must_use]
pub fn annotation_to_citation(a: &Value, part_text: &str) -> Option<RawCitation> {
    let ty = a.get("type").and_then(Value::as_str).unwrap_or_default();
    match ty {
        "url_citation" => {
            let url = a.get("url").and_then(Value::as_str)?.to_owned();
            let title = a.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
            let start = a.get("start_index").and_then(Value::as_u64);
            let end = a.get("end_index").and_then(Value::as_u64);
            let span = match (start, end) {
                (Some(s), Some(e)) => Some((s, e)),
                _ => None,
            };
            let snippet = a
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| span.map(|(s, e)| char_slice(part_text, s, e)))
                .unwrap_or_default();
            Some(RawCitation::Web { url, title, snippet, span })
        }
        "file_citation" => Some(RawCitation::File {
            file_id: a.get("file_id").and_then(Value::as_str)?.to_owned(),
            filename: a.get("filename").and_then(Value::as_str).map(str::to_owned),
        }),
        _ => None,
    }
}

fn citations_from_output(response: &Value) -> (Vec<RawCitation>, Option<String>) {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut any_text = false;
    if let Some(items) = response.get("output").and_then(Value::as_array) {
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
                let part_text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                any_text = true;
                text.push_str(part_text);
                if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                    out.extend(anns.iter().filter_map(|a| annotation_to_citation(a, part_text)));
                }
            }
        }
    }
    (out, any_text.then_some(text))
}

fn code_interpreter_output(item: &Value) -> String {
    let mut logs: Vec<&str> = Vec::new();
    if let Some(outputs) = item.get("outputs").and_then(Value::as_array) {
        for o in outputs {
            if o.get("type").and_then(Value::as_str) == Some("logs")
                && let Some(l) = o.get("logs").and_then(Value::as_str)
            {
                logs.push(l);
            }
        }
    }
    let joined = logs.join("\n");
    if joined.chars().count() > CODE_OUTPUT_CAP {
        let mut s: String = joined.chars().take(CODE_OUTPUT_CAP).collect();
        s.push_str("...[truncated]");
        s
    } else {
        joined
    }
}

/// Incremental translator of Responses API SSE events.
#[derive(Debug, Default)]
pub struct ResponsesParser {
    /// Annotations seen via `response.output_text.annotation.added`.
    streamed_annotations: Vec<Value>,
    /// Accumulated text (for annotation snippets).
    text: String,
    response_id: Option<String>,
    /// Emit `<think>` blocks as reasoning (vLLM).
    pub split_think: bool,
    in_think: bool,
}

impl ResponsesParser {
    #[must_use]
    pub fn new(split_think: bool) -> Self {
        Self { split_think, ..Self::default() }
    }

    fn think_split(&mut self, delta: &str, out: &mut Vec<ProviderEvent>) {
        let mut rest = delta;
        while !rest.is_empty() {
            if self.in_think {
                if let Some(i) = rest.find("</think>") {
                    if i > 0 {
                        out.push(ProviderEvent::ReasoningDelta(rest[..i].to_owned()));
                    }
                    self.in_think = false;
                    rest = &rest[i + "</think>".len()..];
                } else {
                    out.push(ProviderEvent::ReasoningDelta(rest.to_owned()));
                    rest = "";
                }
            } else if let Some(i) = rest.find("<think>") {
                if i > 0 {
                    self.text.push_str(&rest[..i]);
                    out.push(ProviderEvent::TextDelta(rest[..i].to_owned()));
                }
                self.in_think = true;
                rest = &rest[i + "<think>".len()..];
            } else {
                self.text.push_str(rest);
                out.push(ProviderEvent::TextDelta(rest.to_owned()));
                rest = "";
            }
        }
    }

    /// Translate one provider event (`name` from the `event:` line, or the
    /// `type` of the data when missing / `message`).
    #[allow(clippy::too_many_lines, reason = "flat event translation table")]
    pub fn on_event(&mut self, name: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let event_name: String = match name {
            Some(n) if !n.is_empty() && n != "message" => n.to_owned(),
            _ => parsed
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let mut out = Vec::new();
        if event_name == "error" {
            let (code, message) = parsed.as_ref().map_or((None, None), extract_error);
            let message = message.unwrap_or_else(|| data.to_owned());
            out.push(ProviderEvent::Failed {
                kind: ProviderFailureKind::ProviderError,
                message: sanitize_provider_message(&message),
                provider_code: code,
                usage: None,
                response_id: self.response_id.clone(),
            });
            return out;
        }
        let Some(v) = parsed else {
            return out;
        };
        match event_name.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(id) = v.get("response").and_then(|r| r.get("id")).and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
            }
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(Value::as_str) {
                    if self.split_think {
                        self.think_split(d, &mut out);
                    } else {
                        self.text.push_str(d);
                        out.push(ProviderEvent::TextDelta(d.to_owned()));
                    }
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" if self.split_think => {
                if let Some(d) = v.get("delta").and_then(Value::as_str) {
                    out.push(ProviderEvent::ReasoningDelta(d.to_owned()));
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(a) = v.get("annotation") {
                    self.streamed_annotations.push(a.clone());
                }
            }
            "response.file_search_call.searching" => out.push(ProviderEvent::ToolStart {
                name: "file_search".to_owned(),
                details: json!({}),
            }),
            "response.file_search_call.completed" => {
                let n = v.get("results").and_then(Value::as_array).map_or(0, Vec::len);
                out.push(ProviderEvent::ToolDone {
                    name: "file_search".to_owned(),
                    details: json!({"files_searched": n}),
                });
            }
            "response.web_search_call.searching" => out.push(ProviderEvent::ToolStart {
                name: "web_search".to_owned(),
                details: json!({}),
            }),
            "response.web_search_call.completed" => out.push(ProviderEvent::ToolDone {
                name: "web_search".to_owned(),
                details: json!({}),
            }),
            "response.code_interpreter_call.in_progress" => out.push(ProviderEvent::ToolStart {
                name: "code_interpreter".to_owned(),
                details: json!({}),
            }),
            "response.output_item.done" => {
                let item = v.get("item").cloned().unwrap_or(Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("code_interpreter_call") => out.push(ProviderEvent::ToolDone {
                        name: "code_interpreter".to_owned(),
                        details: json!({"output": code_interpreter_output(&item)}),
                    }),
                    Some("function_call") => out.push(ProviderEvent::FunctionCall {
                        call_id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        raw_item: item.clone(),
                    }),
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = v.get("response").cloned().unwrap_or(Value::Null);
                let usage = response.get("usage").and_then(parse_usage);
                let response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| self.response_id.clone());
                let incomplete_reason = if event_name == "response.incomplete" {
                    Some(
                        response
                            .get("incomplete_details")
                            .and_then(|d| d.get("reason"))
                            .and_then(Value::as_str)
                            .unwrap_or("other")
                            .to_owned(),
                    )
                } else {
                    None
                };
                let (mut citations, output_text) = citations_from_output(&response);
                if citations.is_empty() {
                    let text = self.text.clone();
                    citations = self
                        .streamed_annotations
                        .iter()
                        .filter_map(|a| annotation_to_citation(a, &text))
                        .collect();
                }
                if incomplete_reason.is_some() {
                    citations.clear();
                }
                out.push(ProviderEvent::Completed {
                    usage,
                    response_id,
                    incomplete_reason,
                    citations,
                    output_text,
                });
            }
            "response.failed" => {
                let (code, message) = extract_error(&v);
                let usage = v.get("response").and_then(|r| r.get("usage")).and_then(parse_usage);
                out.push(ProviderEvent::Failed {
                    kind: ProviderFailureKind::ProviderError,
                    message: sanitize_provider_message(
                        message.as_deref().unwrap_or("Provider returned an error"),
                    ),
                    provider_code: code,
                    usage,
                    response_id: v
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| self.response_id.clone()),
                });
            }
            _ => {}
        }
        out
    }
}

/// Parse a non-streaming Responses API JSON body (thread summary call):
/// returns `(output_text, usage)`.
#[must_use]
pub fn parse_non_streaming(v: &Value) -> (String, Option<ProviderUsage>) {
    let (_, text) = citations_from_output(v);
    let text = text
        .or_else(|| v.get("output_text").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    (text, v.get("usage").and_then(parse_usage))
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod openai_responses_tests;
