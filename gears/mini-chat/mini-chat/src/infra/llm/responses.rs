//! `OpenAI` / Azure `OpenAI` Responses adapter (also used by vLLM Responses).

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::{
    ContentPart, InputMessage, LlmRequest, ProviderError, ProviderErrorCode, ProviderEvent,
    RawCitation, ToolSpec,
};
use crate::domain::sanitize::sanitize_provider_message;

/// Keys the request controls; `extra_body` entries with these names are ignored.
pub const RESERVED_KEYS: &[&str] = &[
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

/// Maximum characters of a code interpreter output in a `tool` event.
pub const CODE_OUTPUT_LIMIT: usize = 8192;

/// Flavor of the Responses API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// `OpenAI` / Azure `OpenAI`.
    OpenAi,
    /// vLLM: no tools, no metadata, `<think>` reasoning.
    Vllm,
}

fn input_json(msg: &InputMessage) -> Value {
    let only_text = msg.parts.iter().all(|p| matches!(p, ContentPart::Text(_)));
    if only_text {
        let text: Vec<&str> = msg
            .parts
            .iter()
            .filter_map(|p| if let ContentPart::Text(t) = p { Some(t.as_str()) } else { None })
            .collect();
        return json!({"role": msg.role.as_str(), "content": text.join("\n")});
    }
    let parts: Vec<Value> = msg
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => json!({"type": "input_text", "text": t}),
            ContentPart::Image { file_id, .. } => json!({"type": "input_image", "file_id": file_id}),
        })
        .collect();
    json!({"role": msg.role.as_str(), "content": parts})
}

/// Inserts the optional catalog parameters and `extra_body`.
pub fn apply_api_params(body: &mut Map<String, Value>, req: &LlmRequest) {
    let p = &req.api_params;
    for (k, v) in [
        ("temperature", p.temperature),
        ("top_p", p.top_p),
        ("frequency_penalty", p.frequency_penalty),
        ("presence_penalty", p.presence_penalty),
    ] {
        if let Some(v) = v {
            body.insert(k.to_owned(), json!(v));
        }
    }
    if let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if RESERVED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "ignoring reserved key in api_params.extra_body");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

/// Builds the Responses request body.
#[must_use]
pub fn build_request(req: &LlmRequest, flavor: Flavor) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert("input".into(), Value::Array(req.input.iter().map(input_json).collect()));
    body.insert("stream".into(), json!(req.stream));
    body.insert("store".into(), json!(false));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    if flavor == Flavor::OpenAi && !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| match t {
                ToolSpec::FileSearch { vector_store_ids, max_num_results } => json!({
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
                    "container": {"type": "auto", "file_ids": file_ids},
                }),
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
        if req.tools.iter().any(|t| matches!(t, ToolSpec::CodeInterpreter { .. })) {
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
    }
    body.insert("user".into(), json!(req.user));
    if flavor == Flavor::OpenAi && !req.metadata.is_empty() {
        body.insert("metadata".into(), Value::Object(req.metadata.clone()));
    }
    apply_api_params(&mut body, req);
    Value::Object(body)
}

/// Parses Responses `usage`.
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let u = v.as_object()?;
    let get = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let cached = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reasoning = u
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(UsageTokens {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        reasoning_tokens: reasoning,
    })
}

/// Error message from a `response.failed` / `error` payload.
#[must_use]
pub fn parse_error_message(data: &Value) -> Option<String> {
    let from = |e: &Value| e.get("message").and_then(Value::as_str).map(str::to_owned);
    data.get("response")
        .and_then(|r| r.get("error"))
        .and_then(from)
        .or_else(|| data.get("error").and_then(from))
        .or_else(|| data.get("message").and_then(Value::as_str).map(str::to_owned))
}

