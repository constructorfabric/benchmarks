//! `OpenAI` / Azure `OpenAI` Responses API adapter: request builder and SSE
//! event translation (DESIGN "Provider Event Translation").

use std::collections::HashMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::sse::SseFrame;
use super::types::{
    InputMessage, InputPart, InputRole, LlmEvent, LlmRequest, ProviderErrorKind, ProviderFailure,
    RawCitation, ToolSpec,
};

/// Keys of `extra_body` that the request controls (ignored with a warning).
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
const CI_OUTPUT_CAP: usize = 8192;

fn input_json(m: &InputMessage) -> Value {
    let role = match m.role {
        InputRole::User => "user",
        InputRole::Assistant => "assistant",
    };
    if m.is_current {
        let parts: Vec<Value> = m
            .parts
            .iter()
            .map(|p| match p {
                InputPart::Text(t) => json!({"type": "input_text", "text": t}),
                InputPart::Image { file_id } => json!({"type": "input_image", "file_id": file_id}),
            })
            .collect();
        return json!({"role": role, "content": parts});
    }
    let text: String = m
        .parts
        .iter()
        .filter_map(|p| match p {
            InputPart::Text(t) => Some(t.as_str()),
            InputPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    json!({"role": role, "content": text})
}

fn tool_json(t: &ToolSpec) -> Value {
    match t {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => {
            json!({"type": "file_search", "vector_store_ids": vector_store_ids, "max_num_results": max_num_results})
        }
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({"type": "web_search", "search_context_size": search_context_size}),
        ToolSpec::CodeInterpreter { file_ids } => {
            json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": file_ids}})
        }
    }
}

/// Builds the Responses API request body.
#[must_use]
pub fn build_request(req: &LlmRequest, with_tools: bool, with_metadata: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(req.instructions));
    }
    body.insert(
        "input".to_owned(),
        Value::Array(req.input.iter().map(input_json).collect()),
    );
    body.insert("stream".to_owned(), json!(req.stream));
    body.insert("store".to_owned(), json!(false));
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    if with_tools && !req.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(req.tools.iter().map(tool_json).collect()),
        );
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert("include".to_owned(), json!(["code_interpreter_call.outputs"]));
        }
    }
    if with_tools && let Some(n) = req.max_tool_calls {
        body.insert("max_tool_calls".to_owned(), json!(n));
    }
    body.insert("user".to_owned(), json!(req.user));
    if with_metadata && !req.metadata.is_empty() {
        body.insert("metadata".to_owned(), Value::Object(req.metadata.clone()));
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
    merge_extra_body(&mut body, p.extra_body.as_ref());
    Value::Object(body)
}

/// Merges `extra_body` keys into the top level, except controlled keys.
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

/// Parses a provider usage object (`input_tokens` / `output_tokens` or the
/// Chat Completions `prompt_tokens` / `completion_tokens`).
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(Value::as_i64);
    let input = get("input_tokens").or_else(|| get("prompt_tokens")).unwrap_or(0);
    let output = get("output_tokens")
        .or_else(|| get("completion_tokens"))
        .unwrap_or(0);
    let cached = obj
        .get("input_tokens_details")
        .or_else(|| obj.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| get("cache_read_input_tokens"))
        .unwrap_or(0);
    let reasoning = obj
        .get("output_tokens_details")
        .or_else(|| obj.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_write = get("cache_creation_input_tokens").unwrap_or(0);
    Some(UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: reasoning,
    })
}

/// Extracts a provider error message from an error payload.
#[must_use]
pub fn error_message(v: &Value) -> Option<String> {
    let e = v
        .get("response")
        .and_then(|r| r.get("error"))
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()))
        .unwrap_or(v);
    if let Some(s) = e.as_str() {
        return Some(s.to_owned());
    }
    e.get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn char_slice(s: &str, start: u64, end: u64) -> String {
    let start = usize::try_from(start).unwrap_or(usize::MAX);
    let end = usize::try_from(end).unwrap_or(usize::MAX);
    if start >= end {
        return String::new();
    }
    s.chars().skip(start).take(end - start).collect()
}

/// Stateful translator of Responses API SSE frames.
#[derive(Debug, Default)]
pub struct ResponsesTranslator {
    /// Accumulated text per `(output_index, content_index)`.
    parts: HashMap<(u64, u64), String>,
    saw_annotation_events: bool,
    terminal: bool,
}

