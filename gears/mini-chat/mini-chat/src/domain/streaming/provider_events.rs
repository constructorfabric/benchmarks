//! The wire translation of the four adapter kinds (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): one provider SSE event in, zero or more
//! internal events out. Provider events are never forwarded to clients.
//!
//! Algorithms `cpt-cf-mini-chat-algo-message-streaming-event-translation` and
//! `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds`.

use std::collections::BTreeMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Value, json};
use toolkit_macros::domain_model;

use crate::config::ProviderKind;

/// The code interpreter output of a `tool` `done` event is cut at this many characters.
const CODE_OUTPUT_CAP: usize = 8192;

/// The suffix of a cut code interpreter output.
const TRUNCATED_SUFFIX: &str = "...[truncated]";

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// One provider SSE event as the SSE parser hands it over: the `event:` line (when present) and
/// the joined `data:` lines.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireEvent {
    pub event: Option<String>,
    pub data: String,
}

impl WireEvent {
    /// An event with an `event:` line.
    #[must_use]
    pub fn named(event: &str, data: &str) -> Self {
        Self {
            event: Some(event.to_owned()),
            data: data.to_owned(),
        }
    }

    /// An event without an `event:` line (the name is then taken from the `type` of the data).
    #[must_use]
    pub fn data_only(data: &str) -> Self {
        Self {
            event: None,
            data: data.to_owned(),
        }
    }
}

/// A citation annotation of an `output_text` part.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    /// `url_citation`: a web source.
    Url {
        url: String,
        title: Option<String>,
        start_index: Option<usize>,
        end_index: Option<usize>,
        /// The annotation text, when the provider sends one.
        text: Option<String>,
    },
    /// `file_citation`: a file source by its provider file id.
    File {
        file_id: String,
        filename: Option<String>,
    },
}

/// An `output_text` part of the final response with the annotations it carries.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPart {
    pub text: String,
    pub annotations: Vec<Annotation>,
}

/// A normal or truncated completion.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCompletion {
    pub usage: Option<UsageTokens>,
    /// The provider `response.id`, kept internally.
    pub response_id: Option<String>,
    /// `Some` for `response.incomplete` (or the adapter equivalent): `max_tokens` for a
    /// `length` or `max_tokens` stop, otherwise the provider reason.
    pub incomplete_reason: Option<String>,
    /// The `output_text` parts with their annotations (empty for Chat Completions and
    /// Anthropic, which produce no citations).
    pub parts: Vec<OutputPart>,
    /// The function calls of the response (the Responses and Anthropic adapters).
    pub tool_calls: Vec<ToolCallData>,
}

/// A function call of a completed response.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallData {
    pub name: String,
    pub call_id: String,
    /// The raw JSON text of the arguments.
    pub arguments: String,
}

/// A provider failure carried by an event (`response.failed`, `error`, the Anthropic `error`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailureEvent {
    /// The provider's own code, kept internally.
    pub code: Option<String>,
    /// The provider's own message, kept internally (the client text is sanitized).
    pub message: Option<String>,
    /// `response.usage` when present.
    pub usage: Option<UsageTokens>,
}

/// An internal provider event.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// A text delta.
    Text(String),
    /// Reasoning text (vLLM `<think>` blocks).
    Reasoning(String),
    /// A tool call started.
    ToolStart {
        name: String,
        details: serde_json::Value,
    },
    /// A tool call finished.
    ToolDone {
        name: String,
        details: serde_json::Value,
    },
    /// The model requested a function tool (an outcome of the turn: unexpected unless
    /// knowledge search is active).
    ToolUse { name: String },
    /// A completed or incomplete response.
    Completed(ProviderCompletion),
    /// A failure reported by the provider stream.
    Failed(ProviderFailureEvent),
}

/// The stateful wire parser of one adapter kind: one parser per provider response.
pub trait WireParser: Send {
    /// Translate one provider event into zero or more internal events; an event that maps to
    /// nothing (an ignored or unknown event) gives an empty vector.
    fn feed(&mut self, event: &WireEvent) -> Vec<ProviderEvent>;
}

