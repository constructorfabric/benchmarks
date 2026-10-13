//! Provider SSE → [`LlmEvent`] translation (DESIGN "Provider Event Translation").
//!
//! One translator per adapter kind. A translator is fed raw SSE events (`event:` name +
//! `data`) and returns the internal events they produce; the driver stops after the first
//! terminal event (`Completed` / `Failed`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use mini_chat_sdk::UsageTokens;
use regex::Regex;
use serde_json::{Value, json};

use super::super::{LlmCompletion, LlmEvent, LlmFailure, RawCitation};
use super::errors::{STREAM_ENDED_MESSAGE, failure, stream_failure};
use crate::domain::error::stream_codes::{PROVIDER_ERROR, RATE_LIMITED};

/// Maximum characters of code interpreter output carried in a `tool` done event.
pub(crate) const CODE_OUTPUT_MAX_CHARS: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// Adapter-specific SSE translator.
pub(crate) trait Translator: Send {
    /// Translates one SSE event.
    fn on_event(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent>;

    /// The transport ended without a terminal event (the returned events end with a
    /// terminal one).
    fn on_end(&mut self) -> Vec<LlmEvent> {
        vec![LlmEvent::Failed(failure(PROVIDER_ERROR, STREAM_ENDED_MESSAGE))]
    }
}

// ── shared parsing helpers ─────────────────────────────────────────────────

fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| i64::try_from(u).unwrap_or(i64::MAX)))
            .or_else(|| {
                #[allow(clippy::cast_possible_truncation)]
                n.as_f64().map(|f| f as i64)
            })
            .unwrap_or(0),
        _ => 0,
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Responses API usage (`input_tokens`, `output_tokens`, `*_details`). Also accepts the
/// Chat Completions names. `None` when there is no usage object.
pub(crate) fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v.filter(|u| u.is_object())?;
    let input = u.get("input_tokens").or_else(|| u.get("prompt_tokens"));
    let output = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"));
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| u.pointer("/prompt_tokens_details/cached_tokens"))
        .or_else(|| u.get("cache_read_input_tokens"));
    let reasoning = u
        .pointer("/output_tokens_details/reasoning_tokens")
        .or_else(|| u.pointer("/completion_tokens_details/reasoning_tokens"));
    Some(UsageTokens {
        input_tokens: int(input),
        output_tokens: int(output),
        cache_read_input_tokens: int(cached),
        cache_write_input_tokens: int(u.get("cache_creation_input_tokens")),
        reasoning_tokens: int(reasoning),
    })
}

/// `output_text` parts of a Responses API `response.output`.
fn output_text_parts(response: &Value) -> impl Iterator<Item = &Value> {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
}

/// Concatenated `output_text` of a Responses API response object.
pub(crate) fn responses_output_text(response: &Value) -> String {
    output_text_parts(response)
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect()
}

fn char_substring(text: &str, start: u64, end: u64) -> String {
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    if start > end || end > text.chars().count() {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

/// Citations from `response.output[*].content[*].annotations`.
pub(crate) fn responses_citations(response: &Value) -> Vec<RawCitation> {
    let mut out = Vec::new();
    for part in output_text_parts(response) {
        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
        let Some(annotations) = part.get("annotations").and_then(Value::as_array) else {
            continue;
        };
        for a in annotations {
            let start = a.get("start_index").and_then(Value::as_u64);
            let end = a.get("end_index").and_then(Value::as_u64);
            let span = start.zip(end);
            match a.get("type").and_then(Value::as_str) {
                Some("url_citation") => {
                    let Some(url) = s(a, "url") else { continue };
                    let snippet = s(a, "text").filter(|t| !t.is_empty()).unwrap_or_else(|| {
                        span.map_or_else(String::new, |(st, en)| char_substring(text, st, en))
                    });
                    out.push(RawCitation::Web {
                        url,
                        title: s(a, "title").unwrap_or_default(),
                        snippet,
                        span,
                    });
                }
                Some("file_citation") => {
                    let Some(file_id) = s(a, "file_id") else {
                        continue;
                    };
                    out.push(RawCitation::File {
                        file_id,
                        filename: s(a, "filename"),
                        span,
                    });
                }
                _ => {}
            }
        }
    }
    out
}

/// `logs` outputs of a `code_interpreter_call` item joined with `\n`, capped.
pub(crate) fn code_interpreter_output(item: &Value) -> String {
    let joined = item
        .get("outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
        .filter_map(|o| o.get("logs").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    cap_chars(&joined, CODE_OUTPUT_MAX_CHARS)
}

fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((idx, _)) => format!("{}{TRUNCATED_SUFFIX}", &text[..idx]),
        None => text.to_owned(),
    }
}

fn tool(name: &str, start: bool, details: Value) -> LlmEvent {
    if start {
        LlmEvent::ToolStart {
            name: name.to_owned(),
            details,
        }
    } else {
        LlmEvent::ToolDone {
            name: name.to_owned(),
            details,
        }
    }
}

// ── <think> splitting (vLLM) ───────────────────────────────────────────────

static THINK_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"(?s)<think>.*?(?:</think>|$)").unwrap()
});