impl ResponsesTranslator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a terminal event was produced.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    #[allow(clippy::unused_self)]
    fn annotation_to_citation(&self, a: &Value, part_text: Option<&str>) -> Option<RawCitation> {
        let ty = a.get("type").and_then(Value::as_str).unwrap_or_default();
        match ty {
            "url_citation" => {
                let url = a.get("url").and_then(Value::as_str)?.to_owned();
                let title = a
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let start = a.get("start_index").and_then(Value::as_u64);
                let end = a.get("end_index").and_then(Value::as_u64);
                let span = start.zip(end);
                let snippet = a
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| span.and_then(|(s, e)| part_text.map(|t| char_slice(t, s, e))))
                    .unwrap_or_default();
                Some(RawCitation::Web {
                    url,
                    title,
                    snippet,
                    span,
                })
            }
            "file_citation" | "container_file_citation" | "file_path" => {
                let file_id = a.get("file_id").and_then(Value::as_str)?.to_owned();
                let filename = a
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                Some(RawCitation::File { file_id, filename })
            }
            _ => None,
        }
    }

    fn citations_from_response(&self, resp: &Value) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let Some(items) = resp.get("output").and_then(Value::as_array) else {
            return out;
        };
        for item in items {
            let Some(content) = item.get("content").and_then(Value::as_array) else {
                continue;
            };
            for c in content {
                if c.get("type").and_then(Value::as_str) != Some("output_text") {
                    continue;
                }
                let text = c.get("text").and_then(Value::as_str);
                if let Some(anns) = c.get("annotations").and_then(Value::as_array) {
                    for a in anns {
                        if let Some(cit) = self.annotation_to_citation(a, text) {
                            out.push(LlmEvent::Citation(cit));
                        }
                    }
                }
            }
        }
        out
    }

    /// Translates one SSE frame.
    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Vec::new();
        }
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = frame
            .event
            .as_deref()
            .filter(|e| !e.is_empty() && *e != "message")
            .map(ToOwned::to_owned)
            .or_else(|| {
                parsed
                    .as_ref()
                    .and_then(|v| v.get("type"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default();
        let v = parsed.unwrap_or(Value::Null);
        self.translate(&name, &v, data)
    }

    #[allow(clippy::too_many_lines)]
    fn translate(&mut self, name: &str, v: &Value, raw: &str) -> Vec<LlmEvent> {
        match name {
            "response.output_text.delta" => {
                let delta = v
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let key = (
                    v.get("output_index").and_then(Value::as_u64).unwrap_or(0),
                    v.get("content_index").and_then(Value::as_u64).unwrap_or(0),
                );
                self.parts.entry(key).or_default().push_str(&delta);
                if delta.is_empty() {
                    Vec::new()
                } else {
                    vec![LlmEvent::TextDelta(delta)]
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => v
                .get("delta")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(|d| vec![LlmEvent::ReasoningDelta(d.to_owned())])
                .unwrap_or_default(),
            "response.output_text.annotation.added" => {
                self.saw_annotation_events = true;
                let key = (
                    v.get("output_index").and_then(Value::as_u64).unwrap_or(0),
                    v.get("content_index").and_then(Value::as_u64).unwrap_or(0),
                );
                let text = self.parts.get(&key).map(String::as_str);
                v.get("annotation")
                    .and_then(|a| self.annotation_to_citation(a, text))
                    .map(|c| vec![LlmEvent::Citation(c)])
                    .unwrap_or_default()
            }
            "response.file_search_call.searching" => vec![LlmEvent::ToolStart {
                name: "file_search".to_owned(),
                details: json!({}),
            }],
            "response.file_search_call.completed" => {
                let n = v
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![LlmEvent::ToolDone {
                    name: "file_search".to_owned(),
                    details: json!({"files_searched": n}),
                }]
            }
            "response.web_search_call.searching" => vec![LlmEvent::ToolStart {
                name: "web_search".to_owned(),
                details: json!({}),
            }],
            "response.web_search_call.completed" => vec![LlmEvent::ToolDone {
                name: "web_search".to_owned(),
                details: json!({}),
            }],
            "response.code_interpreter_call.in_progress" => vec![LlmEvent::ToolStart {
                name: "code_interpreter".to_owned(),
                details: json!({}),
            }],
            "response.output_item.done" => {
                let item = v.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("code_interpreter_call") {
                    return Vec::new();
                }
                let logs: Vec<String> = item
                    .get("outputs")
                    .and_then(Value::as_array)
                    .map(|outs| {
                        outs.iter()
                            .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                            .filter_map(|o| o.get("logs").and_then(Value::as_str))
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut output = logs.join("\n");
                if output.chars().count() > CI_OUTPUT_CAP {
                    output = output.chars().take(CI_OUTPUT_CAP).collect();
                    output.push_str("...[truncated]");
                }
                vec![LlmEvent::ToolDone {
                    name: "code_interpreter".to_owned(),
                    details: json!({"output": output}),
                }]
            }
            "response.completed" | "response.incomplete" => {
                self.terminal = true;
                let resp = v.get("response").unwrap_or(v);
                let mut out = Vec::new();
                if !self.saw_annotation_events {
                    out.extend(self.citations_from_response(resp));
                }
                let incomplete_reason = if name == "response.incomplete" {
                    Some(
                        resp.get("incomplete_details")
                            .and_then(|d| d.get("reason"))
                            .and_then(Value::as_str)
                            .unwrap_or("other")
                            .to_owned(),
                    )
                } else {
                    None
                };
                out.push(LlmEvent::Completed {
                    usage: resp.get("usage").and_then(parse_usage),
                    response_id: resp
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    incomplete_reason,
                });
                out
            }
            "response.failed" => {
                self.terminal = true;
                let resp = v.get("response").unwrap_or(v);
                vec![LlmEvent::Failed(ProviderFailure {
                    kind: ProviderErrorKind::ProviderError,
                    message: error_message(v).unwrap_or_else(|| "provider request failed".to_owned()),
                    usage: resp.get("usage").and_then(parse_usage),
                    response_id: resp.get("id").and_then(Value::as_str).map(ToOwned::to_owned),
                })]
            }
            "error" => {
                self.terminal = true;
                let message = if v.is_null() {
                    raw.to_owned()
                } else {
                    error_message(v).unwrap_or_else(|| raw.to_owned())
                };
                vec![LlmEvent::Failed(ProviderFailure::error(message))]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod openai_responses_tests;
