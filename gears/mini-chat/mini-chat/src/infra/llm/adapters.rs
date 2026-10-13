//! Provider adapters: request bodies and SSE event translation.
//!
//! `openai_responses` is the full-fidelity adapter (`OpenAI` / `Azure OpenAI`
//! Responses API). `vllm_responses` reuses it without tools or metadata;
//! `openai_chat_completions` and `anthropic_messages` stream text.

use std::collections::HashMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::sse::SseFrame;
use super::types::{
    ChatRequest, Completion, ContentPart, ProviderEvent, RawAnnotation, Role, StreamErrorCode,
    ToolSpec,
};
use crate::config::ProviderKind;
use crate::domain::sanitize::sanitize_provider_message;

const CONTROLLED_KEYS: &[&str] = &[
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

const CI_OUTPUT_CAP: usize = 8192;

fn apply_api_params(body: &mut Map<String, Value>, req: &ChatRequest, with_extra: bool) {
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
    if let Some(effort) = &p.reasoning_effort {
        body.insert("reasoning".into(), json!({"effort": effort}));
    }
    if with_extra && let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if CONTROLLED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

fn responses_input(req: &ChatRequest) -> Vec<Value> {
    req.input
        .iter()
        .map(|m| {
            let (role, text_type) = match m.role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            let content: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text(t) => json!({"type": text_type, "text": t}),
                    ContentPart::Image { file_id } => json!({"type": "input_image", "file_id": file_id}),
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

fn tool_json(t: &ToolSpec) -> Value {
    match t {
        ToolSpec::FileSearch { vector_store_ids, max_num_results } => json!({
            "type": "file_search",
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch { search_context_size } => json!({
            "type": "web_search",
            "search_context_size": search_context_size,
        }),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": file_ids},
        }),
    }
}

/// Build the provider request body for `kind`.
#[must_use]
pub fn build_body(kind: ProviderKind, req: &ChatRequest) -> Value {
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            let vllm = kind == ProviderKind::VllmResponses;
            let mut body = Map::new();
            body.insert("model".into(), json!(req.model));
            if !req.instructions.is_empty() {
                body.insert("instructions".into(), json!(req.instructions));
            }
            body.insert("input".into(), Value::Array(responses_input(req)));
            body.insert("stream".into(), json!(req.stream));
            body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
            body.insert("store".into(), json!(false));
            if !vllm {
                if !req.tools.is_empty() {
                    body.insert("tools".into(), Value::Array(req.tools.iter().map(tool_json).collect()));
                    if req.tools.iter().any(|t| matches!(t, ToolSpec::CodeInterpreter { .. })) {
                        body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
                    }
                }
                if let Some(n) = req.max_tool_calls {
                    body.insert("max_tool_calls".into(), json!(n));
                }
                body.insert("metadata".into(), Value::Object(req.metadata.clone()));
            }
            body.insert("user".into(), json!(req.user));
            apply_api_params(&mut body, req, true);
            Value::Object(body)
        }
        ProviderKind::OpenaiChatCompletions => {
            let mut messages = Vec::new();
            if !req.instructions.is_empty() {
                messages.push(json!({"role": "system", "content": req.instructions}));
            }
            for m in &req.input {
                let role = if m.role == Role::User { "user" } else { "assistant" };
                let text: Vec<&str> = m
                    .parts
                    .iter()
                    .filter_map(|p| if let ContentPart::Text(t) = p { Some(t.as_str()) } else { None })
                    .collect();
                messages.push(json!({"role": role, "content": text.join("\n")}));
            }
            let mut body = Map::new();
            body.insert("model".into(), json!(req.model));
            body.insert("messages".into(), Value::Array(messages));
            body.insert("stream".into(), json!(req.stream));
            if req.stream {
                body.insert("stream_options".into(), json!({"include_usage": true}));
            }
            body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
            body.insert("user".into(), json!(req.user));
            apply_api_params(&mut body, req, true);
            Value::Object(body)
        }
        ProviderKind::AnthropicMessages => {
            let messages: Vec<Value> = req
                .input
                .iter()
                .map(|m| {
                    let role = if m.role == Role::User { "user" } else { "assistant" };
                    let text: Vec<&str> = m
                        .parts
                        .iter()
                        .filter_map(|p| if let ContentPart::Text(t) = p { Some(t.as_str()) } else { None })
                        .collect();
                    json!({"role": role, "content": text.join("\n")})
                })
                .collect();
            let mut body = Map::new();
            body.insert("model".into(), json!(req.model));
            if !req.instructions.is_empty() {
                body.insert("system".into(), json!(req.instructions));
            }
            body.insert("messages".into(), Value::Array(messages));
            body.insert("max_tokens".into(), json!(req.max_output_tokens));
            body.insert("stream".into(), json!(req.stream));
            body.insert("metadata".into(), json!({"user_id": req.user}));
            apply_api_params(&mut body, req, false);
            Value::Object(body)
        }
    }
}

fn as_i64(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(0)
}

/// Parse a usage object (Responses, Chat Completions or Anthropic shapes).
#[must_use]
pub fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v?;
    if !u.is_object() {
        return None;
    }
    let input = u.get("input_tokens").or_else(|| u.get("prompt_tokens"));
    let output = u.get("output_tokens").or_else(|| u.get("completion_tokens"));
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| u.pointer("/prompt_tokens_details/cached_tokens"))
        .or_else(|| u.get("cache_read_input_tokens"));
    let reasoning = u
        .pointer("/output_tokens_details/reasoning_tokens")
        .or_else(|| u.pointer("/completion_tokens_details/reasoning_tokens"));
    Some(UsageTokens {
        input_tokens: as_i64(input),
        output_tokens: as_i64(output),
        cache_read_input_tokens: as_i64(cached),
        cache_write_input_tokens: as_i64(u.get("cache_creation_input_tokens")),
        reasoning_tokens: as_i64(reasoning),
    })
}

fn parse_annotation(a: &Value, part_text: Option<&str>) -> Option<RawAnnotation> {
    let kind = a.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "url_citation" => Some(RawAnnotation::Url {
            url: a.get("url").and_then(Value::as_str).unwrap_or_default().to_owned(),
            title: a.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(),
            start: a.get("start_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()),
            end: a.get("end_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()),
            part_text: part_text.map(str::to_owned),
        }),
        "file_citation" | "container_file_citation" => Some(RawAnnotation::File {
            file_id: a.get("file_id").and_then(Value::as_str)?.to_owned(),
            filename: a.get("filename").and_then(Value::as_str).map(str::to_owned),
        }),
        _ => None,
    }
}

fn completed_annotations(response: &Value) -> Vec<RawAnnotation> {
    let mut out = Vec::new();
    if let Some(items) = response.get("output").and_then(Value::as_array) {
        for item in items {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for c in content {
                    let text = c.get("text").and_then(Value::as_str);
                    if let Some(anns) = c.get("annotations").and_then(Value::as_array) {
                        out.extend(anns.iter().filter_map(|a| parse_annotation(a, text)));
                    }
                }
            }
        }
    }
    out
}

/// Text of a non-streaming response.
#[must_use]
pub fn completion_text(kind: ProviderKind, v: &Value) -> String {
    match kind {
        ProviderKind::OpenaiChatCompletions => v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ProviderKind::AnthropicMessages => v
            .get("content")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        _ => {
            if let Some(t) = v.get("output_text").and_then(Value::as_str) {
                return t.to_owned();
            }
            let mut text = String::new();
            if let Some(items) = v.get("output").and_then(Value::as_array) {
                for item in items {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for c in content {
                            if c.get("type").and_then(Value::as_str) == Some("output_text") {
                                text.push_str(c.get("text").and_then(Value::as_str).unwrap_or_default());
                            }
                        }
                    }
                }
            }
            text
        }
    }
}