/// Removes `<think>…</think>` blocks (and a reasoning prefix closed by a stray `</think>`).
pub(crate) fn strip_think(text: &str) -> String {
    let text = match (text.find("<think>"), text.find("</think>")) {
        (None, Some(i)) => &text[i + "</think>".len()..],
        (Some(o), Some(c)) if c < o => &text[c + "</think>".len()..],
        _ => text,
    };
    THINK_BLOCK_RE.replace_all(text, "").trim_start().to_owned()
}

/// Incremental splitter of text deltas into text / reasoning by `<think>` tags. Tags split
/// across deltas are handled by holding back a possible tag prefix.
#[derive(Debug, Default)]
pub(crate) struct ThinkSplitter {
    in_think: bool,
    buf: String,
}

impl ThinkSplitter {
    fn emit(&self, text: &str, out: &mut Vec<LlmEvent>) {
        if text.is_empty() {
            return;
        }
        out.push(if self.in_think {
            LlmEvent::ReasoningDelta(text.to_owned())
        } else {
            LlmEvent::TextDelta(text.to_owned())
        });
    }

    pub(crate) fn push(&mut self, delta: &str) -> Vec<LlmEvent> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(i) = self.buf.find(tag) {
                let before = self.buf[..i].to_owned();
                self.emit(&before, &mut out);
                self.buf.drain(..i + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            let len = self.buf.len();
            let keep = (1..tag.len().min(len + 1))
                .rev()
                .find(|k| {
                    self.buf.is_char_boundary(len - k) && tag.starts_with(&self.buf[len - k..])
                })
                .unwrap_or(0);
            let ready = self.buf[..len - keep].to_owned();
            self.emit(&ready, &mut out);
            self.buf.drain(..len - keep);
            break;
        }
        out
    }

    pub(crate) fn flush(&mut self) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buf);
        self.emit(&rest, &mut out);
        out
    }
}

// ── OpenAI / vLLM Responses ────────────────────────────────────────────────

/// Translator of the Responses API stream (`openai_responses`, `vllm_responses`).
#[derive(Debug, Default)]
pub(crate) struct ResponsesTranslator {
    vllm: bool,
    think: ThinkSplitter,
    /// `call_id`s of the function calls already reported.
    calls: HashSet<String>,
}

/// `FunctionCall` of a Responses API `function_call` output item.
pub(crate) fn responses_function_call(item: &Value) -> Option<LlmEvent> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    let call_id = s(item, "call_id")
        .or_else(|| s(item, "id"))
        .filter(|c| !c.is_empty())?;
    Some(LlmEvent::FunctionCall {
        call_id,
        name: s(item, "name").unwrap_or_default(),
        arguments: s(item, "arguments").unwrap_or_default(),
    })
}

impl ResponsesTranslator {
    pub(crate) fn new(vllm: bool) -> Self {
        Self {
            vllm,
            think: ThinkSplitter::default(),
            calls: HashSet::new(),
        }
    }

