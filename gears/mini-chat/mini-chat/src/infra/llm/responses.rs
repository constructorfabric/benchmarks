//! `OpenAI` / Azure `OpenAI` Responses API adapter (`openai_responses`):
//! request building and translation of provider SSE events into internal
//! events (DESIGN §3.3 "Provider Event Translation").

use std::collections::BTreeMap;

use mini_chat_sdk::{ModelApiParams, UsageTokens};
use serde_json::{Map, Value, json};

/// Maximum characters of a code interpreter `output` detail.
pub const CODE_OUTPUT_CAP: usize = 8192;

/// Request keys controlled by the gear; `extra_body` cannot override them.
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

/// Role of a context message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One message of the assembled context.
#[derive(Debug, Clone)]
pub struct InputMessage {
    pub role: Role,
    pub text: String,
    pub image_file_ids: Vec<String>,
}

/// Provider-neutral chat request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub max_output_tokens: u32,
    pub tools: Vec<Value>,
    pub include_code_interpreter_outputs: bool,
    pub max_tool_calls: Option<u32>,
    pub user: String,
    pub metadata: Option<Value>,
    pub api_params: ModelApiParams,
    pub stream: bool,
    /// Raw input items appended after the messages (function calls and their outputs).
    pub extra_input: Vec<Value>,
}

fn message_json(m: &InputMessage) -> Value {
    match m.role {
        Role::User => {
            let mut content = vec![json!({"type": "input_text", "text": m.text})];
            for fid in &m.image_file_ids {
                content.push(json!({"type": "input_image", "file_id": fid}));
            }
            json!({"role": "user", "content": content})
        }
        Role::Assistant => json!({
            "role": "assistant",
            "content": [{"type": "output_text", "text": m.text}],
        }),
    }
}

/// Builds the Responses API request body.
#[must_use]
// Flat sequence of optional request fields.
#[allow(clippy::cognitive_complexity)]
pub fn build_body(req: &ChatRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    let mut input: Vec<Value> = req.input.iter().map(message_json).collect();
    input.extend(req.extra_input.iter().cloned());
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), json!(req.stream));
    body.insert("store".into(), json!(false));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    if !req.tools.is_empty() {
        body.insert("tools".into(), Value::Array(req.tools.clone()));
    }
    // The Responses adapter always bounds built-in tool calls (catalog `max_tool_calls`).
    if let Some(n) = req.max_tool_calls {
        body.insert("max_tool_calls".into(), json!(n));
    }
    if req.include_code_interpreter_outputs {
        body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
    }
    body.insert("user".into(), json!(req.user));
    if let Some(m) = &req.metadata {
        body.insert("metadata".into(), m.clone());
    }
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
    if let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if CONTROLLED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key is controlled by the request and ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
    Value::Object(body)
}

/// A text part of the final answer with its annotations.
#[derive(Debug, Clone, Default)]
pub struct TextPart {
    pub text: String,
    pub annotations: Vec<Value>,
}

/// Successful terminal outcome (`response.completed` / `response.incomplete`).
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
    pub incomplete_reason: Option<String>,
    pub parts: Vec<TextPart>,
}

/// Internal provider event.
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolStart {
        name: &'static str,
        details: Value,
    },
    ToolDone {
        name: &'static str,
        details: Value,
    },
    FunctionCall {
        name: String,
        call_id: String,
        arguments: String,
    },
    Completed(Completion),
    Failed {
        code: Option<String>,
        message: String,
        usage: Option<UsageTokens>,
    },
}

/// Parses `usage` of a Responses / Chat Completions payload.
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let u = v.as_object()?;
    let num = |k: &str| u.get(k).and_then(Value::as_i64);
    let input = num("input_tokens")
        .or_else(|| num("prompt_tokens"))
        .unwrap_or(0);
    let output = num("output_tokens")
        .or_else(|| num("completion_tokens"))
        .unwrap_or(0);
    let cached = u
        .get("input_tokens_details")
        .or_else(|| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reasoning = u
        .get("output_tokens_details")
        .or_else(|| u.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        reasoning_tokens: reasoning,
    })
}

fn error_of(v: &Value) -> (Option<String>, String) {
    let err = v
        .get("response")
        .and_then(|r| r.get("error"))
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()));
    let src = err.unwrap_or(v);
    let code = src.get("code").and_then(|c| {
        c.as_str()
            .map(str::to_owned)
            .or_else(|| c.as_i64().map(|n| n.to_string()))
    });
    let message = src
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    (code, message)
}

/// Text parts of a final `response.output`.
fn output_parts(response: &Value) -> Vec<TextPart> {
    let mut parts = Vec::new();
    if let Some(items) = response.get("output").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for c in content {
                    if c.get("type").and_then(Value::as_str) == Some("output_text") {
                        parts.push(TextPart {
                            text: c
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            annotations: c
                                .get("annotations")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default(),
                        });
                    }
                }
            }
        }
    }
    parts
}

