//! Chat Completions adapter (`provider kind = openai_chat_completions`): builds a
//! `/chat/completions` body, sends it through OAGW and translates the chunk stream into
//! [`ProviderEvent`]s as the chunks arrive.
//!
//! Built-in tools (`file_search`, `web_search`, `code_interpreter`) are dropped; function tools
//! are kept. Images are dropped too: Chat Completions cannot reference an uploaded file id as
//! an image.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::ServiceGatewayClientV1;
use oagw_sdk::sse::ServerEvent;
use serde_json::{Map, Value, json};

use super::errors::error_fields;
use super::openai_responses::apply_api_params;
use super::transport::{self, Translate};
use super::types::{
    ChatAdapter, CompletionResult, ContentPart, InputItem, ProviderError, ProviderEvent,
    ProviderEventStream, ProviderRequest, ProviderUsage, Role, ToolSpec,
};
use super::{ChatTarget, S2sContext};

/// Name of the tool events of a function call.
const FUNCTION_CALL_TOOL: &str = "function_call";
/// Terminal data line of the chunk stream.
const DONE_MARKER: &str = "[DONE]";

/// The Chat Completions protocol over the in-process OAGW client.
pub struct OpenAiChatAdapter {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

impl OpenAiChatAdapter {
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
        transport::post_json(
            &self.gateway,
            &self.s2s,
            target,
            &req.model,
            &body,
            stream,
            &[],
        )
        .await
    }
}

#[async_trait]
impl ChatAdapter for OpenAiChatAdapter {
    async fn stream(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        let resp = self.send(target, &req, true).await?;
        let events = transport::event_stream(resp)?;
        Ok(transport::translate_stream(
            events,
            ChunkTranslator::default(),
        ))
    }

    async fn complete(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<CompletionResult, ProviderError> {
        let resp = self.send(target, &req, false).await?;
        parse_completion(&transport::json_body(resp).await?)
    }
}

// ---- request ---------------------------------------------------------------------------------

fn build_body(req: &ProviderRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages(req)));
    body.insert("stream".into(), json!(stream));
    if stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
    body.insert("user".into(), json!(req.user));
    let tools: Vec<Value> = req.tools.iter().filter_map(tool).collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    apply_api_params(&mut body, req);
    // Chat Completions takes the effort as a top-level field.
    if let Some(reasoning) = body.remove("reasoning")
        && let Some(effort) = reasoning.get("effort")
    {
        body.insert("reasoning_effort".into(), effort.clone());
    }
    Value::Object(body)
}

/// The system message first, then the input; consecutive function calls form one assistant
/// message with several `tool_calls`.
fn messages(req: &ProviderRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.input.len() + 1);
    let mut dropped_images = 0_usize;
    if !req.instructions.is_empty() {
        out.push(json!({"role": "system", "content": req.instructions}));
    }
    for item in &req.input {
        match item {
            InputItem::Message { role, parts } => {
                let role = match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let text: String = parts
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text(text) => Some(text.as_str()),
                        ContentPart::Image { .. } => {
                            dropped_images += 1;
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(json!({"role": role, "content": text}));
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let call = json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                });
                let previous_calls = out
                    .last_mut()
                    .filter(|m| m["role"] == "assistant")
                    .and_then(|m| m.get_mut("tool_calls"))
                    .and_then(Value::as_array_mut);
                match previous_calls {
                    Some(calls) => calls.push(call),
                    None => out
                        .push(json!({"role": "assistant", "content": null, "tool_calls": [call]})),
                }
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
        }
    }
    if dropped_images > 0 {
        tracing::warn!(
            dropped_images,
            model = %req.model,
            "Chat Completions cannot reference uploaded images: image parts dropped"
        );
    }
    out
}

/// Function tools only; the built-in tools have no Chat Completions equivalent.
fn tool(spec: &ToolSpec) -> Option<Value> {
    match spec {
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => Some(json!({"type": "function", "function": {
            "name": name, "description": description, "parameters": parameters,
        }})),
        ToolSpec::FileSearch { .. }
        | ToolSpec::WebSearch { .. }
        | ToolSpec::CodeInterpreter { .. } => None,
    }
}

// ---- responses -------------------------------------------------------------------------------