    /// A function call not reported yet.
    fn new_call(&mut self, item: &Value) -> Option<LlmEvent> {
        let ev = responses_function_call(item)?;
        match &ev {
            LlmEvent::FunctionCall { call_id, .. } if self.calls.insert(call_id.clone()) => Some(ev),
            _ => None,
        }
    }

    fn completion(&self, v: &Value, incomplete: bool) -> LlmCompletion {
        let response = v.get("response").unwrap_or(&Value::Null);
        let mut output_text = responses_output_text(response);
        if self.vllm {
            output_text = strip_think(&output_text);
        }
        LlmCompletion {
            response_id: s(response, "id"),
            usage: parse_usage(response.get("usage")),
            citations: responses_citations(response),
            output_text,
            incomplete_reason: incomplete.then(|| {
                response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned()
            }),
        }
    }
}

/// Event name: the SSE `event:` line, or the JSON `type` when it is missing or `message`.
fn event_name(event: Option<&str>, v: Option<&Value>) -> String {
    match event {
        Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
        _ => v
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    }
}

impl Translator for ResponsesTranslator {
    fn on_event(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = event_name(event, parsed.as_ref());
        let v = parsed.as_ref().unwrap_or(&Value::Null);
        match name.as_str() {
            "response.output_text.delta" => {
                let delta = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if self.vllm {
                    self.think.push(delta)
                } else if delta.is_empty() {
                    vec![]
                } else {
                    vec![LlmEvent::TextDelta(delta.to_owned())]
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" if self.vllm => {
                let delta = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if delta.is_empty() {
                    vec![]
                } else {
                    vec![LlmEvent::ReasoningDelta(delta.to_owned())]
                }
            }
            "response.file_search_call.searching" => vec![tool("file_search", true, json!({}))],
            "response.file_search_call.completed" => {
                let n = v
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![tool("file_search", false, json!({ "files_searched": n }))]
            }
            "response.web_search_call.searching" => vec![tool("web_search", true, json!({}))],
            "response.web_search_call.completed" => vec![tool("web_search", false, json!({}))],
            "response.code_interpreter_call.in_progress" => {
                vec![tool("code_interpreter", true, json!({}))]
            }
            "response.output_item.done" => {
                let item = v.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("code_interpreter_call") {
                    vec![tool(
                        "code_interpreter",
                        false,
                        json!({ "output": code_interpreter_output(item) }),
                    )]
                } else {
                    self.new_call(item).into_iter().collect()
                }
            }
            "response.completed" | "response.incomplete" => {
                let mut out = if self.vllm {
                    self.think.flush()
                } else {
                    vec![]
                };
                let items: Vec<Value> = v
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                out.extend(items.iter().filter_map(|item| self.new_call(item)));
                out.push(LlmEvent::Completed(
                    self.completion(v, name == "response.incomplete"),
                ));
                out
            }
            "response.failed" => {
                let response = v.get("response").unwrap_or(&Value::Null);
                vec![LlmEvent::Failed(stream_failure(
                    Some(v),
                    None,
                    parse_usage(response.get("usage")),
                    s(response, "id"),
                ))]
            }
            "error" => vec![LlmEvent::Failed(stream_failure(
                parsed.as_ref(),
                Some(data),
                None,
                None,
            ))],
            _ => vec![],
        }
    }
}

// ── OpenAI Chat Completions ────────────────────────────────────────────────

/// Translator of the Chat Completions stream (`choices[].delta`, `[DONE]`).
#[derive(Debug, Default)]
pub(crate) struct ChatCompletionsTranslator {
    response_id: Option<String>,
    usage: Option<UsageTokens>,
    finish_reason: Option<String>,
    text: String,
    /// Streamed tool calls by `index`: `(call_id, name, arguments)`.
    calls: BTreeMap<u64, (String, String, String)>,
}

impl ChatCompletionsTranslator {
    /// Reports the accumulated function calls (`tool` done + `FunctionCall`) and the
    /// terminal `Completed` event.
    fn completed(&mut self) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        for (call_id, name, arguments) in std::mem::take(&mut self.calls).into_values() {
            out.push(tool(
                "function_call",
                false,
                json!({ "call_id": call_id, "name": name, "arguments": arguments }),
            ));
            out.push(LlmEvent::FunctionCall {
                call_id,
                name,
                arguments,
            });
        }
        out.push(self.completion());
        out
    }

