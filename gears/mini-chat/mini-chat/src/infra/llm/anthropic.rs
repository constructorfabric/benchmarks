//! Anthropic Messages adapter (`provider kind = anthropic_messages`): builds a Messages API
//! body, sends it through OAGW and translates the event stream into [`ProviderEvent`]s as the
//! events arrive.
//!
//! - Images are sent as `image` blocks of their Anthropic Files copy (`secondary_file_id`);
//!   an image without one is dropped.
//! - Tools: `file_search` is dropped, `web_search` and `code_interpreter` become the server tools
//!   `web_search_20250305` / `code_execution_20250522`, function tools keep their schema
//!   (`input_schema`).
//! - The caller identity goes in `metadata.user_id`; `extra_body` is not sent.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::ServiceGatewayClientV1;
use oagw_sdk::sse::ServerEvent;
use serde_json::{Map, Value, json};

use super::transport::{self, Translate};
use super::types::{
    ChatAdapter, CompletionResult, ContentPart, InputItem, ProviderError, ProviderErrorKind,
    ProviderEvent, ProviderEventStream, ProviderRequest, ProviderUsage, RawCitation, Role,
    ToolSpec,
};
use super::{ChatTarget, S2sContext};
use crate::domain::sanitize::sanitize_provider_message;

/// Value of the `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta flag of the Files API (file-sourced content blocks and `/v1/files`).
pub const FILES_API_BETA: &str = "files-api-2025-04-14";
/// Beta flag of the code execution server tool.
const CODE_EXECUTION_BETA: &str = "code-execution-2025-05-22";

const WEB_SEARCH_TOOL: &str = "web_search";
const CODE_EXECUTION_TOOL: &str = "code_execution";
/// Client-side tool names reported as themselves; any other is `unknown_tool`.
const KNOWN_FUNCTION_TOOLS: &[&str] = &["search_knowledge", "load_files"];

/// The Anthropic Messages protocol over the in-process OAGW client.
pub struct AnthropicAdapter {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

impl AnthropicAdapter {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: S2sContext) -> Self {
        Self { gateway, s2s }
    }

    async fn send(
        &self,
        target: &ChatTarget,
        req: &ProviderRequest,
        stream: bool,
    ) -> Result<http::Response<oagw_sdk::Body>, ProviderError> {
        let body = build_body(req, stream);
        let beta = beta_flags(req, &body);
        let mut headers = vec![("anthropic-version", ANTHROPIC_VERSION)];
        if !beta.is_empty() {
            headers.push(("anthropic-beta", beta.as_str()));
        }
        transport::post_json(
            &self.gateway,
            &self.s2s,
            target,
            &req.model,
            &body,
            stream,
            &headers,
        )
        .await
    }
}

#[async_trait]
impl ChatAdapter for AnthropicAdapter {
    async fn stream(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        let resp = self.send(target, &req, true).await?;
        let events = transport::event_stream(resp)?;
        Ok(transport::translate_stream(
            events,
            EventTranslator::default(),
        ))
    }

    async fn complete(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<CompletionResult, ProviderError> {
        let resp = self.send(target, &req, false).await?;
        let value = transport::json_body(resp).await?;
        if value.get("type").and_then(Value::as_str) == Some("error") {
            return Err(error_of(&value));
        }
        let text = value
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        let mut usage = UsageParts::default();
        if let Some(u) = value.get("usage") {
            usage.merge(u);
        }
        Ok(CompletionResult {
            text,
            usage: usage.total(),
        })
    }
}

// ---- request ---------------------------------------------------------------------------------

fn build_body(req: &ProviderRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("messages".into(), Value::Array(messages(&req.input)));
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(stream));
    body.insert("metadata".into(), json!({"user_id": req.user}));
    let tools: Vec<Value> = req.tools.iter().filter_map(tool).collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    let params = &req.api_params;
    if let Some(t) = params.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = params.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if !params.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(params.stop));
    }
    Value::Object(body)
}

/// `anthropic-beta` flags the request needs (comma-separated; empty when none).
fn beta_flags(req: &ProviderRequest, body: &Value) -> String {
    let mut flags = Vec::new();
    let has_file_block = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .any(|block| block.pointer("/source/type").and_then(Value::as_str) == Some("file"));
    if has_file_block {
        flags.push(FILES_API_BETA);
    }
    if req
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
    {
        flags.push(CODE_EXECUTION_BETA);
    }
    flags.join(",")
}

