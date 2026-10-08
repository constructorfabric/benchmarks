//! `OpenAI` / Azure `OpenAI` Responses API adapter (DESIGN §3.3 "Provider Event
//! Translation", §4 "Provider Request Metadata").
//!
//! The event name comes from the SSE `event:` line, or from the data's `type`
//! when the line is missing or `message`. Citations are collected from
//! `response.output_text.annotation.added` and from the `response.completed`
//! output, and sent once before `Completed` (not on `response.incomplete`).
//! A `function_call` item (`response.output_item.done`) is reported as
//! [`LlmEvent::FunctionCall`] (no `tool` event); function call input items are
//! sent as `function_call` / `function_call_output` (knowledge search loop).

use std::collections::{BTreeMap, HashMap};

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::error_from_value;
use super::stream::Translate;
use super::wire::{array, merge_extra_body, str_field};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::llm::sse::SseFrame;
use crate::infra::llm::types::{
    CompletionResult, ContentPart, InputItem, LlmEvent, LlmRequest, ProviderError, RawCitation,
    ToolSpec,
};

/// Cap of the code interpreter `output` detail, in characters.
const CODE_OUTPUT_CAP: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

const FILE_SEARCH: &str = "file_search";
const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";

// ── Request ──────────────────────────────────────────────────────────────────

/// The Responses API request body.
pub(super) fn build_body(req: &LlmRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(req.instructions));
    }
    body.insert(
        "input".to_owned(),
        Value::Array(req.input.iter().map(input_item).collect()),
    );
    if !req.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(req.tools.iter().map(tool).collect()),
        );
    }
    if req
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
    {
        body.insert(
            "include".to_owned(),
            json!(["code_interpreter_call.outputs"]),
        );
    }
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    if let Some(n) = req.max_tool_calls {
        body.insert("max_tool_calls".to_owned(), json!(n));
    }
    insert_api_params(&mut body, req);
    body.insert("user".to_owned(), json!(req.user));
    let m = &req.metadata;
    body.insert(
        "metadata".to_owned(),
        json!({
            "tenant_id": m.tenant_id,
            "user_id": m.user_id,
            "chat_id": m.chat_id,
            "request_type": m.request_type,
            "feature": m.feature,
        }),
    );
    body.insert("stream".to_owned(), json!(req.stream));
    merge_extra_body(&mut body, req);
    Value::Object(body)
}

fn input_item(item: &InputItem) -> Value {
    match item {
        InputItem::Message { role, content } => json!({
            "role": role,
            "content": content.iter().map(content_part).collect::<Vec<_>>(),
        }),
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
    }
}

fn content_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::InputText(text) => json!({"type": "input_text", "text": text}),
        ContentPart::OutputText(text) => json!({"type": "output_text", "text": text}),
        ContentPart::InputImage { file_id } => json!({"type": "input_image", "file_id": file_id}),
    }
}

fn tool(spec: &ToolSpec) -> Value {
    match spec {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => json!({
            "type": FILE_SEARCH,
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({"type": WEB_SEARCH, "search_context_size": search_context_size}),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": CODE_INTERPRETER,
            "container": {"type": "auto", "file_ids": file_ids},
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
    }
}

/// Sampling parameters, each only when set; `reasoning_effort` becomes
/// `reasoning.effort`. `stop` is not a Responses API parameter and is not sent.
fn insert_api_params(body: &mut Map<String, Value>, req: &LlmRequest) {
    let p = &req.api_params;
    for (key, value) in [
        ("temperature", p.temperature),
        ("top_p", p.top_p),
        ("frequency_penalty", p.frequency_penalty),
        ("presence_penalty", p.presence_penalty),
    ] {
        if let Some(v) = value {
            body.insert(key.to_owned(), json!(v));
        }
    }
    if let Some(effort) = &p.reasoning_effort {
        body.insert("reasoning".to_owned(), json!({"effort": effort}));
    }
}

// ── Non-streaming response ───────────────────────────────────────────────────

/// Text and usage of a non-streaming Responses API reply.
pub(super) fn parse_completion(bytes: &[u8]) -> Result<CompletionResult, ProviderError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| ProviderError::provider("invalid provider response"))?;
    if v.get("status").and_then(Value::as_str) == Some("failed")
        || v.get("error").is_some_and(|e| !e.is_null())
    {
        return Err(error_from_value(v.get("error"))
            .unwrap_or_else(|| ProviderError::provider("provider error")));
    }
    let text: String = output_texts(&v)
        .map(|(_, _, part)| str_field(part, "text"))
        .collect();
    Ok(CompletionResult {
        text,
        usage: parse_usage(v.get("usage")),
    })
}