/// Parse a non-streaming completion body.
#[must_use]
pub fn parse_completion(kind: ProviderKind, v: &Value) -> Completion {
    Completion { text: completion_text(kind, v), usage: parse_usage(v.get("usage")) }
}

/// Extract `(message, is_context_length)` from a provider error payload.
#[must_use]
pub fn error_message(v: &Value) -> (String, bool) {
    let err = v
        .pointer("/response/error")
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()))
        .unwrap_or(v);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| err.as_str().map(str::to_owned))
        .unwrap_or_else(|| "Provider returned an error".to_owned());
    let code = err.get("code").and_then(Value::as_str).unwrap_or_default();
    let ctx = code.contains("context_length") || message.to_lowercase().contains("context length");
    (message, ctx)
}

/// Stateful translator from provider SSE frames to [`ProviderEvent`]s.
pub struct EventTranslator {
    kind: ProviderKind,
    part_text: HashMap<(u64, u64), String>,
    cc_usage: Option<UsageTokens>,
    cc_finish: Option<String>,
    anthropic_usage: UsageTokens,
    anthropic_stop: Option<String>,
    anthropic_tool_blocks: HashMap<u64, String>,
}

impl EventTranslator {
    #[must_use]
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            part_text: HashMap::new(),
            cc_usage: None,
            cc_finish: None,
            anthropic_usage: UsageTokens::default(),
            anthropic_stop: None,
            anthropic_tool_blocks: HashMap::new(),
        }
    }

    /// Translate one frame. Unknown events yield nothing.
    pub fn translate(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        match self.kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => self.responses(frame),
            ProviderKind::OpenaiChatCompletions => self.chat_completions(frame),
            ProviderKind::AnthropicMessages => self.anthropic(frame),
        }
    }

    /// Called when the byte stream ended; may synthesize a terminal event
    /// (Chat Completions `[DONE]` without explicit completion).
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.kind == ProviderKind::OpenaiChatCompletions && self.cc_finish.is_some() {
            let reason = self.cc_finish.take();
            return vec![ProviderEvent::Completed {
                response_id: None,
                usage: self.cc_usage.take(),
                annotations: Vec::new(),
                incomplete_reason: reason.filter(|r| r == "length").map(|_| "max_tokens".to_owned()),
            }];
        }
        Vec::new()
    }

    fn responses(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        let Ok(data) = serde_json::from_str::<Value>(&frame.data) else {
            if frame.event.as_deref() == Some("error") {
                return vec![ProviderEvent::Failed {
                    code: StreamErrorCode::ProviderError,
                    message: sanitize_provider_message(&frame.data),
                    usage: None,
                }];
            }
            return Vec::new();
        };
        let name = match frame.event.as_deref() {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => data.get("type").and_then(Value::as_str).unwrap_or_default().to_owned(),
        };
        match name.as_str() {
            "response.output_text.delta" => {
                let delta = data.get("delta").and_then(Value::as_str).unwrap_or_default().to_owned();
                let key = (
                    data.get("output_index").and_then(Value::as_u64).unwrap_or(0),
                    data.get("content_index").and_then(Value::as_u64).unwrap_or(0),
                );
                self.part_text.entry(key).or_default().push_str(&delta);
                if delta.is_empty() { Vec::new() } else { vec![ProviderEvent::TextDelta(delta)] }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" => {
                let delta = data.get("delta").and_then(Value::as_str).unwrap_or_default().to_owned();
                if delta.is_empty() { Vec::new() } else { vec![ProviderEvent::ReasoningDelta(delta)] }
            }
            "response.output_text.annotation.added" => {
                let key = (
                    data.get("output_index").and_then(Value::as_u64).unwrap_or(0),
                    data.get("content_index").and_then(Value::as_u64).unwrap_or(0),
                );
                let text = self.part_text.get(&key).cloned();
                data.get("annotation")
                    .and_then(|a| parse_annotation(a, text.as_deref()))
                    .map(ProviderEvent::Annotation)
                    .into_iter()
                    .collect()
            }
            "response.file_search_call.searching" => {
                vec![ProviderEvent::ToolStart { name: "file_search".into(), details: json!({}) }]
            }
            "response.file_search_call.completed" => {
                let n = data.get("results").and_then(Value::as_array).map_or(0, Vec::len);
                vec![ProviderEvent::ToolDone { name: "file_search".into(), details: json!({"files_searched": n}) }]
            }
            "response.web_search_call.searching" => {
                vec![ProviderEvent::ToolStart { name: "web_search".into(), details: json!({}) }]
            }
            "response.web_search_call.completed" => {
                vec![ProviderEvent::ToolDone { name: "web_search".into(), details: json!({}) }]
            }
            "response.code_interpreter_call.in_progress" => {
                vec![ProviderEvent::ToolStart { name: "code_interpreter".into(), details: json!({}) }]
            }
            "response.output_item.done" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("code_interpreter_call") {
                    return Vec::new();
                }
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
                if output.chars().count() > CI_OUTPUT_CAP {
                    output = output.chars().take(CI_OUTPUT_CAP).collect::<String>() + "...[truncated]";
                }
                vec![ProviderEvent::ToolDone { name: "code_interpreter".into(), details: json!({"output": output}) }]
            }
            "response.completed" | "response.incomplete" => {
                let response = data.get("response").cloned().unwrap_or(Value::Null);
                let incomplete_reason = (name == "response.incomplete").then(|| {
                    response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                vec![ProviderEvent::Completed {
                    response_id: response.get("id").and_then(Value::as_str).map(str::to_owned),
                    usage: parse_usage(response.get("usage")),
                    annotations: completed_annotations(&response),
                    incomplete_reason,
                }]
            }
            "response.failed" | "error" => {
                let (message, _) = error_message(&data);
                let usage = parse_usage(data.pointer("/response/usage"));
                vec![ProviderEvent::Failed {
                    code: StreamErrorCode::ProviderError,
                    message: sanitize_provider_message(&message),
                    usage,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn chat_completions(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        if frame.data.trim() == "[DONE]" {
            return self.finish();
        }
        let Ok(data) = serde_json::from_str::<Value>(&frame.data) else { return Vec::new() };
        if data.get("error").is_some() {
            let (message, _) = error_message(&data);
            return vec![ProviderEvent::Failed {
                code: StreamErrorCode::ProviderError,
                message: sanitize_provider_message(&message),
                usage: None,
            }];
        }
        if let Some(u) = parse_usage(data.get("usage")) {
            self.cc_usage = Some(u);
        }
        let mut out = Vec::new();
        if let Some(choice) = data.pointer("/choices/0") {
            if let Some(t) = choice.pointer("/delta/content").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(ProviderEvent::TextDelta(t.to_owned()));
            }
            if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
                self.cc_finish = Some(f.to_owned());
            }
        }
        out
    }

    fn anthropic(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        let Ok(data) = serde_json::from_str::<Value>(&frame.data) else { return Vec::new() };
        let name = frame
            .event
            .clone()
            .unwrap_or_else(|| data.get("type").and_then(Value::as_str).unwrap_or_default().to_owned());
        match name.as_str() {
            "message_start" => {
                if let Some(u) = parse_usage(data.pointer("/message/usage")) {
                    self.anthropic_usage.input_tokens = u.input_tokens;
                    self.anthropic_usage.cache_read_input_tokens = u.cache_read_input_tokens;
                    self.anthropic_usage.cache_write_input_tokens = u.cache_write_input_tokens;
                }
                Vec::new()
            }
            "content_block_start" => {
                let block = data.get("content_block").cloned().unwrap_or(Value::Null);
                let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                match block.get("type").and_then(Value::as_str) {
                    Some("server_tool_use") => {
                        let tool = match block.get("name").and_then(Value::as_str) {
                            Some("web_search") => "web_search",
                            Some("code_execution") => "code_interpreter",
                            Some(other) => {
                                self.anthropic_tool_blocks.insert(idx, other.to_owned());
                                return Vec::new();
                            }
                            None => return Vec::new(),
                        };
                        self.anthropic_tool_blocks.insert(idx, tool.to_owned());
                        vec![ProviderEvent::ToolStart { name: tool.into(), details: json!({}) }]
                    }
                    Some("tool_use") => {
                        let n = block.get("name").and_then(Value::as_str).unwrap_or("unknown_tool");
                        let n = if matches!(n, "search_knowledge" | "load_files") { n } else { "unknown_tool" };
                        vec![ProviderEvent::ToolStart { name: n.into(), details: json!({}) }]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => {
                let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                match self.anthropic_tool_blocks.remove(&idx) {
                    Some(t) if t == "web_search" || t == "code_interpreter" => {
                        vec![ProviderEvent::ToolDone { name: t, details: json!({}) }]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                if data.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta") {
                    let t = data.pointer("/delta/text").and_then(Value::as_str).unwrap_or_default();
                    if !t.is_empty() {
                        return vec![ProviderEvent::TextDelta(t.to_owned())];
                    }
                }
                Vec::new()
            }
            "message_delta" => {
                if let Some(o) = data.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                    self.anthropic_usage.output_tokens = o;
                }
                if let Some(s) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.anthropic_stop = Some(s.to_owned());
                }
                Vec::new()
            }
            "message_stop" => vec![ProviderEvent::Completed {
                response_id: None,
                usage: Some(self.anthropic_usage),
                annotations: Vec::new(),
                incomplete_reason: self
                    .anthropic_stop
                    .take()
                    .filter(|s| s == "max_tokens")
                    .map(|_| "max_tokens".to_owned()),
            }],
            "error" => {
                let (message, _) = error_message(&data);
                vec![ProviderEvent::Failed {
                    code: StreamErrorCode::ProviderError,
                    message: sanitize_provider_message(&message),
                    usage: None,
                }]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::ModelApiParams;

    use super::*;
    use crate::infra::llm::types::InputMessage;

    fn frame(event: Option<&str>, data: &Value) -> SseFrame {
        SseFrame { event: event.map(str::to_owned), data: data.to_string() }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "gpt-x".into(),
            instructions: "sys".into(),
            input: vec![InputMessage {
                role: Role::User,
                parts: vec![ContentPart::Text("hi".into()), ContentPart::Image { file_id: "file-1".into() }],
            }],
            max_output_tokens: 100,
            max_tool_calls: Some(2),
            tools: vec![
                ToolSpec::FileSearch { vector_store_ids: vec!["vs_1".into()], max_num_results: 5 },
                ToolSpec::CodeInterpreter { file_ids: vec!["file-2".into()] },
            ],
            user: "u".into(),
            metadata: Map::new(),
            api_params: ModelApiParams { temperature: Some(0.5), ..Default::default() },
            stream: true,
        }
    }

    #[test]
    fn responses_body_shape() {
        let b = build_body(ProviderKind::OpenaiResponses, &req());
        assert_eq!(b["model"], "gpt-x");
        assert_eq!(b["instructions"], "sys");
        assert_eq!(b["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(b["tools"][0]["vector_store_ids"][0], "vs_1");
        assert_eq!(b["tools"][1]["container"]["file_ids"][0], "file-2");
        assert_eq!(b["include"][0], "code_interpreter_call.outputs");
        assert_eq!(b["max_tool_calls"], 2);
        assert_eq!(b["temperature"], 0.5);
        assert!(b.get("top_p").is_none());
        let v = build_body(ProviderKind::VllmResponses, &req());
        assert!(v.get("tools").is_none());
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn translates_responses_stream() {
        let mut t = EventTranslator::new(ProviderKind::OpenaiResponses);
        let ev = t.translate(&frame(Some("response.output_text.delta"), &json!({"delta": "Hel"})));
        assert_eq!(ev, vec![ProviderEvent::TextDelta("Hel".into())]);
        // event name from data.type when no event line
        let ev = t.translate(&frame(None, &json!({"type": "response.output_text.delta", "delta": "lo"})));
        assert_eq!(ev, vec![ProviderEvent::TextDelta("lo".into())]);
        let ev = t.translate(&frame(None, &json!({"type": "response.web_search_call.searching"})));
        assert!(matches!(&ev[0], ProviderEvent::ToolStart { name, .. } if name == "web_search"));
        let ev = t.translate(&frame(Some("response.completed"), &json!({"response": {
            "id": "resp_1", "usage": {"input_tokens": 10, "output_tokens": 5,
                "input_tokens_details": {"cached_tokens": 2}, "output_tokens_details": {"reasoning_tokens": 1}},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hello",
                "annotations": [{"type": "url_citation", "url": "https://x", "title": "X", "start_index": 0, "end_index": 4}]}]}]
        }})));
        match &ev[0] {
            ProviderEvent::Completed { response_id, usage, annotations, incomplete_reason } => {
                assert_eq!(response_id.as_deref(), Some("resp_1"));
                let u = usage.expect("usage");
                assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_input_tokens, u.reasoning_tokens), (10, 5, 2, 1));
                assert_eq!(annotations.len(), 1);
                assert!(incomplete_reason.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn failed_event_is_sanitized() {
        let mut t = EventTranslator::new(ProviderKind::OpenaiResponses);
        let ev = t.translate(&frame(Some("response.failed"), &json!({"response": {
            "error": {"message": "file file-abcdefghijklmnop missing"}, "usage": {"input_tokens": 3, "output_tokens": 0}}})));
        match &ev[0] {
            ProviderEvent::Failed { code, message, usage } => {
                assert_eq!(*code, StreamErrorCode::ProviderError);
                assert!(!message.contains("file-abc"));
                assert_eq!(usage.expect("usage").input_tokens, 3);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn code_interpreter_output_capped() {
        let mut t = EventTranslator::new(ProviderKind::OpenaiResponses);
        let long = "x".repeat(9000);
        let ev = t.translate(&frame(Some("response.output_item.done"), &json!({"item": {
            "type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": long}]}})));
        match &ev[0] {
            ProviderEvent::ToolDone { name, details } => {
                assert_eq!(name, "code_interpreter");
                assert!(details["output"].as_str().expect("s").ends_with("...[truncated]"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn incomplete_maps_to_completed_with_reason() {
        let mut t = EventTranslator::new(ProviderKind::OpenaiResponses);
        let ev = t.translate(&frame(Some("response.incomplete"), &json!({"response": {
            "incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 2}}})));
        assert!(matches!(&ev[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens"));
    }
}