/// Messages with content blocks; consecutive items of one role are merged (the API expects
/// alternating roles), and messages left without blocks are skipped.
fn messages(input: &[InputItem]) -> Vec<Value> {
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for item in input {
        let (role, blocks) = match item {
            InputItem::Message { role, parts } => {
                let role = match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let blocks = parts
                    .iter()
                    .filter_map(|part| {
                        match part {
                        ContentPart::Text(text) => Some(json!({"type": "text", "text": text})),
                        ContentPart::Image {
                            secondary_file_id, ..
                        } => secondary_file_id.as_ref().map(|id| {
                            json!({"type": "image", "source": {"type": "file", "file_id": id}})
                        }),
                    }
                    })
                    .collect();
                (role, blocks)
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let input = serde_json::from_str::<Value>(arguments)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                (
                    "assistant",
                    vec![json!({"type": "tool_use", "id": call_id, "name": name, "input": input})],
                )
            }
            InputItem::FunctionCallOutput { call_id, output } => (
                "user",
                vec![json!({"type": "tool_result", "tool_use_id": call_id, "content": output})],
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => out.push((role, blocks)),
        }
    }
    out.into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

fn tool(spec: &ToolSpec) -> Option<Value> {
    match spec {
        ToolSpec::FileSearch { .. } => None,
        ToolSpec::WebSearch { .. } => {
            Some(json!({"type": "web_search_20250305", "name": WEB_SEARCH_TOOL}))
        }
        ToolSpec::CodeInterpreter { .. } => {
            Some(json!({"type": "code_execution_20250522", "name": CODE_EXECUTION_TOOL}))
        }
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => Some(json!({"name": name, "description": description, "input_schema": parameters})),
    }
}

// ---- responses -------------------------------------------------------------------------------

/// Usage as Anthropic reports it: `input_tokens` excludes the cache reads and writes, which are
/// added to the gear's `input_tokens` (the `OpenAI` convention, where cached tokens are part of
/// the input).
#[derive(Default)]
struct UsageParts {
    input: Option<i64>,
    output: Option<i64>,
    cache_read: i64,
    cache_write: i64,
}

impl UsageParts {
    /// Takes every field `usage` carries (later events report cumulative values).
    fn merge(&mut self, usage: &Value) {
        let number = |key: &str| usage.get(key).and_then(Value::as_i64);
        if let Some(n) = number("input_tokens") {
            self.input = Some(n);
        }
        if let Some(n) = number("output_tokens") {
            self.output = Some(n);
        }
        if let Some(n) = number("cache_read_input_tokens") {
            self.cache_read = n;
        }
        if let Some(n) = number("cache_creation_input_tokens") {
            self.cache_write = n;
        }
    }

    fn total(&self) -> Option<ProviderUsage> {
        if self.input.is_none() && self.output.is_none() {
            return None;
        }
        Some(ProviderUsage {
            input_tokens: self
                .input
                .unwrap_or(0)
                .saturating_add(self.cache_read)
                .saturating_add(self.cache_write),
            output_tokens: self.output.unwrap_or(0),
            cache_read_input_tokens: self.cache_read,
            cache_write_input_tokens: self.cache_write,
            reasoning_tokens: 0,
        })
    }
}

/// An `error` object (`{type: error, error: {type, message}}` or the inner object); rate limits
/// keep their kind.
fn error_of(data: &Value) -> ProviderError {
    let error = data.get("error").unwrap_or(data);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map_or_else(
            || "provider request failed".to_owned(),
            sanitize_provider_message,
        );
    let kind = match error.get("type").and_then(Value::as_str) {
        Some("rate_limit_error") => ProviderErrorKind::RateLimited {
            retry_after_secs: None,
        },
        _ => ProviderErrorKind::Provider,
    };
    ProviderError::new(kind, message)
}

/// What a content block is, by its index.
enum Block {
    Text,
    /// A server tool, reported under its shared name.
    ServerTool(&'static str),
    /// A client tool use being assembled.
    ToolUse {
        call_id: String,
        name: String,
        arguments: String,
    },
    Other,
}

#[derive(Default)]
struct EventTranslator {
    response_id: Option<String>,
    usage: UsageParts,
    stop_reason: Option<String>,
    blocks: HashMap<u64, Block>,
    citations: Vec<RawCitation>,
}

impl Translate for EventTranslator {
    fn translate(&mut self, raw: &ServerEvent) -> Vec<ProviderEvent> {
        let Ok(data) = serde_json::from_str::<Value>(raw.data.trim()) else {
            if raw.event.as_deref() == Some("error") {
                return vec![ProviderEvent::Failed(ProviderError::provider(
                    sanitize_provider_message(raw.data.trim()),
                ))];
            }
            return Vec::new();
        };
        let name = raw
            .event
            .as_deref()
            .filter(|n| !n.is_empty() && *n != "message")
            .or_else(|| data.get("type").and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        match name.as_str() {
            "message_start" => {
                let message = data.get("message");
                self.response_id = message
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    self.usage.merge(usage);
                }
                Vec::new()
            }
            "content_block_start" => self
                .block_start(index, data.get("content_block").unwrap_or(&Value::Null))
                .into_iter()
                .collect(),
            "content_block_delta" => self
                .block_delta(index, data.get("delta").unwrap_or(&Value::Null))
                .into_iter()
                .collect(),
            "content_block_stop" => self.block_stop(index).into_iter().collect(),
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(usage) = data.get("usage") {
                    self.usage.merge(usage);
                }
                Vec::new()
            }
            "message_stop" => vec![self.terminal()],
            "error" => vec![ProviderEvent::Failed(error_of(&data))],
            _ => Vec::new(),
        }
    }
}

impl EventTranslator {
    fn block_start(&mut self, index: u64, block: &Value) -> Option<ProviderEvent> {
        let text = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default();
        let (kept, event) = match text("type") {
            "text" => (Block::Text, None),
            "server_tool_use" => {
                let shared = match text("name") {
                    WEB_SEARCH_TOOL => Some(WEB_SEARCH_TOOL),
                    CODE_EXECUTION_TOOL => Some("code_interpreter"),
                    _ => None,
                };
                match shared {
                    Some(shared) => (Block::ServerTool(shared), Some(tool_event(shared, true))),
                    None => (Block::Other, None),
                }
            }
            "tool_use" => {
                let name = text("name").to_owned();
                let reported = if KNOWN_FUNCTION_TOOLS.contains(&name.as_str()) {
                    name.clone()
                } else {
                    "unknown_tool".to_owned()
                };
                (
                    Block::ToolUse {
                        call_id: text("id").to_owned(),
                        name,
                        arguments: String::new(),
                    },
                    Some(tool_event(&reported, true)),
                )
            }
            _ => (Block::Other, None),
        };
        self.blocks.insert(index, kept);
        event
    }

    fn block_delta(&mut self, index: u64, delta: &Value) -> Option<ProviderEvent> {
        let text = |key: &str| delta.get(key).and_then(Value::as_str);
        match delta.get("type").and_then(Value::as_str)? {
            "text_delta" => text("text")
                .filter(|t| !t.is_empty())
                .map(|t| ProviderEvent::TextDelta(t.to_owned())),
            "input_json_delta" => {
                if let Some(Block::ToolUse { arguments, .. }) = self.blocks.get_mut(&index) {
                    arguments.push_str(text("partial_json").unwrap_or_default());
                }
                None
            }
            "citations_delta" => {
                if let Some(citation) = delta.get("citation").and_then(citation) {
                    self.citations.push(citation);
                }
                None
            }
            _ => None,
        }
    }

    fn block_stop(&mut self, index: u64) -> Option<ProviderEvent> {
        match self.blocks.remove(&index)? {
            Block::ServerTool(name) => Some(tool_event(name, false)),
            Block::ToolUse {
                call_id,
                name,
                arguments,
            } => Some(ProviderEvent::FunctionCall {
                call_id,
                name,
                arguments: if arguments.is_empty() {
                    "{}".to_owned()
                } else {
                    arguments
                },
            }),
            Block::Text | Block::Other => None,
        }
    }

    /// `message_stop`: `max_tokens` is a truncated answer, every other stop reason a completed one.
    fn terminal(&mut self) -> ProviderEvent {
        let response_id = self.response_id.take();
        let usage = self.usage.total();
        if self.stop_reason.as_deref() == Some("max_tokens") {
            return ProviderEvent::Incomplete {
                response_id,
                usage,
                reason: "max_tokens".to_owned(),
            };
        }
        ProviderEvent::Completed {
            response_id,
            usage,
            citations: std::mem::take(&mut self.citations),
        }
    }
}

fn tool_event(name: &str, start: bool) -> ProviderEvent {
    let (name, details) = (name.to_owned(), json!({}));
    if start {
        ProviderEvent::ToolStart { name, details }
    } else {
        ProviderEvent::ToolDone { name, details }
    }
}

/// A web search citation of a text block.
fn citation(c: &Value) -> Option<RawCitation> {
    let text = |key: &str| c.get(key).and_then(Value::as_str).map(str::to_owned);
    if c.get("type").and_then(Value::as_str)? != "web_search_result_location" {
        return None;
    }
    Some(RawCitation::Web {
        url: text("url")?,
        title: text("title").unwrap_or_default(),
        snippet: text("cited_text").unwrap_or_default(),
        span: None,
    })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use http::Method;
    use mini_chat_sdk::WebSearchContextSize;
    use serde_json::{Value, json};

    use super::*;
    use crate::config::ProviderKind;
    use crate::infra::llm::{
        ContentPart, InputItem, ProviderErrorKind, ProviderEvent, ProviderUsage, RawCitation, Role,
        ToolSpec,
    };
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::fixtures::{chat_target, provider_request};
    use crate::test_support::gateway::{FakeGateway, Responder, SseScript};
    use crate::test_support::stream::anthropic_event as ev;

    const PATH: &str = "/v1/messages";

    fn setup() -> (Arc<FakeGateway>, AnthropicAdapter) {
        let gateway = Arc::new(FakeGateway::new());
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        let adapter = AnthropicAdapter::new(Arc::clone(&gateway) as _, s2s);
        (gateway, adapter)
    }

    fn target() -> ChatTarget {
        ChatTarget {
            api_path_template: PATH.to_owned(),
            ..chat_target(ProviderKind::AnthropicMessages)
        }
    }

    fn message_start() -> SseScript {
        ev(
            "message_start",
            json!({"message": {"id": "msg_1", "type": "message", "role": "assistant",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 2,
                          "cache_creation_input_tokens": 1, "output_tokens": 1}}}),
        )
    }

    fn block_start(index: u64, block: &Value) -> SseScript {
        ev(
            "content_block_start",
            json!({"index": index, "content_block": block}),
        )
    }

    fn block_delta(index: u64, delta: &Value) -> SseScript {
        ev(
            "content_block_delta",
            json!({"index": index, "delta": delta}),
        )
    }

    fn text_delta(index: u64, text: &str) -> SseScript {
        block_delta(index, &json!({"type": "text_delta", "text": text}))
    }

    fn block_stop(index: u64) -> SseScript {
        ev("content_block_stop", json!({"index": index}))
    }

    fn message_delta(stop_reason: &str, output_tokens: i64) -> SseScript {
        ev(
            "message_delta",
            json!({"delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": output_tokens}}),
        )
    }

    fn message_stop() -> SseScript {
        ev("message_stop", json!({}))
    }

    fn usage(output_tokens: i64) -> ProviderUsage {
        ProviderUsage {
            input_tokens: 13,
            output_tokens,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 1,
            reasoning_tokens: 0,
        }
    }

    async fn collect(adapter: &AnthropicAdapter, req: ProviderRequest) -> Vec<ProviderEvent> {
        adapter
            .stream(&target(), req)
            .await
            .expect("stream starts")
            .collect()
            .await
    }

    #[tokio::test]
    async fn anthropic_translation() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                message_start(),
                ev("ping", json!({})),
                block_start(
                    0,
                    &json!({"type": "text", "text": ""})),
                text_delta(0, "Hel"),
                text_delta(0, "lo"),
                block_stop(0),
                block_start(
                    1,
                    &json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
                ),
                block_delta(
                    1,
                    &json!({"type": "input_json_delta", "partial_json": "{\"query\":\"x\"}"})),
                block_stop(1),
                block_start(
                    2,
                    &json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []}),
                ),
                block_stop(2),
                block_start(
                    3,
                    &json!({"type": "server_tool_use", "id": "srvtoolu_2", "name": "code_execution", "input": {}}),
                ),
                block_stop(3),
                message_delta("max_tokens", 7),
                message_stop(),
            ]),
        );

        let mut req = provider_request();
        req.input = vec![
            InputItem::Message {
                role: Role::User,
                parts: vec![
                    ContentPart::Text("what is this".to_owned()),
                    ContentPart::Image {
                        file_id: "file-img".to_owned(),
                        secondary_file_id: Some("file_011sec".to_owned()),
                    },
                    ContentPart::Image {
                        file_id: "file-other".to_owned(),
                        secondary_file_id: None,
                    },
                ],
            },
            InputItem::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text("a cat".to_owned())],
            },
            InputItem::Message {
                role: Role::User,
                parts: vec![ContentPart::Text("more".to_owned())],
            },
        ];
        req.tools = vec![
            ToolSpec::FileSearch {
                vector_store_ids: vec!["vs_1".to_owned()],
                max_num_results: 5,
            },
            ToolSpec::WebSearch {
                search_context_size: WebSearchContextSize::Low,
            },
            ToolSpec::CodeInterpreter {
                file_ids: vec!["file-1".to_owned()],
            },
            ToolSpec::Function {
                name: "search_knowledge".to_owned(),
                description: "look up".to_owned(),
                parameters: json!({"type": "object"}),
            },
        ];
        req.api_params.temperature = Some(0.2);
        req.api_params.stop = vec!["END".to_owned()];
        req.api_params.extra_body = json!({"foo": 1}).as_object().cloned();
        let user = req.user.clone();

        let events = collect(&adapter, req).await;
        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("Hel".to_owned()),
                ProviderEvent::TextDelta("lo".to_owned()),
                ProviderEvent::ToolStart {
                    name: "web_search".to_owned(),
                    details: json!({})
                },
                ProviderEvent::ToolDone {
                    name: "web_search".to_owned(),
                    details: json!({})
                },
                ProviderEvent::ToolStart {
                    name: "code_interpreter".to_owned(),
                    details: json!({})
                },
                ProviderEvent::ToolDone {
                    name: "code_interpreter".to_owned(),
                    details: json!({})
                },
                ProviderEvent::Incomplete {
                    response_id: Some("msg_1".to_owned()),
                    usage: Some(usage(7)),
                    reason: "max_tokens".to_owned(),
                },
            ]
        );

        let recorded = gateway.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].uri, "/llm.test/v1/messages");
        let header = |name: &str| {
            recorded[0]
                .headers
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
        let beta = header("anthropic-beta").expect("beta header");
        assert!(beta.contains("files-api-2025-04-14"), "{beta}");
        let body = recorded[0].json.clone().expect("json body");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
        assert_eq!(body["metadata"], json!({"user_id": user}));
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["stop_sequences"], json!(["END"]));
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image", "source": {"type": "file", "file_id": "file_011sec"}},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "a cat"}]},
                {"role": "user", "content": [{"type": "text", "text": "more"}]},
            ])
        );
        assert_eq!(
            body["tools"],
            json!([
                {"type": "web_search_20250305", "name": "web_search"},
                {"type": "code_execution_20250522", "name": "code_execution"},
                {"name": "search_knowledge", "description": "look up", "input_schema": {"type": "object"}},
            ])
        );
        for absent in ["user", "foo", "input", "instructions", "max_tool_calls"] {
            assert!(body.get(absent).is_none(), "{absent} in {body}");
        }
    }

    #[tokio::test]
    async fn anthropic_translation_completion_citations_and_function_calls() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                message_start(),
                block_start(
                    0,
                    &json!({"type": "text", "text": ""})),
                text_delta(0, "See"),
                block_delta(
                    0,
                    &json!({"type": "citations_delta", "citation": {
                        "type": "web_search_result_location", "url": "https://e.com",
                        "title": "E", "cited_text": "snippet"}}),
                ),
                block_stop(0),
                block_start(
                    1,
                    &json!({"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {}}),
                ),
                block_delta(
                    1,
                    &json!({"type": "input_json_delta", "partial_json": "{\"query\""})),
                block_delta(
                    1,
                    &json!({"type": "input_json_delta", "partial_json": ":\"x\"}"})),
                block_stop(1),
                block_start(
                    2,
                    &json!({"type": "tool_use", "id": "toolu_2", "name": "other", "input": {}}),
                ),
                block_stop(2),
                message_delta("tool_use", 9),
                message_stop(),
            ]),
        );
        assert_eq!(
            collect(&adapter, provider_request()).await,
            vec![
                ProviderEvent::TextDelta("See".to_owned()),
                ProviderEvent::ToolStart {
                    name: "search_knowledge".to_owned(),
                    details: json!({})
                },
                ProviderEvent::FunctionCall {
                    call_id: "toolu_1".to_owned(),
                    name: "search_knowledge".to_owned(),
                    arguments: "{\"query\":\"x\"}".to_owned(),
                },
                ProviderEvent::ToolStart {
                    name: "unknown_tool".to_owned(),
                    details: json!({})
                },
                ProviderEvent::FunctionCall {
                    call_id: "toolu_2".to_owned(),
                    name: "other".to_owned(),
                    arguments: "{}".to_owned(),
                },
                ProviderEvent::Completed {
                    response_id: Some("msg_1".to_owned()),
                    usage: Some(usage(9)),
                    citations: vec![RawCitation::Web {
                        url: "https://e.com".to_owned(),
                        title: "E".to_owned(),
                        snippet: "snippet".to_owned(),
                        span: None,
                    }],
                },
            ]
        );
    }

    #[tokio::test]
    async fn anthropic_translation_history_with_tool_results() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                message_start(),
                message_delta("end_turn", 1),
                message_stop(),
            ]),
        );
        let mut req = provider_request();
        req.instructions = String::new();
        req.input.push(InputItem::FunctionCall {
            call_id: "toolu_1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "{\"query\":\"x\"}".to_owned(),
        });
        req.input.push(InputItem::FunctionCallOutput {
            call_id: "toolu_1".to_owned(),
            output: "found".to_owned(),
        });
        let _ = collect(&adapter, req).await;
        let recorded = gateway.requests();
        let body = recorded[0].json.clone().expect("json body");
        assert!(body.get("system").is_none(), "{body}");
        assert!(recorded[0].headers.get("anthropic-beta").is_none());
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {"query": "x"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"},
                ]},
            ])
        );
    }

    #[tokio::test]
    async fn anthropic_translation_errors_and_complete() {
        // in-stream `error`
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                message_start(),
                ev(
                    "error",
                    json!({"error": {"type": "overloaded_error", "message": "Overloaded msg_0123456789abcdef"}}),
                ),
            ]),
        );
        match collect(&adapter, provider_request()).await.as_slice() {
            [ProviderEvent::Failed(err)] => {
                assert_eq!(err.kind, ProviderErrorKind::Provider);
                assert_eq!(err.message, "Overloaded [provider_id]");
            }
            other => panic!("unexpected events {other:?}"),
        }

        // rate limit error event
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![ev(
                "error",
                json!({"error": {"type": "rate_limit_error", "message": "slow down"}}),
            )]),
        );
        match collect(&adapter, provider_request()).await.as_slice() {
            [ProviderEvent::Failed(err)] => assert_eq!(
                err.kind,
                ProviderErrorKind::RateLimited {
                    retry_after_secs: None
                }
            ),
            other => panic!("unexpected events {other:?}"),
        }

        // non-streaming call (thread summary)
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(
                200,
                json!({"id": "msg_2", "type": "message", "content": [
                    {"type": "text", "text": "sum"}, {"type": "text", "text": "mary"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "cache_read_input_tokens": 2,
                              "cache_creation_input_tokens": 1, "output_tokens": 4}}),
            ),
        );
        let mut req = provider_request();
        req.stream = false;
        let result = adapter.complete(&target(), req).await.expect("complete");
        assert_eq!(result.text, "summary");
        assert_eq!(result.usage, Some(usage(4)));
        assert_eq!(
            gateway.requests()[0].json.as_ref().unwrap()["stream"],
            false
        );
    }
}