/// The parser of the adapter of `kind`.
#[must_use]
pub fn parser_for(kind: ProviderKind) -> Box<dyn WireParser> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-common
    // the four adapters translate their wire protocol into the same internal events
    match kind {
        ProviderKind::OpenaiResponses => Box::new(ResponsesParser::new(false)),
        ProviderKind::VllmResponses => Box::new(ResponsesParser::new(true)),
        ProviderKind::OpenaiChatCompletions => Box::<ChatCompletionsParser>::default(),
        ProviderKind::AnthropicMessages => Box::<AnthropicParser>::default(),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-common
}

// ── shared helpers ───────────────────────────────────────────────────────────

fn as_u64(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn string_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(ToOwned::to_owned)
}

/// The usage of a Responses `response.usage` object; `None` when the object is absent.
fn responses_usage(usage: Option<&Value>) -> Option<UsageTokens> {
    let usage = usage.filter(|u| u.is_object())?;
    Some(UsageTokens {
        input_tokens: as_u64(usage.get("input_tokens")),
        output_tokens: as_u64(usage.get("output_tokens")),
        cache_read_input_tokens: as_u64(usage.pointer("/input_tokens_details/cached_tokens")),
        cache_write_input_tokens: 0,
        reasoning_tokens: as_u64(usage.pointer("/output_tokens_details/reasoning_tokens")),
    })
}

/// The `error` of a failure: `{code, message}` of the object, or the string itself.
fn failure_of(error: Option<&Value>, usage: Option<UsageTokens>) -> ProviderFailureEvent {
    let (code, message) = match error {
        Some(Value::Object(object)) => (
            string_of(object.get("code")).or_else(|| string_of(object.get("type"))),
            string_of(object.get("message")),
        ),
        Some(Value::String(text)) => (None, Some(text.clone())),
        _ => (None, None),
    };
    ProviderFailureEvent {
        code,
        message,
        usage,
    }
}

// ── OpenAI and vLLM Responses ────────────────────────────────────────────────

/// The text of `<think>` blocks split across deltas (`vLLM` only).
#[derive(Debug, Default)]
struct ThinkSplitter {
    in_think: bool,
    /// A suffix of the text that may be the start of a tag.
    pending: String,
}

impl ThinkSplitter {
    fn emit(&self, text: &str, events: &mut Vec<ProviderEvent>) {
        if text.is_empty() {
            return;
        }
        events.push(if self.in_think {
            ProviderEvent::Reasoning(text.to_owned())
        } else {
            ProviderEvent::Text(text.to_owned())
        });
    }

    /// The length of the longest suffix of `text` that is a proper prefix of `tag`.
    fn held_back(text: &str, tag: &str) -> usize {
        (1..tag.len())
            .rev()
            .find(|len| {
                text.len() >= *len
                    && text.is_char_boundary(text.len() - len)
                    && text.ends_with(&tag[..*len])
            })
            .unwrap_or(0)
    }

    fn push(&mut self, delta: &str, events: &mut Vec<ProviderEvent>) {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.push_str(delta);
        loop {
            let tag = if self.in_think {
                THINK_CLOSE
            } else {
                THINK_OPEN
            };
            if let Some(at) = buffer.find(tag) {
                let before = buffer[..at].to_owned();
                self.emit(&before, events);
                buffer = buffer[at + tag.len()..].to_owned();
                self.in_think = !self.in_think;
                continue;
            }
            let hold = Self::held_back(&buffer, tag);
            let keep = buffer.len() - hold;
            let ready = buffer[..keep].to_owned();
            self.emit(&ready, events);
            buffer[keep..].clone_into(&mut self.pending);
            break;
        }
    }

    /// Release the held-back text (the stream ended or terminated).
    fn flush(&mut self, events: &mut Vec<ProviderEvent>) {
        let rest = std::mem::take(&mut self.pending);
        self.emit(&rest, events);
    }
}

/// The wire parser of `openai_responses` and `vllm_responses`.
#[derive(Debug)]
struct ResponsesParser {
    think: Option<ThinkSplitter>,
}

impl ResponsesParser {
    fn new(vllm: bool) -> Self {
        Self {
            think: vllm.then(ThinkSplitter::default),
        }
    }