/// Every `output_text` part of a response: `(output_index, content_index, part)`.
fn output_texts(response: &Value) -> impl Iterator<Item = (u64, u64, &Value)> {
    array(response.get("output"))
        .iter()
        .zip(0u64..)
        .filter(|(item, _)| item.get("type").and_then(Value::as_str) == Some("message"))
        .flat_map(|(item, oi)| {
            array(item.get("content"))
                .iter()
                .zip(0u64..)
                .filter(|(part, _)| part.get("type").and_then(Value::as_str) == Some("output_text"))
                .map(move |(part, ci)| (oi, ci, part))
        })
}

fn index(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v?.as_object()?;
    let num = |obj: Option<&Value>, key: &str| {
        obj.and_then(|o| o.get(key))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    Some(UsageTokens {
        input_tokens: num(v, "input_tokens"),
        output_tokens: num(v, "output_tokens"),
        cache_read_input_tokens: num(u.get("input_tokens_details"), "cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: num(u.get("output_tokens_details"), "reasoning_tokens"),
    })
}

// ── Streaming ────────────────────────────────────────────────────────────────

/// Per-stream translation state.
#[derive(Default)]
pub(super) struct Translator {
    /// Streamed text per `(output_index, content_index)` (citation snippets).
    texts: HashMap<(u64, u64), String>,
    /// Streamed annotations by `(output_index, content_index, annotation_index)`.
    annotations: BTreeMap<(u64, u64, u64), Value>,
    /// Map `response.reasoning_text.delta` to reasoning deltas (vLLM).
    reasoning_text: bool,
}

impl Translator {
    /// A translator that also reports `response.reasoning_text.delta`.
    pub(super) fn with_reasoning_text() -> Self {
        Self {
            reasoning_text: true,
            ..Self::default()
        }
    }
}

impl Translate for Translator {
    fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Vec::new();
        }
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = match frame.event.as_deref() {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => parsed
                .as_ref()
                .map(|v| str_field(v, "type").to_owned())
                .unwrap_or_default(),
        };
        if name == "error" {
            return vec![error_event(parsed.as_ref(), data)];
        }
        let Some(v) = parsed else {
            tracing::debug!(event = %name, "unparseable provider event ignored");
            return Vec::new();
        };
        self.on_event(&name, &v)
    }
}

impl Translator {
    fn on_event(&mut self, name: &str, v: &Value) -> Vec<LlmEvent> {
        let ev = match name {
            "response.output_text.delta" => {
                let delta = str_field(v, "delta");
                self.texts
                    .entry((index(v, "output_index"), index(v, "content_index")))
                    .or_default()
                    .push_str(delta);
                LlmEvent::TextDelta(delta.to_owned())
            }
            "response.reasoning_text.delta" if self.reasoning_text => {
                LlmEvent::ReasoningDelta(str_field(v, "delta").to_owned())
            }
            "response.output_text.annotation.added" => {
                if let Some(ann) = v.get("annotation") {
                    let key = (
                        index(v, "output_index"),
                        index(v, "content_index"),
                        index(v, "annotation_index"),
                    );
                    self.annotations.insert(key, ann.clone());
                }
                return Vec::new();
            }
            "response.file_search_call.searching" => tool_start(FILE_SEARCH),
            "response.file_search_call.completed" => LlmEvent::ToolDone {
                name: FILE_SEARCH.to_owned(),
                details: json!({"files_searched": array(v.get("results")).len()}),
            },
            "response.web_search_call.searching" => tool_start(WEB_SEARCH),
            "response.web_search_call.completed" => LlmEvent::ToolDone {
                name: WEB_SEARCH.to_owned(),
                details: json!({}),
            },
            "response.code_interpreter_call.in_progress" => tool_start(CODE_INTERPRETER),
            "response.output_item.done" => match code_interpreter_done(v.get("item"))
                .or_else(|| function_call_done(v.get("item")))
            {
                Some(ev) => ev,
                None => return Vec::new(),
            },
            "response.completed" => return self.completed(v.get("response")),
            "response.incomplete" => {
                let response = v.get("response");
                let reason = response
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                LlmEvent::Completed {
                    usage: parse_usage(response.and_then(|r| r.get("usage"))),
                    response_id: response_id(response),
                    incomplete_reason: Some(reason.to_owned()),
                }
            }
            "response.failed" => failed(v),
            _ => return Vec::new(),
        };
        vec![ev]
    }

    fn completed(&self, response: Option<&Value>) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        let citations = response.map(|r| self.citations(r)).unwrap_or_default();
        if !citations.is_empty() {
            events.push(LlmEvent::Citations(citations));
        }
        events.push(LlmEvent::Completed {
            usage: parse_usage(response.and_then(|r| r.get("usage"))),
            response_id: response_id(response),
            incomplete_reason: None,
        });
        events
    }

    /// Streamed annotations plus those in the final output (deduplicated by
    /// position and value), mapped with the text of the part carrying them.
    fn citations(&self, response: &Value) -> Vec<RawCitation> {
        let mut annotations = self.annotations.clone();
        let mut final_texts: HashMap<(u64, u64), &str> = HashMap::new();
        for (oi, ci, part) in output_texts(response) {
            final_texts.insert((oi, ci), str_field(part, "text"));
            for (ann, ai) in array(part.get("annotations")).iter().zip(0u64..) {
                let key = (oi, ci, ai);
                if !annotations.contains_key(&key) && !annotations.values().any(|a| a == ann) {
                    annotations.insert(key, ann.clone());
                }
            }
        }
        annotations
            .iter()
            .filter_map(|(&(oi, ci, _), ann)| {
                let text = final_texts
                    .get(&(oi, ci))
                    .copied()
                    .or_else(|| self.texts.get(&(oi, ci)).map(String::as_str))
                    .unwrap_or_default();
                map_annotation(ann, text)
            })
            .collect()
    }
}