fn char_slice(text: &str, start: usize, end: usize) -> String {
    if end <= start {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

fn parse_annotation(a: &Value, text: &str) -> Option<RawCitation> {
    match a.get("type").and_then(Value::as_str)? {
        "file_citation" | "container_file_citation" | "file_path" => Some(RawCitation::File {
            file_id: a.get("file_id").and_then(Value::as_str)?.to_owned(),
        }),
        "url_citation" => {
            let url = a.get("url").and_then(Value::as_str)?.to_owned();
            let title = a.get("title").and_then(Value::as_str).unwrap_or(&url).to_owned();
            let start = a.get("start_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
            let end = a.get("end_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
            let span = start.zip(end);
            let snippet = a
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| span.map(|(s, e)| char_slice(text, s, e)))
                .unwrap_or_default();
            Some(RawCitation::Web { url, title, snippet, span })
        }
        _ => None,
    }
}

/// Stateful translator from Responses SSE events to [`ProviderEvent`]s.
#[derive(Debug)]
pub struct ResponsesTranslator {
    flavor: Flavor,
    text: String,
    streamed_annotations: Vec<Value>,
    in_think: bool,
    pending: String,
}

impl ResponsesTranslator {
    /// New translator.
    #[must_use]
    pub fn new(flavor: Flavor) -> Self {
        Self { flavor, text: String::new(), streamed_annotations: Vec::new(), in_think: false, pending: String::new() }
    }

    fn text_events(&mut self, delta: &str) -> Vec<ProviderEvent> {
        if self.flavor == Flavor::OpenAi {
            self.text.push_str(delta);
            return vec![ProviderEvent::TextDelta(delta.to_owned())];
        }
        // vLLM: split `<think>...</think>` into reasoning deltas.
        let mut out = Vec::new();
        self.pending.push_str(delta);
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.pending.find(tag) {
                let before: String = self.pending.drain(..pos).collect();
                self.pending.drain(..tag.len());
                if !before.is_empty() {
                    out.push(self.emit_vllm(before));
                }
                self.in_think = !self.in_think;
                continue;
            }
            // keep a possible partial tag at the end
            let keep = (1..tag.len())
                .rev()
                .find(|n| self.pending.len() >= *n && tag.starts_with(&self.pending[self.pending.len() - n..]))
                .unwrap_or(0);
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 && self.pending.is_char_boundary(emit_len) {
                let chunk: String = self.pending.drain(..emit_len).collect();
                out.push(self.emit_vllm(chunk));
            }
            break;
        }
        out
    }

    fn emit_vllm(&mut self, chunk: String) -> ProviderEvent {
        if self.in_think {
            ProviderEvent::ReasoningDelta(chunk)
        } else {
            self.text.push_str(&chunk);
            ProviderEvent::TextDelta(chunk)
        }
    }

    fn citations(&self, response: Option<&Value>) -> Vec<RawCitation> {
        let mut annotations: Vec<(Value, String)> = Vec::new();
        if let Some(output) = response.and_then(|r| r.get("output")).and_then(Value::as_array) {
            for item in output {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        let part_text = part.get("text").and_then(Value::as_str).unwrap_or(&self.text).to_owned();
                        if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                            for a in anns {
                                annotations.push((a.clone(), part_text.clone()));
                            }
                        }
                    }
                }
            }
        }
        if annotations.is_empty() {
            annotations = self.streamed_annotations.iter().map(|a| (a.clone(), self.text.clone())).collect();
        }
        annotations.iter().filter_map(|(a, t)| parse_annotation(a, t)).collect()
    }

    /// Translates one SSE frame.
    #[must_use]
    pub fn on_frame(&mut self, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = match event {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => parsed
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let v = parsed.unwrap_or(Value::Null);
        match name.as_str() {
            "response.output_text.delta" => {
                let delta = v.get("delta").and_then(Value::as_str).unwrap_or_default().to_owned();
                if delta.is_empty() { Vec::new() } else { self.text_events(&delta) }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta"
                if self.flavor == Flavor::Vllm =>
            {
                let d = v.get("delta").and_then(Value::as_str).unwrap_or_default();
                if d.is_empty() { Vec::new() } else { vec![ProviderEvent::ReasoningDelta(d.to_owned())] }
            }
            "response.output_text.annotation.added" => {
                if let Some(a) = v.get("annotation") {
                    self.streamed_annotations.push(a.clone());
                }
                Vec::new()
            }
            "response.file_search_call.searching" => vec![ProviderEvent::ToolStart {
                name: "file_search".into(),
                details: json!({}),
            }],
            "response.file_search_call.completed" => {
                let n = v.get("results").and_then(Value::as_array).map_or(0, Vec::len);
                vec![ProviderEvent::ToolDone { name: "file_search".into(), details: json!({"files_searched": n}) }]
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
                let item = v.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("code_interpreter_call") {
                    let logs: Vec<String> = item
                        .get("outputs")
                        .and_then(Value::as_array)
                        .map(|outs| {
                            outs.iter()
                                .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                                .filter_map(|o| o.get("logs").and_then(Value::as_str).map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut output = logs.join("\n");
                    if output.chars().count() > CODE_OUTPUT_LIMIT {
                        output = output.chars().take(CODE_OUTPUT_LIMIT).collect::<String>() + "...[truncated]";
                    }
                    vec![ProviderEvent::ToolDone { name: "code_interpreter".into(), details: json!({"output": output}) }]
                } else {
                    Vec::new()
                }
            }
            "response.completed" | "response.incomplete" => {
                let mut out = Vec::new();
                if !self.pending.is_empty() {
                    let rest = std::mem::take(&mut self.pending);
                    out.push(self.emit_vllm(rest));
                }
                let response = v.get("response");
                let usage = response.and_then(|r| r.get("usage")).and_then(parse_usage);
                let response_id = response.and_then(|r| r.get("id")).and_then(Value::as_str).map(str::to_owned);
                let incomplete_reason = (name == "response.incomplete").then(|| {
                    response
                        .and_then(|r| r.get("incomplete_details"))
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                let citations = if incomplete_reason.is_some() { Vec::new() } else { self.citations(response) };
                out.push(ProviderEvent::Completed { usage, response_id, incomplete_reason, citations });
                out
            }
            "response.failed" | "error" | "response.error" => {
                let msg = parse_error_message(&v)
                    .or_else(|| (!data.trim().is_empty() && v.is_null()).then(|| data.to_owned()))
                    .unwrap_or_else(|| "Provider returned an error".to_owned());
                let usage = v.get("response").and_then(|r| r.get("usage")).and_then(parse_usage);
                vec![ProviderEvent::Failed(ProviderError {
                    code: ProviderErrorCode::ProviderError,
                    message: sanitize_provider_message(&msg),
                    usage,
                })]
            }
            _ => Vec::new(),
        }
    }

    /// Accumulated output text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Parses a non-streaming Responses body.
#[must_use]
pub fn parse_completion(body: &Value) -> (String, Option<UsageTokens>) {
    let mut text = String::new();
    if let Some(t) = body.get("output_text").and_then(Value::as_str) {
        text.push_str(t);
    } else if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    (text, body.get("usage").and_then(parse_usage))
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod responses_tests;