    fn completion(response: Option<&Value>, incomplete: bool) -> ProviderCompletion {
        let response_id = string_of(response.and_then(|r| r.get("id")));
        let usage = responses_usage(response.and_then(|r| r.get("usage")));
        let incomplete_reason = incomplete.then(|| {
            string_of(response.and_then(|r| r.pointer("/incomplete_details/reason")))
                .unwrap_or_else(|| "unknown".to_owned())
        });
        let mut parts = Vec::new();
        let mut tool_calls = Vec::new();
        let output = response
            .and_then(|r| r.get("output"))
            .and_then(Value::as_array);
        for item in output.into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                tool_calls.push(ToolCallData {
                    name: string_of(item.get("name")).unwrap_or_default(),
                    call_id: string_of(item.get("call_id")).unwrap_or_default(),
                    arguments: string_of(item.get("arguments")).unwrap_or_default(),
                });
            }
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) != Some("output_text") {
                    continue;
                }
                let annotations = part
                    .get("annotations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Self::annotation)
                    .collect();
                parts.push(OutputPart {
                    text: string_of(part.get("text")).unwrap_or_default(),
                    annotations,
                });
            }
        }
        ProviderCompletion {
            usage,
            response_id,
            incomplete_reason,
            parts,
            tool_calls,
        }
    }

    fn annotation(annotation: &Value) -> Option<Annotation> {
        let index = |key: &str| {
            annotation
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        match annotation.get("type").and_then(Value::as_str)? {
            "url_citation" => Some(Annotation::Url {
                url: string_of(annotation.get("url"))?,
                title: string_of(annotation.get("title")),
                start_index: index("start_index"),
                end_index: index("end_index"),
                text: string_of(annotation.get("text")),
            }),
            "file_citation" => Some(Annotation::File {
                file_id: string_of(annotation.get("file_id"))?,
                filename: string_of(annotation.get("filename")),
            }),
            _ => None,
        }
    }

    /// The joined `logs` outputs of a `code_interpreter_call` item, capped.
    fn code_output(item: &Value) -> String {
        let logs: Vec<&str> = item
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|output| output.get("type").and_then(Value::as_str) == Some("logs"))
            .filter_map(|output| output.get("logs").and_then(Value::as_str))
            .collect();
        let joined = logs.join("\n");
        if joined.chars().count() > CODE_OUTPUT_CAP {
            let cut: String = joined.chars().take(CODE_OUTPUT_CAP).collect();
            format!("{cut}{TRUNCATED_SUFFIX}")
        } else {
            joined
        }
    }

    fn tool(name: &str, details: Value) -> (String, Value) {
        (name.to_owned(), details)
    }

    fn finish(&mut self, mut events: Vec<ProviderEvent>) -> Vec<ProviderEvent> {
        // text held back for a possible tag is released before a terminal event
        if let Some(think) = &mut self.think {
            let mut flushed = Vec::new();
            think.flush(&mut flushed);
            flushed.append(&mut events);
            return flushed;
        }
        events
    }
}

