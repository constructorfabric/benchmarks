//! `OpenAI` / Azure `OpenAI` Responses API adapter (`openai_responses`), also the
//! base of the `vLLM` Responses adapter.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::types::{
    Annotation, Completion, ContentPart, FunctionItem, InputRole, LlmEvent, LlmRequest,
    ProviderErrorKind, ProviderFailure, ToolSpec, Usage,
};

/// Top-level keys the request controls; `extra_body` keys with these names
/// are ignored with a warning.
pub const RESERVED_BODY_KEYS: &[&str] = &[
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

/// Maximum characters of a code interpreter `output` in a tool event.
pub const CODE_OUTPUT_CAP: usize = 8192;

/// Options of the Responses request builder.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct BuildOptions {
    /// Send tools (`false` for `vLLM`).
    pub tools: bool,
    /// Send `metadata` (`false` for `vLLM`).
    pub metadata: bool,
    /// Send `max_tool_calls` (`OpenAI` Responses only).
    pub max_tool_calls: bool,
}

impl BuildOptions {
    pub const OPENAI: Self = Self {
        tools: true,
        metadata: true,
        max_tool_calls: true,
    };
    pub const VLLM: Self = Self {
        tools: false,
        metadata: false,
        max_tool_calls: false,
    };
}

fn input_item(role: InputRole, content: &[ContentPart]) -> Value {
    match role {
        InputRole::User => {
            let parts: Vec<Value> = content
                .iter()
                .map(|p| match p {
                    ContentPart::Text(t) => json!({"type": "input_text", "text": t}),
                    ContentPart::Image { file_id, .. } => {
                        json!({"type": "input_image", "file_id": file_id})
                    }
                })
                .collect();
            json!({"role": "user", "content": parts})
        }
        InputRole::Assistant => {
            let text: String = content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(t) => Some(t.as_str()),
                    ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            json!({"role": "assistant", "content": [{"type": "output_text", "text": text}]})
        }
    }
}

fn tool_json(tool: &ToolSpec) -> Value {
    match tool {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => {
            let mut t = json!({"type": "file_search", "vector_store_ids": vector_store_ids});
            if *max_num_results > 0 {
                t["max_num_results"] = json!(max_num_results);
            }
            t
        }
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({"type": "web_search", "search_context_size": search_context_size}),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": file_ids}
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
            "strict": false
        }),
    }
}

/// Merge `api_params` (sampling, `extra_body`, `reasoning_effort`) into a
/// request body.
pub fn apply_api_params(
    body: &mut Map<String, Value>,
    req: &LlmRequest,
    send_stop: bool,
    reasoning_as_object: bool,
) {
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
    if send_stop && !p.stop.is_empty() {
        body.insert("stop".into(), json!(p.stop));
    }
    if let Some(effort) = &p.reasoning_effort {
        if reasoning_as_object {
            body.insert("reasoning".into(), json!({"effort": effort}));
        } else {
            body.insert("reasoning_effort".into(), json!(effort));
        }
    }
    if let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if RESERVED_BODY_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "mini-chat: extra_body key controlled by the request ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

/// Build a Responses API request body.
#[must_use]
pub fn build_request(req: &LlmRequest, opts: BuildOptions) -> Value {
    let mut input: Vec<Value> = req
        .input
        .iter()
        .map(|m| input_item(m.role, &m.content))
        .collect();
    for item in &req.function_items {
        input.push(match item {
            FunctionItem::Call {
                call_id,
                name,
                arguments,
            } => json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": arguments}),
            FunctionItem::Output { call_id, output } => {
                json!({"type": "function_call_output", "call_id": call_id, "output": output})
            }
        });
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), json!(req.stream));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    body.insert("user".into(), json!(req.user));
    if opts.metadata {
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
    if opts.tools && !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(tool_json).collect()),
        );
        if opts.max_tool_calls
            && let Some(n) = req.max_tool_calls
        {
            body.insert("max_tool_calls".into(), json!(n));
        }
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
    }
    apply_api_params(&mut body, req, false, true);
    Value::Object(body)
}

/// Parse a Responses `usage` object.
#[must_use]
pub fn parse_usage(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let num = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0);
    Some(Usage {
        input_tokens: num(usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))),
        output_tokens: num(usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))),
        cache_read_input_tokens: num(usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })),
        cache_write_input_tokens: 0,
        reasoning_tokens: num(usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .or_else(|| {
                usage
                    .get("completion_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
            })),
    })
}