fn code_output(item: &Value) -> String {
    let mut logs = Vec::new();
    if let Some(outputs) = item.get("outputs").and_then(Value::as_array) {
        for o in outputs {
            if o.get("type").and_then(Value::as_str) == Some("logs")
                && let Some(l) = o.get("logs").and_then(Value::as_str)
            {
                logs.push(l.to_owned());
            }
        }
    }
    let joined = logs.join("\n");
    if joined.chars().count() > CODE_OUTPUT_CAP {
        let cut: String = joined.chars().take(CODE_OUTPUT_CAP).collect();
        format!("{cut}...[truncated]")
    } else {
        joined
    }
}

/// Streaming parse state.
#[derive(Debug, Default)]
pub struct ResponsesParser {
    /// Streamed text per (`output_index`, `content_index`).
    streamed: BTreeMap<(i64, i64), TextPart>,
    pub saw_terminal: bool,
}

impl ResponsesParser {
    /// Translates one provider SSE event. `event` is the SSE `event:` name
    /// (`message` or empty falls back to the data `type`).
    pub fn on_event(&mut self, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = match event {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => parsed
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        };
        if name == "error" {
            self.saw_terminal = true;
            return vec![match parsed {
                Some(v) => {
                    let (code, message) = error_of(&v);
                    ProviderEvent::Failed {
                        code,
                        message: if message.is_empty() {
                            data.to_owned()
                        } else {
                            message
                        },
                        usage: None,
                    }
                }
                None => ProviderEvent::Failed {
                    code: None,
                    message: data.to_owned(),
                    usage: None,
                },
            }];
        }
        let Some(v) = parsed else {
            return Vec::new();
        };
        let idx = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        match name.as_str() {
            "response.output_text.delta" => {
                let delta = v
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let key = (idx("output_index"), idx("content_index"));
                self.streamed.entry(key).or_default().text.push_str(&delta);
                if delta.is_empty() {
                    Vec::new()
                } else {
                    vec![ProviderEvent::TextDelta(delta)]
                }
            }
            "response.output_text.annotation.added" => {
                let key = (idx("output_index"), idx("content_index"));
                if let Some(a) = v.get("annotation") {
                    self.streamed
                        .entry(key)
                        .or_default()
                        .annotations
                        .push(a.clone());
                }
                Vec::new()
            }
            "response.file_search_call.searching" => vec![ProviderEvent::ToolStart {
                name: "file_search",
                details: json!({}),
            }],
            "response.file_search_call.completed" => {
                let n = v
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![ProviderEvent::ToolDone {
                    name: "file_search",
                    details: json!({"files_searched": n}),
                }]
            }
            "response.web_search_call.searching" => vec![ProviderEvent::ToolStart {
                name: "web_search",
                details: json!({}),
            }],
            "response.web_search_call.completed" => vec![ProviderEvent::ToolDone {
                name: "web_search",
                details: json!({}),
            }],
            "response.code_interpreter_call.in_progress" => vec![ProviderEvent::ToolStart {
                name: "code_interpreter",
                details: json!({}),
            }],
            "response.output_item.done" => {
                let item = v.get("item").cloned().unwrap_or(Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("code_interpreter_call") => vec![ProviderEvent::ToolDone {
                        name: "code_interpreter",
                        details: json!({"output": code_output(&item)}),
                    }],
                    Some("function_call") => vec![ProviderEvent::FunctionCall {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        call_id: item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
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
                self.saw_terminal = true;
                let response = v.get("response").cloned().unwrap_or(Value::Null);
                let usage = response.get("usage").and_then(parse_usage);
                let response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let incomplete_reason = if name == "response.incomplete" {
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
                let final_parts = output_parts(&response);
                let parts = if final_parts.iter().any(|p| !p.annotations.is_empty()) {
                    final_parts
                } else {
                    std::mem::take(&mut self.streamed).into_values().collect()
                };
                vec![ProviderEvent::Completed(Completion {
                    usage,
                    response_id,
                    incomplete_reason,
                    parts,
                })]
            }
            "response.failed" => {
                self.saw_terminal = true;
                let (code, message) = error_of(&v);
                let usage = v
                    .get("response")
                    .and_then(|r| r.get("usage"))
                    .and_then(parse_usage);
                vec![ProviderEvent::Failed {
                    code,
                    message,
                    usage,
                }]
            }
            _ => Vec::new(),
        }
    }
}

/// Text of a non-streaming Responses API result.
#[must_use]
pub fn response_text(v: &Value) -> String {
    if let Some(t) = v.get("output_text").and_then(Value::as_str) {
        return t.to_owned();
    }
    output_parts(v)
        .into_iter()
        .map(|p| p.text)
        .collect::<String>()
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod responses_tests;