impl WireParser for ResponsesParser {
    fn feed(&mut self, event: &WireEvent) -> Vec<ProviderEvent> {
        let data: Option<Value> = serde_json::from_str(&event.data).ok();
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-event-name
        // the `event:` line; when it is missing or `message`, the `type` of the data
        let name = event
            .event
            .as_deref()
            .filter(|name| *name != "message")
            .map(ToOwned::to_owned)
            .or_else(|| string_of(data.as_ref().and_then(|d| d.get("type"))));
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-event-name
        let Some(name) = name else {
            return Vec::new();
        };
        let data_ref = data.as_ref();
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-match
        match name.as_str() {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-text-delta
            "response.output_text.delta" => {
                let delta = string_of(data_ref.and_then(|d| d.get("delta"))).unwrap_or_default();
                match &mut self.think {
                    Some(think) => {
                        let mut events = Vec::new();
                        think.push(&delta, &mut events);
                        events
                    }
                    None if delta.is_empty() => Vec::new(),
                    None => vec![ProviderEvent::Text(delta)],
                }
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-text-delta
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-fs-start
            "response.file_search_call.searching" => tool_start("file_search"),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-fs-start
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-fs-done
            "response.file_search_call.completed" => {
                let searched = data_ref
                    .and_then(|d| d.get("results"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let (name, details) =
                    Self::tool("file_search", json!({ "files_searched": searched }));
                vec![ProviderEvent::ToolDone { name, details }]
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-fs-done
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ws-start
            "response.web_search_call.searching" => tool_start("web_search"),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ws-start
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ws-done
            "response.web_search_call.completed" => tool_done("web_search"),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ws-done
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ci-start
            "response.code_interpreter_call.in_progress" => tool_start("code_interpreter"),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ci-start
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ci-done
            "response.output_item.done" => {
                let item = data_ref.and_then(|d| d.get("item"));
                match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                    Some("code_interpreter_call") => {
                        let output = item.map(Self::code_output).unwrap_or_default();
                        vec![ProviderEvent::ToolDone {
                            name: "code_interpreter".to_owned(),
                            details: json!({ "output": output }),
                        }]
                    }
                    // a function tool call is a tool-use outcome of the turn
                    Some("function_call") => vec![ProviderEvent::ToolUse {
                        name: string_of(item.and_then(|i| i.get("name"))).unwrap_or_default(),
                    }],
                    _ => Vec::new(),
                }
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ci-done
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-completed
            "response.completed" => self.finish(vec![ProviderEvent::Completed(Self::completion(
                data_ref.and_then(|d| d.get("response")),
                false,
            ))]),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-completed
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-incomplete
            "response.incomplete" => self.finish(vec![ProviderEvent::Completed(Self::completion(
                data_ref.and_then(|d| d.get("response")),
                true,
            ))]),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-incomplete
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-failed
            "response.failed" => {
                let response = data_ref.and_then(|d| d.get("response"));
                let error = response
                    .and_then(|r| r.get("error"))
                    .filter(|e| !e.is_null())
                    .or_else(|| data_ref.and_then(|d| d.get("error")));
                let usage = responses_usage(response.and_then(|r| r.get("usage")));
                self.finish(vec![ProviderEvent::Failed(failure_of(error, usage))])
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-error-event
            "error" => {
                let failure = match data_ref {
                    // parsed like `response.failed`, then as a flat `{code, message}`
                    Some(data) => {
                        let nested = data
                            .pointer("/response/error")
                            .filter(|e| !e.is_null())
                            .or_else(|| data.get("error"))
                            .filter(|e| !e.is_null());
                        match nested {
                            Some(error) => failure_of(
                                Some(error),
                                responses_usage(data.pointer("/response/usage")),
                            ),
                            None => ProviderFailureEvent {
                                code: string_of(data.get("code")),
                                message: string_of(data.get("message")),
                                usage: None,
                            },
                        }
                    }
                    // unparseable data is used as the message
                    None => ProviderFailureEvent {
                        code: None,
                        message: Some(event.data.clone()),
                        usage: None,
                    },
                };
                self.finish(vec![ProviderEvent::Failed(failure)])
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-error-event
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ci-ignored
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-default
            // `response.code_interpreter_call.interpreting` and `.completed` are ignored like
            // every other unknown event: no client event
            _ => Vec::new(),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-default
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-ci-ignored
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-match
    }
}

fn tool_start(name: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::ToolStart {
        name: name.to_owned(),
        details: json!({}),
    }]
}

fn tool_done(name: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::ToolDone {
        name: name.to_owned(),
        details: json!({}),
    }]
}

// ── OpenAI Chat Completions ──────────────────────────────────────────────────

/// One streamed function tool call.
#[derive(Debug, Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// The wire parser of `openai_chat_completions`: the stream completes on `[DONE]`, with the
/// usage of the final chunk and the finish reason.
#[derive(Debug, Default)]
struct ChatCompletionsParser {
    response_id: Option<String>,
    usage: Option<UsageTokens>,
    length_stop: bool,
    calls: BTreeMap<u64, PendingCall>,
    /// The completed function calls of the response, in index order (the `tool_calls` of the
    /// completion).
    done_calls: Vec<ToolCallData>,
}

impl ChatCompletionsParser {
    fn usage_of(usage: &Value) -> UsageTokens {
        UsageTokens {
            input_tokens: as_u64(usage.get("prompt_tokens")),
            output_tokens: as_u64(usage.get("completion_tokens")),
            cache_read_input_tokens: as_u64(usage.pointer("/prompt_tokens_details/cached_tokens")),
            cache_write_input_tokens: 0,
            reasoning_tokens: as_u64(usage.pointer("/completion_tokens_details/reasoning_tokens")),
        }
    }
}

impl WireParser for ChatCompletionsParser {
    fn feed(&mut self, event: &WireEvent) -> Vec<ProviderEvent> {
        let data = event.data.trim();
        if data == "[DONE]" {
            return vec![ProviderEvent::Completed(ProviderCompletion {
                usage: self.usage,
                response_id: self.response_id.clone(),
                incomplete_reason: self.length_stop.then(|| "max_tokens".to_owned()),
                parts: Vec::new(),
                tool_calls: std::mem::take(&mut self.done_calls),
            })];
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            return vec![ProviderEvent::Failed(failure_of(Some(error), None))];
        }
        if self.response_id.is_none() {
            self.response_id = string_of(chunk.get("id"));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Self::usage_of(usage));
        }
        let mut events = Vec::new();
        let Some(choice) = chunk.pointer("/choices/0") else {
            return events;
        };
        if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str)
            && !text.is_empty()
        {
            events.push(ProviderEvent::Text(text.to_owned()));
        }
        for call in choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = as_u64(call.get("index"));
            let function = call.get("function");
            if let Some(id) = string_of(call.get("id")) {
                // the first fragment of a call names it
                let name = string_of(function.and_then(|f| f.get("name"))).unwrap_or_default();
                events.push(ProviderEvent::ToolStart {
                    name: "function_call".to_owned(),
                    details: json!({ "index": index, "call_id": id, "name": name }),
                });
                self.calls.insert(
                    index,
                    PendingCall {
                        call_id: id,
                        name,
                        arguments: String::new(),
                    },
                );
            }
            if let Some(fragment) = string_of(function.and_then(|f| f.get("arguments")))
                && let Some(pending) = self.calls.get_mut(&index)
            {
                pending.arguments.push_str(&fragment);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            if reason == "length" {
                self.length_stop = true;
            }
            for pending in std::mem::take(&mut self.calls).into_values() {
                self.done_calls.push(ToolCallData {
                    name: pending.name.clone(),
                    call_id: pending.call_id.clone(),
                    arguments: if pending.arguments.is_empty() {
                        "{}".to_owned()
                    } else {
                        pending.arguments.clone()
                    },
                });
                events.push(ProviderEvent::ToolDone {
                    name: "function_call".to_owned(),
                    details: json!({
                        "call_id": pending.call_id,
                        "name": pending.name,
                        "arguments": pending.arguments,
                    }),
                });
                events.push(ProviderEvent::ToolUse { name: pending.name });
            }
        }
        events
    }
}

// ── Anthropic Messages ───────────────────────────────────────────────────────

/// What an open content block is.
#[derive(Debug)]
enum Block {
    /// A server tool whose stop is a `tool` `done` event.
    ServerTool(&'static str),
    /// A function tool use whose input arrives as `input_json_delta` fragments; its stop
    /// completes the call.
    Function(PendingCall),
}

/// The wire parser of `anthropic_messages`: completes on `message_stop`, with the usage merged
/// from `message_start` and `message_delta`.
#[derive(Debug, Default)]
struct AnthropicParser {
    response_id: Option<String>,
    usage: Option<UsageTokens>,
    max_tokens: bool,
    blocks: BTreeMap<u64, Block>,
    /// The completed function tool uses of the message, in order.
    calls: Vec<ToolCallData>,
}

impl AnthropicParser {
    /// Merge the counts present in an Anthropic `usage` object into the running usage.
    fn merge_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage.filter(|u| u.is_object()) else {
            return;
        };
        let merged = self.usage.get_or_insert_with(UsageTokens::default);
        if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
            merged.input_tokens = n;
        }
        if let Some(n) = usage.get("output_tokens").and_then(Value::as_u64) {
            merged.output_tokens = n;
        }
        if let Some(n) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            merged.cache_read_input_tokens = n;
        }
        if let Some(n) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            merged.cache_write_input_tokens = n;
        }
    }