/// Parse a provider error object (`response.error` / top-level `error` /
/// flat `{code, message}`), falling back to the raw data.
#[must_use]
pub fn parse_stream_error(data: &Value, raw: &str) -> (Option<String>, String) {
    let err = data
        .get("response")
        .and_then(|r| r.get("error"))
        .filter(|e| e.is_object())
        .or_else(|| data.get("error").filter(|e| e.is_object()))
        .unwrap_or(data);
    let code = err
        .get("code")
        .and_then(|c| c.as_str().map(str::to_owned))
        .or_else(|| err.get("type").and_then(Value::as_str).map(str::to_owned));
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| raw.to_owned(), str::to_owned);
    (code, message)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...[truncated]");
        out
    }
}

/// Code interpreter logs of an output item, joined and capped.
#[must_use]
pub fn code_interpreter_output(item: &Value) -> String {
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
    truncate_chars(&logs.join("\n"), CODE_OUTPUT_CAP)
}

/// Stateful translator of Responses SSE events into [`LlmEvent`]s.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ResponsesTranslator {
    /// Route text inside `<think>` blocks to reasoning (`vLLM`).
    pub split_think: bool,
    in_think: bool,
    think_buf: String,
    terminal: bool,
    response_id: Option<String>,
    started_tools: HashSet<String>,
    done_tools: HashSet<String>,
    anon_started: HashMap<String, u32>,
    anon_done: HashMap<String, u32>,
    annotation_keys: HashSet<String>,
    part_text: HashMap<(String, u64), String>,
}

const TOOL_ITEMS: &[(&str, &str)] = &[
    ("web_search_call", "web_search"),
    ("file_search_call", "file_search"),
    ("code_interpreter_call", "code_interpreter"),
];

impl ResponsesTranslator {
    #[must_use]
    pub fn new(split_think: bool) -> Self {
        Self {
            split_think,
            ..Self::default()
        }
    }