    fn completion(&mut self) -> LlmEvent {
        LlmEvent::Completed(LlmCompletion {
            response_id: self.response_id.take(),
            usage: self.usage.take(),
            citations: vec![],
            output_text: std::mem::take(&mut self.text),
            incomplete_reason: match self.finish_reason.as_deref() {
                Some("length") => Some("max_output_tokens".to_owned()),
                Some("content_filter") => Some("content_filter".to_owned()),
                _ => None,
            },
        })
    }
}

impl Translator for ChatCompletionsTranslator {
    fn on_event(&mut self, _event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        if data.trim() == "[DONE]" {
            return self.completed();
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![];
        };
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return vec![LlmEvent::Failed(stream_failure(
                Some(&v),
                Some(data),
                self.usage.clone(),
                self.response_id.clone(),
            ))];
        }
        if self.response_id.is_none() {
            self.response_id = s(&v, "id");
        }
        if let Some(u) = parse_usage(v.get("usage")) {
            self.usage = Some(u);
        }
        let mut out = Vec::new();
        for choice in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.text.push_str(text);
                out.push(LlmEvent::TextDelta(text.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = self.calls.entry(index).or_default();
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    entry.1.push_str(name);
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.2.push_str(args);
                }
                if let Some(call_id) = s(call, "id") {
                    entry.0.clone_from(&call_id);
                    out.push(tool(
                        "function_call",
                        true,
                        json!({
                            "index": call.get("index").cloned().unwrap_or(Value::Null),
                            "call_id": call_id,
                            "name": call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                        }),
                    ));
                }
            }
            if let Some(reason) = s(choice, "finish_reason") {
                self.finish_reason = Some(reason);
            }
        }
        out
    }

    fn on_end(&mut self) -> Vec<LlmEvent> {
        if self.finish_reason.is_some() {
            self.completed()
        } else {
            vec![LlmEvent::Failed(failure(PROVIDER_ERROR, STREAM_ENDED_MESSAGE))]
        }
    }
}

// ── Anthropic Messages ─────────────────────────────────────────────────────

/// Translator of the Anthropic Messages stream.
#[derive(Debug, Default)]
pub(crate) struct AnthropicTranslator {
    response_id: Option<String>,
    usage: UsageTokens,
    has_usage: bool,
    stop_reason: Option<String>,
    /// Content block index → tool name awaiting `content_block_stop`.
    open_tools: HashMap<u64, &'static str>,
    /// Content block index → client tool use `(id, name, partial input JSON, initial input)`.
    open_calls: HashMap<u64, (String, String, String, Value)>,
    text: String,
    citations: Vec<RawCitation>,
}

impl AnthropicTranslator {
    fn merge_usage(&mut self, u: &Value) {
        if !u.is_object() {
            return;
        }
        self.has_usage = true;
        if let Some(n) = u.get("input_tokens").filter(|n| n.is_number()) {
            self.usage.input_tokens = int(Some(n));
        }
        if let Some(n) = u.get("output_tokens").filter(|n| n.is_number()) {
            self.usage.output_tokens = int(Some(n));
        }
        if let Some(n) = u.get("cache_read_input_tokens").filter(|n| n.is_number()) {
            self.usage.cache_read_input_tokens = int(Some(n));
        }
        if let Some(n) = u
            .get("cache_creation_input_tokens")
            .filter(|n| n.is_number())
        {
            self.usage.cache_write_input_tokens = int(Some(n));
        }
    }

    fn failure(&self, v: Option<&Value>, data: &str) -> LlmFailure {
        let mut f = stream_failure(
            v,
            Some(data),
            self.has_usage.then(|| self.usage.clone()),
            self.response_id.clone(),
        );
        if v.and_then(|v| v.pointer("/error/type"))
            .and_then(Value::as_str)
            == Some("rate_limit_error")
        {
            f.code = RATE_LIMITED;
        }
        f
    }
}