fn tool_start(name: &str) -> LlmEvent {
    LlmEvent::ToolStart {
        name: name.to_owned(),
        details: json!({}),
    }
}

fn response_id(response: Option<&Value>) -> Option<String> {
    response
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `output_item.done` of a `code_interpreter_call`: the `logs` outputs joined
/// with `\n`, capped at [`CODE_OUTPUT_CAP`] characters.
fn code_interpreter_done(item: Option<&Value>) -> Option<LlmEvent> {
    let item = item?;
    if str_field(item, "type") != "code_interpreter_call" {
        return None;
    }
    let logs: Vec<&str> = array(item.get("outputs"))
        .iter()
        .filter(|o| str_field(o, "type") == "logs")
        .map(|o| str_field(o, "logs"))
        .collect();
    let joined = logs.join("\n");
    let output = if joined.chars().count() > CODE_OUTPUT_CAP {
        let mut capped: String = joined.chars().take(CODE_OUTPUT_CAP).collect();
        capped.push_str(TRUNCATED_SUFFIX);
        capped
    } else {
        joined
    };
    Some(LlmEvent::ToolDone {
        name: CODE_INTERPRETER.to_owned(),
        details: json!({"output": output}),
    })
}

/// `output_item.done` of a `function_call` item.
fn function_call_done(item: Option<&Value>) -> Option<LlmEvent> {
    let item = item?;
    if str_field(item, "type") != "function_call" {
        return None;
    }
    Some(LlmEvent::FunctionCall {
        call_id: str_field(item, "call_id").to_owned(),
        name: str_field(item, "name").to_owned(),
        arguments: str_field(item, "arguments").to_owned(),
    })
}

/// `url_citation` -> web citation, `file_citation` -> file citation; other
/// annotation types are skipped.
fn map_annotation(ann: &Value, text: &str) -> Option<RawCitation> {
    match str_field(ann, "type") {
        "url_citation" => {
            let url = ann.get("url").and_then(Value::as_str)?;
            let start = ann.get("start_index").and_then(Value::as_u64);
            let end = ann.get("end_index").and_then(Value::as_u64);
            let span = start.zip(end);
            let snippet = match ann.get("text").and_then(Value::as_str) {
                Some(own) if !own.is_empty() => own.to_owned(),
                _ => span
                    .map(|(s, e)| char_range(text, s, e))
                    .unwrap_or_default(),
            };
            Some(RawCitation::Web {
                url: url.to_owned(),
                title: str_field(ann, "title").to_owned(),
                snippet,
                span,
            })
        }
        "file_citation" => {
            let file_id = ann.get("file_id").and_then(Value::as_str)?;
            Some(RawCitation::File {
                provider_file_id: file_id.to_owned(),
                filename: ann
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        _ => None,
    }
}

/// Characters `[start, end)` of `text`; empty when the range is outside it.
fn char_range(text: &str, start: u64, end: u64) -> String {
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    if start > end || end > text.chars().count() {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

/// Error of a failure event: `response.error`, then a top-level `error`.
fn failure_error(v: &Value) -> Option<ProviderError> {
    error_from_value(v.get("response").and_then(|r| r.get("error")))
        .or_else(|| error_from_value(v.get("error")))
}

fn failure_usage(v: &Value) -> Option<UsageTokens> {
    parse_usage(v.get("response").and_then(|r| r.get("usage")))
}

/// `response.failed`: [`failure_error`], usage kept.
fn failed(v: &Value) -> LlmEvent {
    LlmEvent::Failed {
        error: failure_error(v).unwrap_or_else(|| ProviderError::provider("provider error")),
        usage: failure_usage(v),
    }
}

/// SSE `error`: parsed like `response.failed`, then as flat `{code, message}`;
/// unparseable data becomes the (sanitized) message.
fn error_event(parsed: Option<&Value>, raw: &str) -> LlmEvent {
    let error = parsed
        .and_then(|v| failure_error(v).or_else(|| error_from_value(Some(v))))
        .unwrap_or_else(|| ProviderError::provider(sanitize_provider_message(raw)));
    LlmEvent::Failed {
        error,
        usage: parsed.and_then(failure_usage),
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod openai_responses_tests;