fn parse_completion(value: &Value) -> Result<CompletionResult, ProviderError> {
    if let Some(err) = in_band_error(value) {
        return Err(err);
    }
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(CompletionResult {
        text,
        usage: value.get("usage").and_then(parse_usage),
    })
}

fn parse_usage(usage: &Value) -> Option<ProviderUsage> {
    if !usage.is_object() {
        return None;
    }
    let number = |pointer: &str| usage.pointer(pointer).and_then(Value::as_i64).unwrap_or(0);
    Some(ProviderUsage {
        input_tokens: number("/prompt_tokens"),
        output_tokens: number("/completion_tokens"),
        cache_read_input_tokens: number("/prompt_tokens_details/cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: number("/completion_tokens_details/reasoning_tokens"),
    })
}

/// A chunk / response carrying an `error` object.
fn in_band_error(value: &Value) -> Option<ProviderError> {
    value.get("error").filter(|e| !e.is_null())?;
    let (_, message) = error_fields(value.to_string().as_bytes());
    Some(ProviderError::provider(
        message.unwrap_or_else(|| "provider request failed".to_owned()),
    ))
}

/// A tool call being streamed (by its `index`).
#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Per-response state: the response id, the finish reason, the usage chunk and the tool calls
/// being assembled. The terminal event is produced at `[DONE]` (the usage chunk follows the
/// finish chunk).
#[derive(Default)]
struct ChunkTranslator {
    response_id: Option<String>,
    finish_reason: Option<String>,
    usage: Option<ProviderUsage>,
    calls: BTreeMap<u64, PendingCall>,
}

impl Translate for ChunkTranslator {
    fn translate(&mut self, raw: &ServerEvent) -> Vec<ProviderEvent> {
        let data = raw.data.trim();
        if data == DONE_MARKER {
            return vec![self.terminal()];
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        if let Some(err) = in_band_error(&chunk) {
            return vec![ProviderEvent::Failed(err)];
        }
        if self.response_id.is_none() {
            self.response_id = chunk.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(usage) = chunk.get("usage").and_then(parse_usage) {
            self.usage = Some(usage);
        }
        let mut out = Vec::new();
        let Some(choice) = chunk.pointer("/choices/0") else {
            return out;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                out.push(ProviderEvent::TextDelta(text.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.extend(self.tool_call_delta(call));
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
            out.extend(self.finish_calls());
        }
        out
    }
}

impl ChunkTranslator {
    /// Adds a `tool_calls[]` delta; the first delta of a call (with its id and name) starts it.
    fn tool_call_delta(&mut self, call: &Value) -> Option<ProviderEvent> {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let text = |pointer: &str| call.pointer(pointer).and_then(Value::as_str);
        let is_new = !self.calls.contains_key(&index);
        let pending = self.calls.entry(index).or_default();
        if let Some(id) = text("/id") {
            id.clone_into(&mut pending.call_id);
        }
        if let Some(name) = text("/function/name") {
            pending.name.push_str(name);
        }
        if let Some(arguments) = text("/function/arguments") {
            pending.arguments.push_str(arguments);
        }
        is_new.then(|| ProviderEvent::ToolStart {
            name: FUNCTION_CALL_TOOL.to_owned(),
            details: json!({"index": index, "call_id": pending.call_id, "name": pending.name}),
        })
    }

    /// The assembled calls, each as a tool `done` event and a function call.
    fn finish_calls(&mut self) -> Vec<ProviderEvent> {
        std::mem::take(&mut self.calls)
            .into_values()
            .flat_map(|call| {
                let arguments = if call.arguments.is_empty() {
                    "{}".to_owned()
                } else {
                    call.arguments
                };
                [
                    ProviderEvent::ToolDone {
                        name: FUNCTION_CALL_TOOL.to_owned(),
                        details: json!({"call_id": call.call_id, "name": call.name,
                                        "arguments": arguments}),
                    },
                    ProviderEvent::FunctionCall {
                        call_id: call.call_id,
                        name: call.name,
                        arguments,
                    },
                ]
            })
            .collect()
    }

    /// `[DONE]`: `length` is a truncated answer, anything else a completed one.
    fn terminal(&mut self) -> ProviderEvent {
        let response_id = self.response_id.take();
        let usage = self.usage.take();
        if self.finish_reason.as_deref() == Some("length") {
            return ProviderEvent::Incomplete {
                response_id,
                usage,
                reason: "max_tokens".to_owned(),
            };
        }
        ProviderEvent::Completed {
            response_id,
            usage,
            citations: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use http::Method;
    use mini_chat_sdk::WebSearchContextSize;
    use serde_json::{Value, json};

    use super::*;
    use crate::config::ProviderKind;
    use crate::infra::llm::{ContentPart, InputItem, ProviderEvent, ProviderUsage, Role, ToolSpec};
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::fixtures::{chat_target, provider_request};
    use crate::test_support::gateway::{FakeGateway, Responder, SseScript};

    const PATH: &str = "/v1/chat/completions";

    fn setup() -> (Arc<FakeGateway>, OpenAiChatAdapter) {
        let gateway = Arc::new(FakeGateway::new());
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        let adapter = OpenAiChatAdapter::new(Arc::clone(&gateway) as _, s2s);
        (gateway, adapter)
    }

    fn target() -> ChatTarget {
        ChatTarget {
            api_path_template: PATH.to_owned(),
            ..chat_target(ProviderKind::OpenaiChatCompletions)
        }
    }

    /// A data-only chunk.
    fn chunk(data: &Value) -> SseScript {
        SseScript::Raw(format!("data: {data}\n\n"))
    }

    fn done() -> SseScript {
        SseScript::Raw("data: [DONE]\n\n".to_owned())
    }

    fn delta(content: &str) -> SseScript {
        chunk(
            &json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]}),
        )
    }

    fn finish(reason: &str) -> SseScript {
        chunk(
            &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}),
        )
    }

    fn usage_chunk() -> SseScript {
        chunk(&json!({"id": "chatcmpl-1", "choices": [], "usage": {
            "prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17,
            "prompt_tokens_details": {"cached_tokens": 3},
            "completion_tokens_details": {"reasoning_tokens": 1},
        }}))
    }

    fn usage() -> ProviderUsage {
        ProviderUsage {
            input_tokens: 12,
            output_tokens: 5,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 0,
            reasoning_tokens: 1,
        }
    }

    async fn collect(adapter: &OpenAiChatAdapter, req: ProviderRequest) -> Vec<ProviderEvent> {
        adapter
            .stream(&target(), req)
            .await
            .expect("stream starts")
            .collect()
            .await
    }

    #[tokio::test]
    async fn openai_chat_translation() {
        // text, usage (sent after the finish chunk), `[DONE]`
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                chunk(&json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]})),
                delta("Hel"),
                delta("lo"),
                finish("stop"),
                usage_chunk(),
                done(),
            ]),
        );
        assert_eq!(
            collect(&adapter, provider_request()).await,
            vec![
                ProviderEvent::TextDelta("Hel".to_owned()),
                ProviderEvent::TextDelta("lo".to_owned()),
                ProviderEvent::Completed {
                    response_id: Some("chatcmpl-1".to_owned()),
                    usage: Some(usage()),
                    citations: vec![],
                },
            ]
        );

        // finish_reason `length` → incomplete `max_tokens`
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![delta("cut"), finish("length"), usage_chunk(), done()]),
        );
        assert_eq!(
            collect(&adapter, provider_request()).await,
            vec![
                ProviderEvent::TextDelta("cut".to_owned()),
                ProviderEvent::Incomplete {
                    response_id: Some("chatcmpl-1".to_owned()),
                    usage: Some(usage()),
                    reason: "max_tokens".to_owned(),
                },
            ]
        );

        // a stream without `[DONE]` is a failure
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![delta("a"), finish("stop")]),
        );
        let events = collect(&adapter, provider_request()).await;
        assert!(
            matches!(events.last(), Some(ProviderEvent::Failed(_))),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn openai_chat_translation_request_body_drops_built_in_tools() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![finish("stop"), done()]),
        );
        let mut req = provider_request();
        req.input = vec![
            InputItem::Message {
                role: Role::User,
                parts: vec![ContentPart::Text("q1".to_owned())],
            },
            InputItem::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text("a1".to_owned())],
            },
            InputItem::Message {
                role: Role::User,
                parts: vec![ContentPart::Text("q2".to_owned())],
            },
            InputItem::FunctionCall {
                call_id: "call_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"x\"}".to_owned(),
            },
            InputItem::FunctionCallOutput {
                call_id: "call_1".to_owned(),
                output: "found".to_owned(),
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
        req.api_params.temperature = Some(0.5);
        req.api_params.extra_body = json!({"foo": 1, "messages": "x"}).as_object().cloned();
        let user = req.user.clone();
        let _ = collect(&adapter, req).await;

        let recorded = gateway.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].uri, "/llm.test/v1/chat/completions");
        let body = recorded[0].json.clone().expect("json body");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(body["max_completion_tokens"], 1024);
        assert_eq!(body["user"], user);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["foo"], 1);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": "a1"},
                {"role": "user", "content": "q2"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "search_knowledge", "arguments": "{\"query\":\"x\"}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "found"},
            ])
        );
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "function": {
                "name": "search_knowledge", "description": "look up", "parameters": {"type": "object"},
            }}])
        );
        for absent in [
            "metadata",
            "max_tool_calls",
            "input",
            "instructions",
            "store",
        ] {
            assert!(body.get(absent).is_none(), "{absent} must be absent");
        }

        // only built-in tools: no `tools` key at all
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![finish("stop"), done()]),
        );
        let mut req = provider_request();
        req.tools = vec![ToolSpec::WebSearch {
            search_context_size: WebSearchContextSize::Low,
        }];
        let _ = collect(&adapter, req).await;
        let body = gateway.requests()[0].json.clone().expect("json body");
        assert!(body.get("tools").is_none(), "{body}");
    }

    #[tokio::test]
    async fn openai_chat_translation_tool_calls() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                chunk(
                    &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_1", "type": "function",
                         "function": {"name": "search_knowledge", "arguments": ""}},
                    ]}}]}),
                ),
                chunk(
                    &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "function": {"arguments": "{\"query\""}},
                    ]}}]}),
                ),
                chunk(
                    &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "function": {"arguments": ":\"x\"}"}},
                    ]}}]}),
                ),
                finish("tool_calls"),
                usage_chunk(),
                done(),
            ]),
        );
        assert_eq!(
            collect(&adapter, provider_request()).await,
            vec![
                ProviderEvent::ToolStart {
                    name: "function_call".to_owned(),
                    details: json!({"index": 0, "call_id": "call_1", "name": "search_knowledge"}),
                },
                ProviderEvent::ToolDone {
                    name: "function_call".to_owned(),
                    details: json!({"call_id": "call_1", "name": "search_knowledge",
                                    "arguments": "{\"query\":\"x\"}"}),
                },
                ProviderEvent::FunctionCall {
                    call_id: "call_1".to_owned(),
                    name: "search_knowledge".to_owned(),
                    arguments: "{\"query\":\"x\"}".to_owned(),
                },
                ProviderEvent::Completed {
                    response_id: Some("chatcmpl-1".to_owned()),
                    usage: Some(usage()),
                    citations: vec![],
                },
            ]
        );
    }

    #[tokio::test]
    async fn openai_chat_translation_complete_and_errors() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(
                200,
                json!({"id": "chatcmpl-2", "choices": [{"index": 0,
                    "message": {"role": "assistant", "content": "summary"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 5,
                              "prompt_tokens_details": {"cached_tokens": 3},
                              "completion_tokens_details": {"reasoning_tokens": 1}}}),
            ),
        );
        let mut req = provider_request();
        req.stream = false;
        let result = adapter.complete(&target(), req).await.expect("complete");
        assert_eq!(result.text, "summary");
        assert_eq!(result.usage, Some(usage()));
        assert_eq!(
            gateway.requests()[0].json.as_ref().unwrap()["stream"],
            false
        );
        assert!(
            gateway.requests()[0]
                .json
                .as_ref()
                .unwrap()
                .get("stream_options")
                .is_none()
        );

        // an in-stream error object fails the stream with its sanitized message
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![chunk(
                &json!({"error": {"message": "bad file-abcdefghijklmnop", "code": "x"}}),
            )]),
        );
        let events = collect(&adapter, provider_request()).await;
        match events.as_slice() {
            [ProviderEvent::Failed(err)] => assert_eq!(err.message, "bad [provider_id]"),
            other => panic!("unexpected events {other:?}"),
        }
    }
}