impl Translator for AnthropicTranslator {
    fn on_event(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = event_name(event, parsed.as_ref());
        let v = parsed.as_ref().unwrap_or(&Value::Null);
        match name.as_str() {
            "message_start" => {
                let msg = v.get("message").unwrap_or(&Value::Null);
                self.response_id = s(msg, "id");
                if let Some(u) = msg.get("usage") {
                    self.merge_usage(u);
                }
                vec![]
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = v.get("content_block").unwrap_or(&Value::Null);
                let tool_name = block.get("name").and_then(Value::as_str).unwrap_or("");
                match block.get("type").and_then(Value::as_str) {
                    Some("server_tool_use") => {
                        let mapped = match tool_name {
                            "web_search" => Some("web_search"),
                            "code_execution"
                            | "bash_code_execution"
                            | "text_editor_code_execution" => Some("code_interpreter"),
                            _ => None,
                        };
                        mapped.map_or_else(Vec::new, |m| {
                            self.open_tools.insert(index, m);
                            vec![tool(m, true, json!({}))]
                        })
                    }
                    Some("tool_use") => {
                        let mapped = match tool_name {
                            "search_knowledge" => "search_knowledge",
                            "load_files" => "load_files",
                            _ => "unknown_tool",
                        };
                        self.open_calls.insert(
                            index,
                            (
                                s(block, "id").unwrap_or_default(),
                                tool_name.to_owned(),
                                String::new(),
                                block.get("input").cloned().unwrap_or(Value::Null),
                            ),
                        );
                        vec![tool(mapped, true, json!({}))]
                    }
                    _ => vec![],
                }
            }
            "content_block_delta" => {
                let delta = v.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            vec![]
                        } else {
                            self.text.push_str(text);
                            vec![LlmEvent::TextDelta(text.to_owned())]
                        }
                    }
                    Some("input_json_delta") => {
                        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let Some(call) = self.open_calls.get_mut(&index)
                            && let Some(p) = delta.get("partial_json").and_then(Value::as_str)
                        {
                            call.2.push_str(p);
                        }
                        vec![]
                    }
                    Some("citations_delta") => {
                        let c = delta.get("citation").unwrap_or(&Value::Null);
                        if c.get("type").and_then(Value::as_str)
                            == Some("web_search_result_location")
                            && let Some(url) = s(c, "url")
                        {
                            self.citations.push(RawCitation::Web {
                                url,
                                title: s(c, "title").unwrap_or_default(),
                                snippet: s(c, "cited_text").unwrap_or_default(),
                                span: None,
                            });
                        }
                        vec![]
                    }
                    _ => vec![],
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some((call_id, name, partial, initial)) = self.open_calls.remove(&index) {
                    let arguments = if partial.is_empty() {
                        if initial.is_object() {
                            initial.to_string()
                        } else {
                            "{}".to_owned()
                        }
                    } else {
                        partial
                    };
                    return vec![LlmEvent::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    }];
                }
                self.open_tools
                    .remove(&index)
                    .map_or_else(Vec::new, |m| vec![tool(m, false, json!({}))])
            }
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(u) = v.get("usage") {
                    self.merge_usage(u);
                }
                vec![]
            }
            "message_stop" => vec![LlmEvent::Completed(LlmCompletion {
                response_id: self.response_id.take(),
                usage: self.has_usage.then(|| self.usage.clone()),
                citations: std::mem::take(&mut self.citations),
                output_text: std::mem::take(&mut self.text),
                incomplete_reason: match self.stop_reason.as_deref() {
                    Some("max_tokens") => Some("max_output_tokens".to_owned()),
                    Some("refusal") => Some("refusal".to_owned()),
                    _ => None,
                },
            })],
            "error" => vec![LlmEvent::Failed(self.failure(parsed.as_ref(), data))],
            _ => vec![],
        }
    }
}