    fn function_tool_name(name: &str) -> &'static str {
        match name {
            "search_knowledge" => "search_knowledge",
            "load_files" => "load_files",
            _ => "unknown_tool",
        }
    }
}

impl WireParser for AnthropicParser {
    fn feed(&mut self, event: &WireEvent) -> Vec<ProviderEvent> {
        let data: Option<Value> = serde_json::from_str(&event.data).ok();
        let name = event
            .event
            .as_deref()
            .filter(|name| *name != "message")
            .map(ToOwned::to_owned)
            .or_else(|| string_of(data.as_ref().and_then(|d| d.get("type"))));
        let (Some(name), Some(data)) = (name, data) else {
            return Vec::new();
        };
        match name.as_str() {
            "message_start" => {
                self.response_id = string_of(data.pointer("/message/id"));
                self.merge_usage(data.pointer("/message/usage"));
                Vec::new()
            }
            "content_block_start" => {
                let index = as_u64(data.get("index"));
                let block = data.get("content_block");
                match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                    Some("server_tool_use") => {
                        let tool = string_of(block.and_then(|b| b.get("name"))).unwrap_or_default();
                        let mapped = if tool.contains("code_execution") {
                            Some("code_interpreter")
                        } else if tool == "web_search" {
                            Some("web_search")
                        } else {
                            None
                        };
                        match mapped {
                            Some(mapped) => {
                                self.blocks.insert(index, Block::ServerTool(mapped));
                                tool_start(mapped)
                            }
                            None => Vec::new(),
                        }
                    }
                    Some("tool_use") => {
                        let tool = string_of(block.and_then(|b| b.get("name"))).unwrap_or_default();
                        let mapped = Self::function_tool_name(&tool);
                        self.blocks.insert(
                            index,
                            Block::Function(PendingCall {
                                call_id: string_of(block.and_then(|b| b.get("id")))
                                    .unwrap_or_default(),
                                name: tool,
                                arguments: String::new(),
                            }),
                        );
                        // one `tool` start, no done: the function tool use is the outcome
                        vec![
                            ProviderEvent::ToolStart {
                                name: mapped.to_owned(),
                                details: json!({}),
                            },
                            ProviderEvent::ToolUse {
                                name: mapped.to_owned(),
                            },
                        ]
                    }
                    Some("text") => match string_of(block.and_then(|b| b.get("text"))) {
                        Some(text) if !text.is_empty() => vec![ProviderEvent::Text(text)],
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                if data.pointer("/delta/type").and_then(Value::as_str) == Some("input_json_delta")
                    && let Some(Block::Function(call)) =
                        self.blocks.get_mut(&as_u64(data.get("index")))
                    && let Some(fragment) =
                        data.pointer("/delta/partial_json").and_then(Value::as_str)
                {
                    call.arguments.push_str(fragment);
                    return Vec::new();
                }
                if data.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                    && let Some(text) = data.pointer("/delta/text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    return vec![ProviderEvent::Text(text.to_owned())];
                }
                Vec::new()
            }
            "content_block_stop" => match self.blocks.remove(&as_u64(data.get("index"))) {
                Some(Block::ServerTool(tool)) => tool_done(tool),
                Some(Block::Function(call)) => {
                    self.calls.push(ToolCallData {
                        name: call.name,
                        call_id: call.call_id,
                        arguments: if call.arguments.is_empty() {
                            "{}".to_owned()
                        } else {
                            call.arguments
                        },
                    });
                    Vec::new()
                }
                None => Vec::new(),
            },
            "message_delta" => {
                if data.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("max_tokens")
                {
                    self.max_tokens = true;
                }
                self.merge_usage(data.get("usage"));
                Vec::new()
            }
            "message_stop" => vec![ProviderEvent::Completed(ProviderCompletion {
                usage: self.usage,
                response_id: self.response_id.clone(),
                incomplete_reason: self.max_tokens.then(|| "max_tokens".to_owned()),
                // no citations on this adapter
                parts: Vec::new(),
                tool_calls: std::mem::take(&mut self.calls),
            })],
            "error" => vec![ProviderEvent::Failed(failure_of(data.get("error"), None))],
            _ => Vec::new(),
        }
    }
}