    /// `true` once a terminal event was produced.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn text_events(&mut self, delta: &str) -> Vec<LlmEvent> {
        if !self.split_think {
            return vec![LlmEvent::TextDelta(delta.to_owned())];
        }
        // Route `<think>...</think>` content to reasoning deltas.
        let mut out = Vec::new();
        self.think_buf.push_str(delta);
        loop {
            if self.in_think {
                if let Some(pos) = self.think_buf.find("</think>") {
                    let reasoning: String = self.think_buf.drain(..pos).collect();
                    self.think_buf.drain(.."</think>".len());
                    if !reasoning.is_empty() {
                        out.push(LlmEvent::ReasoningDelta(reasoning));
                    }
                    self.in_think = false;
                    continue;
                }
                // keep a possible partial closing tag
                let keep = partial_suffix(&self.think_buf, "</think>");
                let emit_len = self.think_buf.len() - keep;
                if emit_len > 0 {
                    let reasoning: String = self.think_buf.drain(..emit_len).collect();
                    out.push(LlmEvent::ReasoningDelta(reasoning));
                }
                break;
            }
            if let Some(pos) = self.think_buf.find("<think>") {
                let text: String = self.think_buf.drain(..pos).collect();
                self.think_buf.drain(.."<think>".len());
                if !text.is_empty() {
                    out.push(LlmEvent::TextDelta(text));
                }
                self.in_think = true;
                continue;
            }
            let keep = partial_suffix(&self.think_buf, "<think>");
            let emit_len = self.think_buf.len() - keep;
            if emit_len > 0 {
                let text: String = self.think_buf.drain(..emit_len).collect();
                out.push(LlmEvent::TextDelta(text));
            }
            break;
        }
        out
    }

    fn tool_start(&mut self, name: &str, item_id: Option<&str>) -> Vec<LlmEvent> {
        match item_id {
            Some(id) => {
                if !self.started_tools.insert(format!("{name}:{id}")) {
                    return Vec::new();
                }
            }
            None => *self.anon_started.entry(name.to_owned()).or_default() += 1,
        }
        vec![LlmEvent::ToolStart {
            name: name.to_owned(),
            details: json!({}),
        }]
    }

    fn tool_done(&mut self, name: &str, item_id: Option<&str>, details: Value) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let Some(id) = item_id else {
            *self.anon_done.entry(name.to_owned()).or_default() += 1;
            out.push(LlmEvent::ToolDone {
                name: name.to_owned(),
                details,
            });
            return out;
        };
        let key = format!("{name}:{id}");
        if !self.done_tools.insert(key.clone()) {
            return out;
        }
        if self.started_tools.insert(key) {
            // No keyed start was seen: match an anonymous start, otherwise
            // synthesize one (a done without a start still is a started call).
            let anon = self.anon_started.entry(name.to_owned()).or_default();
            if *anon > 0 {
                *anon -= 1;
            } else {
                out.push(LlmEvent::ToolStart {
                    name: name.to_owned(),
                    details: json!({}),
                });
            }
        }
        let anon_done = self.anon_done.entry(name.to_owned()).or_default();
        if *anon_done > 0 {
            // The same call was already reported done without an id.
            *anon_done -= 1;
            return out;
        }
        out.push(LlmEvent::ToolDone {
            name: name.to_owned(),
            details,
        });
        out
    }

    fn annotations_from_message(&mut self, item: &Value) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            return out;
        };
        for part in parts {
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                for ann in anns {
                    if let Some(a) = self.annotation(ann, Some(text)) {
                        out.push(LlmEvent::Annotation(a));
                    }
                }
            }
        }
        out
    }

    fn annotation(&mut self, ann: &Value, part_text: Option<&str>) -> Option<Annotation> {
        let kind = ann.get("type").and_then(Value::as_str).unwrap_or_default();
        let start = ann
            .get("start_index")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok());
        let end = ann
            .get("end_index")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok());
        match kind {
            "file_citation" | "file_path" | "container_file_citation" => {
                let file_id = ann.get("file_id").and_then(Value::as_str)?.to_owned();
                let key = format!("file:{file_id}");
                if !self.annotation_keys.insert(key) {
                    return None;
                }
                Some(Annotation::File {
                    file_id,
                    filename: ann
                        .get("filename")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            }
            "url_citation" => {
                let url = ann.get("url").and_then(Value::as_str)?.to_owned();
                let key = format!("url:{url}:{start:?}:{end:?}");
                if !self.annotation_keys.insert(key) {
                    return None;
                }
                Some(Annotation::Url {
                    title: ann
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    url,
                    start,
                    end,
                    text: ann.get("text").and_then(Value::as_str).map(str::to_owned),
                    part_text: part_text.map(str::to_owned),
                })
            }
            _ => None,
        }
    }

    /// Translate one SSE event (`event_name` from the `event:` line; when
    /// missing or `message`, the data `type` is used).
    #[allow(clippy::too_many_lines)]
    pub fn on_event(&mut self, event_name: Option<&str>, data: &str) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Vec::new();
        }
        let json: Value = serde_json::from_str(trimmed).unwrap_or(Value::Null);
        let name = match event_name {
            Some(n) if !n.is_empty() && n != "message" => n.to_owned(),
            _ => json
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let item_id = json.get("item_id").and_then(Value::as_str);
        match name.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(id) = json
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                {
                    self.response_id = Some(id.to_owned());
                }
                Vec::new()
            }
            "response.output_text.delta" => {
                let delta = json
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(id) = item_id {
                    let idx = json
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.part_text
                        .entry((id.to_owned(), idx))
                        .or_default()
                        .push_str(delta);
                }
                if delta.is_empty() {
                    Vec::new()
                } else {
                    self.text_events(delta)
                }
            }
            "response.reasoning_text.delta"
            | "response.reasoning.delta"
            | "response.reasoning_summary_text.delta" => {
                let delta = json
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if delta.is_empty() || !self.split_think {
                    Vec::new()
                } else {
                    vec![LlmEvent::ReasoningDelta(delta.to_owned())]
                }
            }
            "response.output_text.annotation.added" => {
                let part = item_id.and_then(|id| {
                    let idx = json
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.part_text.get(&(id.to_owned(), idx)).cloned()
                });
                json.get("annotation")
                    .and_then(|a| self.annotation(a, part.as_deref()))
                    .map(LlmEvent::Annotation)
                    .into_iter()
                    .collect()
            }
            "response.file_search_call.in_progress" | "response.file_search_call.searching" => {
                self.tool_start("file_search", item_id)
            }
            "response.file_search_call.completed" => {
                let n = json
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                self.tool_done("file_search", item_id, json!({"files_searched": n}))
            }
            "response.web_search_call.in_progress" | "response.web_search_call.searching" => {
                self.tool_start("web_search", item_id)
            }
            "response.web_search_call.completed" => {
                self.tool_done("web_search", item_id, json!({}))
            }
            "response.code_interpreter_call.in_progress" => {
                self.tool_start("code_interpreter", item_id)
            }
            "response.output_item.added" => {
                let item = json.get("item").unwrap_or(&Value::Null);
                let ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let id = item.get("id").and_then(Value::as_str);
                TOOL_ITEMS
                    .iter()
                    .find(|(t, _)| *t == ty)
                    .map(|(_, tool)| self.tool_start(tool, id))
                    .unwrap_or_default()
            }
            "response.output_item.done" => {
                let item = json.get("item").unwrap_or(&Value::Null);
                let ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let id = item.get("id").and_then(Value::as_str);
                match ty {
                    "code_interpreter_call" => self.tool_done(
                        "code_interpreter",
                        id,
                        json!({"output": code_interpreter_output(item)}),
                    ),
                    "web_search_call" if id.is_some() => {
                        self.tool_done("web_search", id, json!({}))
                    }
                    "file_search_call" if id.is_some() => {
                        let n = item
                            .get("results")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        self.tool_done("file_search", id, json!({"files_searched": n}))
                    }
                    "message" => self.annotations_from_message(item),
                    "function_call" => vec![LlmEvent::FunctionCall {
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
                    }],
                    _ => Vec::new(),
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = json.get("response").unwrap_or(&Value::Null);
                let mut out = Vec::new();
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for item in items {
                        if item.get("type").and_then(Value::as_str) == Some("message") {
                            out.extend(self.annotations_from_message(item));
                        }
                    }
                }
                if self.split_think && !self.think_buf.is_empty() {
                    let rest = std::mem::take(&mut self.think_buf);
                    out.push(if self.in_think {
                        LlmEvent::ReasoningDelta(rest)
                    } else {
                        LlmEvent::TextDelta(rest)
                    });
                }
                let incomplete_reason = (name == "response.incomplete").then(|| {
                    response
                        .get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                self.terminal = true;
                out.push(LlmEvent::Completed(Completion {
                    usage: response.get("usage").and_then(parse_usage),
                    response_id: response
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| self.response_id.clone()),
                    incomplete_reason,
                }));
                out
            }
            "response.failed" | "error" => {
                let (code, message) = parse_stream_error(&json, trimmed);
                let usage = json
                    .get("response")
                    .and_then(|r| r.get("usage"))
                    .and_then(parse_usage);
                self.terminal = true;
                vec![LlmEvent::Failed(ProviderFailure {
                    kind: ProviderErrorKind::Provider,
                    message,
                    provider_code: code,
                    usage,
                    response_id: json
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| self.response_id.clone()),
                })]
            }
            _ => Vec::new(),
        }
    }

    /// The stream ended: a missing terminal event is a provider error.
    pub fn finish(&mut self) -> Option<LlmEvent> {
        if self.terminal {
            return None;
        }
        self.terminal = true;
        Some(LlmEvent::Failed(ProviderFailure::provider(
            "provider stream ended without a terminal event",
        )))
    }
}

/// Length of the longest suffix of `buf` that is a proper prefix of `tag`.
fn partial_suffix(buf: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| {
            buf.len() >= n
                && buf.is_char_boundary(buf.len() - n)
                && tag.starts_with(&buf[buf.len() - n..])
        })
        .unwrap_or(0)
}

/// Text and usage of a non-streaming Responses result.
#[must_use]
pub fn parse_complete(json: &Value) -> (String, Option<Usage>) {
    let from_output = || {
        json.get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
                    .flat_map(|i| {
                        i.get("content")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default()
                    })
                    .filter(|p| {
                        matches!(
                            p.get("type").and_then(Value::as_str),
                            Some("output_text" | "text")
                        )
                    })
                    .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_owned))
                    .collect::<String>()
            })
            .unwrap_or_default()
    };
    let text = json
        .get("output_text")
        .and_then(Value::as_str)
        .map_or_else(from_output, str::to_owned);
    (text, json.get("usage").and_then(parse_usage))
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod openai_responses_tests;
