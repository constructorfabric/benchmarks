//! `OpenAI` Responses API adapter (`provider kind = openai_responses`): builds the request body,
//! sends it through OAGW and translates the provider SSE stream into [`ProviderEvent`]s as the
//! events arrive (nothing is buffered).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::sse::ServerEvent;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::{Map, Value, json};

use super::transport::{self, Translate};
use super::types::{
    ChatAdapter, CompletionResult, ContentPart, InputItem, ProviderError, ProviderEvent,
    ProviderEventStream, ProviderRequest, ProviderUsage, RawCitation, Role, ToolSpec,
};
use super::{ChatTarget, S2sContext};
use crate::domain::sanitize::sanitize_provider_message;

/// Longest `output` of a `code_interpreter` tool-done event, in characters.
const CODE_OUTPUT_LIMIT: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// Request keys the adapter controls; `extra_body` entries with these keys are ignored.
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

/// The `OpenAI` Responses protocol over the in-process OAGW client.
pub struct OpenAiResponsesAdapter {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

impl OpenAiResponsesAdapter {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: S2sContext) -> Self {
        Self { gateway, s2s }
    }

    async fn send(
        &self,
        target: &ChatTarget,
        req: &ProviderRequest,
        stream: bool,
    ) -> Result<http::Response<Body>, ProviderError> {
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
impl ChatAdapter for OpenAiResponsesAdapter {
    async fn stream(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        let resp = self.send(target, &req, true).await?;
        let events = transport::event_stream(resp)?;
        Ok(transport::translate_stream(events, Translator::default()))
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

/// The Responses request body (shared with the vLLM Responses adapter).
pub(super) fn build_body(req: &ProviderRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("instructions".into(), json!(req.instructions));
    body.insert(
        "input".into(),
        Value::Array(req.input.iter().map(input_item).collect()),
    );
    body.insert("stream".into(), json!(stream));
    body.insert("store".into(), json!(false));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
    body.insert("user".into(), json!(req.user));
    body.insert("metadata".into(), metadata(req));
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(tool).collect()),
        );
    }
    if req
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
    {
        body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
    }
    apply_api_params(&mut body, req);
    Value::Object(body)
}

fn metadata(req: &ProviderRequest) -> Value {
    let meta = &req.metadata;
    let mut out = Map::new();
    out.insert("tenant_id".into(), json!(meta.tenant_id.to_string()));
    out.insert("user_id".into(), json!(meta.user_id.to_string()));
    if let Some(chat_id) = meta.chat_id {
        out.insert("chat_id".into(), json!(chat_id.to_string()));
    }
    out.insert("request_type".into(), json!(meta.request_type));
    out.insert("feature".into(), json!(meta.feature));
    Value::Object(out)
}

/// Sampling parameters, `stop`, `reasoning.effort` and the `extra_body` keys the request does
/// not control (shared with the Chat Completions adapter, which maps `reasoning` itself).
pub(super) fn apply_api_params(body: &mut Map<String, Value>, req: &ProviderRequest) {
    let params = &req.api_params;
    for (key, value) in [
        ("temperature", params.temperature),
        ("top_p", params.top_p),
        ("frequency_penalty", params.frequency_penalty),
        ("presence_penalty", params.presence_penalty),
    ] {
        if let Some(value) = value {
            body.insert(key.into(), json!(value));
        }
    }
    if !params.stop.is_empty() {
        body.insert("stop".into(), json!(params.stop));
    }
    if let Some(effort) = &params.reasoning_effort {
        body.insert("reasoning".into(), json!({ "effort": effort }));
    }
    if let Some(extra) = &params.extra_body {
        for (key, value) in extra {
            if CONTROLLED_KEYS.contains(&key.as_str()) {
                tracing::warn!(key = %key, "ignoring extra_body key controlled by the request");
            } else {
                body.insert(key.clone(), value.clone());
            }
        }
    }
}

fn input_item(item: &InputItem) -> Value {
    match item {
        InputItem::Message { role, parts } => {
            let (role, text_type) = match role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            let content: Vec<Value> = parts
                .iter()
                .map(|part| match part {
                    ContentPart::Text(text) => json!({"type": text_type, "text": text}),
                    ContentPart::Image { file_id, .. } => {
                        json!({"type": "input_image", "file_id": file_id})
                    }
                })
                .collect();
            json!({"role": role, "content": content})
        }
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

fn tool(spec: &ToolSpec) -> Value {
    match spec {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({"type": "web_search", "search_context_size": search_context_size}),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
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

// ---- non-streaming response ------------------------------------------------------------------

/// Text and usage of a non-streaming Responses answer (shared with the vLLM adapter).
pub(super) fn parse_completion(value: &Value) -> Result<CompletionResult, ProviderError> {
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        return Err(ProviderError::provider(error_message(error)));
    }
    let text = value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    Ok(CompletionResult {
        text,
        usage: value.get("usage").and_then(parse_usage),
    })
}

// ---- streaming translation -------------------------------------------------------------------

/// `(output_index, content_index)` of an `output_text` part.
type PartKey = (u64, u64);

/// Per-response translation state: the text of each part and the streamed annotations, needed to
/// build citations at the end.
#[derive(Default)]
pub(super) struct Translator {
    texts: HashMap<PartKey, String>,
    annotations: Vec<(PartKey, Value)>,
}

impl Translate for Translator {
    fn translate(&mut self, raw: &ServerEvent) -> Vec<ProviderEvent> {
        self.translate_one(raw).into_iter().collect()
    }
}

impl Translator {
    /// Translates one SSE event; `None` for events that produce nothing for the client.
    fn translate_one(&mut self, raw: &ServerEvent) -> Option<ProviderEvent> {
        let data: Option<Value> = serde_json::from_str(raw.data.trim()).ok();
        let name = match raw
            .event
            .as_deref()
            .filter(|n| !n.is_empty() && *n != "message")
        {
            Some(name) => name.to_owned(),
            None => data
                .as_ref()?
                .get("type")
                .and_then(Value::as_str)?
                .to_owned(),
        };
        if name == "error" {
            return Some(ProviderEvent::Failed(error_event(
                raw.data.trim(),
                data.as_ref(),
            )));
        }
        self.dispatch(&name, &data?)
    }

    fn dispatch(&mut self, name: &str, data: &Value) -> Option<ProviderEvent> {
        match name {
            "response.output_text.delta" => {
                let delta = data.get("delta").and_then(Value::as_str)?;
                self.texts
                    .entry(part_key(data))
                    .or_default()
                    .push_str(delta);
                (!delta.is_empty()).then(|| ProviderEvent::TextDelta(delta.to_owned()))
            }
            "response.output_text.done" => {
                if let Some(text) = data.get("text").and_then(Value::as_str) {
                    self.texts.insert(part_key(data), text.to_owned());
                }
                None
            }
            "response.output_text.annotation.added" => {
                if let Some(annotation) = data.get("annotation") {
                    self.annotations.push((part_key(data), annotation.clone()));
                }
                None
            }
            "response.file_search_call.searching" => Some(tool_start("file_search")),
            "response.file_search_call.completed" => {
                let searched = data
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Some(tool_done(
                    "file_search",
                    json!({"files_searched": searched}),
                ))
            }
            "response.web_search_call.searching" => Some(tool_start("web_search")),
            "response.web_search_call.completed" => Some(tool_done("web_search", json!({}))),
            "response.code_interpreter_call.in_progress" => Some(tool_start("code_interpreter")),
            "response.output_item.done" => output_item_done(data.get("item")?),
            "response.completed" => Some(self.completed(data)),
            "response.incomplete" => Some(incomplete(data)),
            "response.failed" => Some(ProviderEvent::Failed(failed(data))),
            _ => None,
        }
    }

    fn completed(&self, data: &Value) -> ProviderEvent {
        let response = data.get("response");
        let citations = if self.annotations.is_empty() {
            response.map(output_citations).unwrap_or_default()
        } else {
            self.annotations
                .iter()
                .filter_map(|(key, annotation)| {
                    citation(annotation, self.texts.get(key).map(String::as_str))
                })
                .collect()
        };
        ProviderEvent::Completed {
            response_id: response_id(response),
            usage: response.and_then(|r| r.get("usage")).and_then(parse_usage),
            citations,
        }
    }
}

fn part_key(data: &Value) -> PartKey {
    let index = |field: &str| data.get(field).and_then(Value::as_u64).unwrap_or(0);
    (index("output_index"), index("content_index"))
}

fn tool_start(name: &str) -> ProviderEvent {
    ProviderEvent::ToolStart {
        name: name.to_owned(),
        details: json!({}),
    }
}

fn tool_done(name: &str, details: Value) -> ProviderEvent {
    ProviderEvent::ToolDone {
        name: name.to_owned(),
        details,
    }
}

fn output_item_done(item: &Value) -> Option<ProviderEvent> {
    let text = |field: &str| item.get(field).and_then(Value::as_str).map(str::to_owned);
    match item.get("type").and_then(Value::as_str)? {
        "code_interpreter_call" => Some(tool_done(
            "code_interpreter",
            json!({"output": code_output(item)}),
        )),
        "function_call" => Some(ProviderEvent::FunctionCall {
            call_id: text("call_id").or_else(|| text("id"))?,
            name: text("name")?,
            arguments: text("arguments").unwrap_or_else(|| "{}".to_owned()),
        }),
        _ => None,
    }
}

/// The `logs` outputs of a code-interpreter call joined with newlines, capped.
fn code_output(item: &Value) -> String {
    let logs = item
        .get("outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
        .filter_map(|o| o.get("logs").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    match logs.char_indices().nth(CODE_OUTPUT_LIMIT) {
        Some((cut, _)) => format!("{}{TRUNCATED_SUFFIX}", &logs[..cut]),
        None => logs,
    }
}

fn response_id(response: Option<&Value>) -> Option<String> {
    response?.get("id")?.as_str().map(str::to_owned)
}

fn incomplete(data: &Value) -> ProviderEvent {
    let response = data.get("response");
    ProviderEvent::Incomplete {
        response_id: response_id(response),
        usage: response.and_then(|r| r.get("usage")).and_then(parse_usage),
        reason: response
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
    }
}

fn parse_usage(usage: &Value) -> Option<ProviderUsage> {
    if !usage.is_object() {
        return None;
    }
    let number = |value: Option<&Value>| value.and_then(Value::as_i64).unwrap_or(0);
    Some(ProviderUsage {
        input_tokens: number(usage.get("input_tokens")),
        output_tokens: number(usage.get("output_tokens")),
        cache_read_input_tokens: number(
            usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens")),
        ),
        cache_write_input_tokens: 0,
        reasoning_tokens: number(
            usage
                .get("output_tokens_details")
                .and_then(|d| d.get("reasoning_tokens")),
        ),
    })
}

// ---- errors ----------------------------------------------------------------------------------

/// `response.failed`: the error of `response.error`, else the top-level `error`; usage is kept.
fn failed(data: &Value) -> ProviderError {
    let response = data.get("response");
    let error = response
        .and_then(|r| r.get("error"))
        .filter(|e| !e.is_null())
        .or_else(|| data.get("error").filter(|e| !e.is_null()));
    ProviderError::provider(
        error.map_or_else(|| "provider request failed".to_owned(), error_message),
    )
    .with_usage(response.and_then(|r| r.get("usage")).and_then(parse_usage))
}

/// The sanitized `message` of an error object, with a generic fallback.
fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map_or_else(
            || "provider request failed".to_owned(),
            sanitize_provider_message,
        )
}

/// `error` event: parsed like `response.failed`, then as flat `{code, message}`; unparseable
/// data becomes the message.
fn error_event(raw: &str, data: Option<&Value>) -> ProviderError {
    let Some(data) = data.filter(|d| d.is_object()) else {
        return ProviderError::provider(if raw.is_empty() {
            "provider request failed".to_owned()
        } else {
            sanitize_provider_message(raw)
        });
    };
    if data.get("response").is_some() || data.get("error").is_some_and(Value::is_object) {
        return failed(data);
    }
    ProviderError::provider(error_message(data))
}

// ---- citations -------------------------------------------------------------------------------

/// Citations of the final `response.output[*].content[*].annotations`.
fn output_citations(response: &Value) -> Vec<RawCitation> {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .flat_map(|part| {
            let text = part.get("text").and_then(Value::as_str);
            part.get("annotations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |annotation| citation(annotation, text))
        })
        .collect()
}

/// A citation from one annotation; `part_text` is the `output_text` part it belongs to.
fn citation(annotation: &Value, part_text: Option<&str>) -> Option<RawCitation> {
    let text = |field: &str| {
        annotation
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match annotation.get("type").and_then(Value::as_str)? {
        "url_citation" => {
            let span = annotation_span(annotation);
            let snippet = text("text").unwrap_or_else(|| {
                span.and_then(|(start, end)| char_range(part_text?, start, end))
                    .unwrap_or_default()
            });
            Some(RawCitation::Web {
                url: text("url")?,
                title: text("title").unwrap_or_default(),
                snippet,
                span,
            })
        }
        "file_citation" => Some(RawCitation::File {
            provider_file_id: text("file_id")?,
            filename: text("filename").unwrap_or_default(),
        }),
        _ => None,
    }
}

fn annotation_span(annotation: &Value) -> Option<(usize, usize)> {
    let index = |field: &str| {
        annotation
            .get(field)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    };
    Some((index("start_index")?, index("end_index")?))
}

/// The characters `start..end` of `text`; `None` when the range is out of bounds.
fn char_range(text: &str, start: usize, end: usize) -> Option<String> {
    if start > end || end > text.chars().count() {
        return None;
    }
    Some(text.chars().skip(start).take(end - start).collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use http::Method;
    use mini_chat_sdk::WebSearchContextSize;
    use toolkit_canonical_errors::{CanonicalError, resource_error};

    use super::*;
    use crate::config::ProviderKind;
    use crate::infra::llm::types::ProviderErrorKind;
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::fixtures::{chat_target, provider_request};
    use crate::test_support::gateway::{FakeGateway, Responder, SseScript};

    const PATH: &str = "/v1/responses";

    #[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
    struct ProxyError;

    fn setup() -> (Arc<FakeGateway>, OpenAiResponsesAdapter) {
        let gateway = Arc::new(FakeGateway::new());
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        let adapter = OpenAiResponsesAdapter::new(Arc::clone(&gateway) as _, s2s);
        (gateway, adapter)
    }

    fn target() -> ChatTarget {
        chat_target(ProviderKind::OpenaiResponses)
    }

    fn sse(name: &str, data: Value) -> SseScript {
        SseScript::event(name, data)
    }

    fn completed(usage: &Value) -> SseScript {
        sse(
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "usage": usage}}),
        )
    }

    async fn collect(adapter: &OpenAiResponsesAdapter) -> Vec<ProviderEvent> {
        adapter
            .stream(&target(), provider_request())
            .await
            .expect("stream starts")
            .collect()
            .await
    }

    async fn stream_error(adapter: &OpenAiResponsesAdapter) -> ProviderError {
        match adapter.stream(&target(), provider_request()).await {
            Err(err) => err,
            Ok(_) => panic!("expected the call to fail"),
        }
    }

    #[tokio::test]
    async fn request_body_shape() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![completed(&json!({}))]),
        );

        let mut req = provider_request();
        req.input = vec![
            InputItem::Message {
                role: Role::User,
                parts: vec![
                    ContentPart::Text("what is this".to_owned()),
                    ContentPart::Image {
                        file_id: "file-img".to_owned(),
                        secondary_file_id: Some("file_sec".to_owned()),
                    },
                ],
            },
            InputItem::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text("a cat".to_owned())],
            },
            InputItem::FunctionCall {
                call_id: "call_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"q\":1}".to_owned(),
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
        req.api_params.extra_body = json!({"foo": 1, "model": "x"}).as_object().cloned();
        req.api_params.stop = vec!["END".to_owned()];
        req.api_params.reasoning_effort = Some("low".to_owned());
        let user = req.user.clone();
        let _ = adapter
            .stream(&target(), req)
            .await
            .expect("stream")
            .count()
            .await;

        let recorded = gateway.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].uri, "/llm.test/v1/responses");
        assert_eq!(
            recorded[0]
                .headers
                .get(http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("application/json")
        );
        let body = recorded[0].json.clone().expect("json body");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(body["max_output_tokens"], 1024);
        assert_eq!(body["max_tool_calls"], 2);
        assert_eq!(body["user"], user);
        assert_eq!(body["user"].as_str().unwrap().len(), 64);
        assert_eq!(body["metadata"]["request_type"], "chat");
        assert_eq!(body["metadata"]["feature"], "none");
        assert_eq!(
            body["metadata"]["tenant_id"],
            "00000000-0000-0000-0000-0000000000a1"
        );
        assert_eq!(
            body["metadata"]["chat_id"],
            "00000000-0000-0000-0000-0000000000c3"
        );
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [
                    {"type": "input_text", "text": "what is this"},
                    {"type": "input_image", "file_id": "file-img"},
                ]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "a cat"}]},
                {"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"q\":1}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "found"},
            ])
        );
        assert_eq!(
            body["tools"],
            json!([
                {"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5},
                {"type": "web_search", "search_context_size": "low"},
                {"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-1"]}},
                {"type": "function", "name": "search_knowledge", "description": "look up", "parameters": {"type": "object"}},
            ])
        );
        assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
        assert_eq!(body["foo"], 1);
        assert_eq!(body["stop"], json!(["END"]));
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[tokio::test]
    async fn optional_body_parts_are_omitted_when_unset() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![completed(&json!({}))]),
        );
        let mut req = provider_request();
        req.metadata.chat_id = None;
        req.api_params.temperature = Some(0.5);
        let _ = adapter
            .stream(&target(), req)
            .await
            .expect("stream")
            .count()
            .await;

        let body = gateway.requests()[0].json.clone().expect("json body");
        for key in ["tools", "include", "stop", "reasoning", "top_p"] {
            assert!(body.get(key).is_none(), "{key} must be absent");
        }
        assert!(body["metadata"].get("chat_id").is_none());
        assert_eq!(body["temperature"], 0.5);
    }

    #[tokio::test]
    async fn translates_text_tools_and_completion() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                sse("response.created", json!({"type": "response.created", "response": {"id": "resp_1"}})),
                sse("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "Hel"})),
                sse("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "lo"})),
                sse("response.file_search_call.searching", json!({"type": "response.file_search_call.searching"})),
                sse("response.file_search_call.completed", json!({"type": "response.file_search_call.completed", "results": [{"file_id": "file-1"}, {"file_id": "file-2"}]})),
                sse("response.web_search_call.searching", json!({"type": "response.web_search_call.searching"})),
                sse("response.web_search_call.completed", json!({"type": "response.web_search_call.completed"})),
                sse("response.code_interpreter_call.in_progress", json!({"type": "response.code_interpreter_call.in_progress"})),
                sse("response.code_interpreter_call.interpreting", json!({"type": "response.code_interpreter_call.interpreting"})),
                sse("response.output_item.done", json!({"type": "response.output_item.done", "item": {"type": "message", "content": []}})),
                sse("response.output_item.done", json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "call_9", "name": "search_knowledge", "arguments": "{\"q\":\"x\"}"}})),
                completed(&json!({
                    "input_tokens": 12,
                    "output_tokens": 5,
                    "input_tokens_details": {"cached_tokens": 3},
                    "output_tokens_details": {"reasoning_tokens": 1},
                })),
            ]),
        );

        let events = collect(&adapter).await;
        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("Hel".to_owned()),
                ProviderEvent::TextDelta("lo".to_owned()),
                ProviderEvent::ToolStart {
                    name: "file_search".to_owned(),
                    details: json!({})
                },
                ProviderEvent::ToolDone {
                    name: "file_search".to_owned(),
                    details: json!({"files_searched": 2})
                },
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
                ProviderEvent::FunctionCall {
                    call_id: "call_9".to_owned(),
                    name: "search_knowledge".to_owned(),
                    arguments: "{\"q\":\"x\"}".to_owned(),
                },
                ProviderEvent::Completed {
                    response_id: Some("resp_1".to_owned()),
                    usage: Some(ProviderUsage {
                        input_tokens: 12,
                        output_tokens: 5,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: 1,
                    }),
                    citations: vec![],
                },
            ]
        );
        assert_eq!(gateway.dropped_bodies(), 0);
    }

    #[tokio::test]
    async fn events_are_yielded_before_the_provider_finishes() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                sse(
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta", "delta": "first"}),
                ),
                SseScript::Hang,
            ]),
        );
        let mut stream = adapter
            .stream(&target(), provider_request())
            .await
            .expect("stream");
        let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("first event arrives while the provider keeps the body open");
        assert_eq!(first, Some(ProviderEvent::TextDelta("first".to_owned())));

        drop(stream);
        assert_eq!(
            gateway.dropped_bodies(),
            1,
            "dropping the stream cancels the body"
        );
    }

    #[tokio::test]
    async fn event_name_from_type_field() {
        let (gateway, adapter) = setup();
        let data_only = |v: Value| SseScript::Raw(format!("data: {v}\n\n"));
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                data_only(json!({"type": "response.output_text.delta", "delta": "a"})),
                SseScript::Raw(format!(
                    "event: message\ndata: {}\n\n",
                    json!({"type": "response.web_search_call.searching"})
                )),
                data_only(json!({"type": "response.some.unknown.event"})),
                SseScript::Raw(": keep-alive\n\n".to_owned()),
                data_only(json!({"type": "response.completed", "response": {"id": "resp_2"}})),
            ]),
        );

        let events = collect(&adapter).await;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], ProviderEvent::TextDelta("a".to_owned()));
        assert_eq!(
            events[1],
            ProviderEvent::ToolStart {
                name: "web_search".to_owned(),
                details: json!({})
            }
        );
        assert_eq!(
            events[2],
            ProviderEvent::Completed {
                response_id: Some("resp_2".to_owned()),
                usage: None,
                citations: vec![],
            }
        );
    }

    #[tokio::test]
    async fn citations_from_annotations() {
        let (gateway, adapter) = setup();
        let text = "Rust is a systems language. See the docs.";
        let (start, end) = (0, 4);
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                sse("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": text})),
                sse("response.output_text.annotation.added", json!({
                    "type": "response.output_text.annotation.added",
                    "output_index": 0, "content_index": 0, "annotation_index": 0,
                    "annotation": {"type": "url_citation", "url": "https://rust-lang.org", "title": "Rust", "start_index": start, "end_index": end},
                })),
                sse("response.output_text.annotation.added", json!({
                    "type": "response.output_text.annotation.added",
                    "output_index": 0, "content_index": 0, "annotation_index": 1,
                    "annotation": {"type": "url_citation", "url": "https://x.example", "title": "X", "start_index": 500, "end_index": 600},
                })),
                sse("response.output_text.annotation.added", json!({
                    "type": "response.output_text.annotation.added",
                    "output_index": 0, "content_index": 0, "annotation_index": 2,
                    "annotation": {"type": "file_citation", "file_id": "file-abc", "filename": "doc.pdf", "index": 3},
                })),
                completed(&json!({})),
            ]),
        );

        let events = collect(&adapter).await;
        let Some(ProviderEvent::Completed { citations, .. }) = events.last() else {
            panic!("expected completion, got {events:?}");
        };
        assert_eq!(
            citations,
            &vec![
                RawCitation::Web {
                    url: "https://rust-lang.org".to_owned(),
                    title: "Rust".to_owned(),
                    snippet: text[start..end].to_owned(),
                    span: Some((start, end)),
                },
                RawCitation::Web {
                    url: "https://x.example".to_owned(),
                    title: "X".to_owned(),
                    snippet: String::new(),
                    span: Some((500, 600)),
                },
                RawCitation::File {
                    provider_file_id: "file-abc".to_owned(),
                    filename: "doc.pdf".to_owned(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn citations_fall_back_to_the_completed_output() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![sse(
                "response.completed",
                json!({"type": "response.completed", "response": {"id": "resp_1", "output": [
                    {"type": "web_search_call"},
                    {"type": "message", "content": [{
                        "type": "output_text",
                        "text": "h\u{e9}llo w\u{f6}rld",
                        "annotations": [
                            {"type": "url_citation", "url": "https://a.example", "title": "A", "start_index": 6, "end_index": 11},
                            {"type": "url_citation", "url": "https://b.example", "title": "B", "start_index": 0, "end_index": 5, "text": "given"},
                            {"type": "file_citation", "file_id": "file-z", "filename": "z.txt"},
                        ],
                    }]},
                ]}}),
            )]),
        );

        let events = collect(&adapter).await;
        let Some(ProviderEvent::Completed { citations, .. }) = events.last() else {
            panic!("expected completion, got {events:?}");
        };
        assert_eq!(citations.len(), 3);
        assert_eq!(
            citations[0],
            RawCitation::Web {
                url: "https://a.example".to_owned(),
                title: "A".to_owned(),
                snippet: "w\u{f6}rld".to_owned(),
                span: Some((6, 11)),
            }
        );
        assert!(matches!(&citations[1], RawCitation::Web { snippet, .. } if snippet == "given"));
        assert!(matches!(&citations[2], RawCitation::File { filename, .. } if filename == "z.txt"));
    }

    #[tokio::test]
    async fn code_interpreter_output_truncated() {
        let (gateway, adapter) = setup();
        let item = |logs: String| {
            sse(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "item": {
                    "type": "code_interpreter_call",
                    "outputs": [{"type": "logs", "logs": logs}, {"type": "image", "url": "x"}, {"type": "logs", "logs": "tail"}],
                }}),
            )
        };
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![
                item("\u{fc}".repeat(9000)),
                item("short".to_owned()),
                completed(&json!({})),
            ]),
        );

        let events = collect(&adapter).await;
        let ProviderEvent::ToolDone { name, details } = &events[0] else {
            panic!("expected tool done, got {:?}", events[0]);
        };
        assert_eq!(name, "code_interpreter");
        let output = details["output"].as_str().unwrap();
        assert!(output.ends_with("...[truncated]"));
        assert_eq!(
            output.chars().count(),
            8192 + "...[truncated]".chars().count()
        );
        assert!(output.starts_with("\u{fc}\u{fc}\u{fc}"));

        let ProviderEvent::ToolDone { details, .. } = &events[1] else {
            panic!("expected tool done, got {:?}", events[1]);
        };
        assert_eq!(details["output"], "short\ntail");
    }

    #[tokio::test]
    async fn failures_map_to_kinds() {
        let (gateway, adapter) = setup();

        // response.failed: sanitized message, usage kept.
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![sse(
                "response.failed",
                json!({"type": "response.failed", "response": {
                    "error": {"code": "server_error", "message": "bad file-abcdefghijklmnop"},
                    "usage": {"input_tokens": 7, "output_tokens": 2},
                }}),
            )]),
        );
        let events = collect(&adapter).await;
        let [ProviderEvent::Failed(err)] = events.as_slice() else {
            panic!("expected one failure, got {events:?}");
        };
        assert_eq!(err.kind, ProviderErrorKind::Provider);
        assert!(err.message.contains("[provider_id]"), "{}", err.message);
        assert!(!err.message.contains("file-abcdefghijklmnop"));
        assert_eq!(
            err.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((7, 2))
        );
        assert_eq!(err.sse_code(), "provider_error");

        // top-level error fallback
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![sse(
                "response.failed",
                json!({"type": "response.failed", "error": {"message": "top level"}}),
            )]),
        );
        let events = collect(&adapter).await;
        assert!(matches!(&events[..], [ProviderEvent::Failed(e)] if e.message == "top level"));

        // 429 with Retry-After
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json_with_headers(
                429,
                &[("Retry-After", "7")],
                json!({"error": {"message": "Rate limit reached"}}),
            ),
        );
        let err = stream_error(&adapter).await;
        assert_eq!(
            err.kind,
            ProviderErrorKind::RateLimited {
                retry_after_secs: Some(7)
            }
        );
        assert_eq!(err.sse_code(), "rate_limited");
        assert!(
            err.client_message().contains("retry in 7s"),
            "{}",
            err.client_message()
        );

        // transport timeout
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Err(ProxyError::deadline_exceeded("slow").create()),
        );
        let err = stream_error(&adapter).await;
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert_eq!(err.sse_code(), "provider_timeout");

        // transport rate limit
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Err(
                ProxyError::resource_exhausted("rl")
                    .with_quota_violation("rate", "too many")
                    .create(),
            ),
        );
        let err = stream_error(&adapter).await;
        assert_eq!(
            err.kind,
            ProviderErrorKind::RateLimited {
                retry_after_secs: None
            }
        );

        // other transport failure
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Err(CanonicalError::internal("boom").create()),
        );
        assert_eq!(
            stream_error(&adapter).await.kind,
            ProviderErrorKind::Provider
        );

        // gateway 504 is a timeout, other gateway statuses are provider errors
        gateway.clear_rules();
        gateway.on(Method::POST, PATH, Responder::GatewayStatus(504));
        assert_eq!(
            stream_error(&adapter).await.kind,
            ProviderErrorKind::Timeout
        );
        gateway.clear_rules();
        gateway.on(Method::POST, PATH, Responder::GatewayStatus(502));
        assert_eq!(
            stream_error(&adapter).await.kind,
            ProviderErrorKind::Provider
        );

        // an upstream 504 is a provider error
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(504, json!({"error": {"message": "upstream timeout"}})),
        );
        let err = stream_error(&adapter).await;
        assert_eq!(err.kind, ProviderErrorKind::Provider);
        assert_eq!(err.message, "upstream timeout");

        // context length
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(
                400,
                json!({"error": {"code": "context_length_exceeded", "message": "too long"}}),
            ),
        );
        assert_eq!(
            stream_error(&adapter).await.kind,
            ProviderErrorKind::ContextLengthExceeded
        );

        // stream ends without a terminal event
        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![sse(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "x"}),
            )]),
        );
        let events = collect(&adapter).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[1],
            ProviderEvent::Failed(e) if e.kind == ProviderErrorKind::Provider
                && e.message == "provider stream ended without a terminal event"
        ));
    }

    #[tokio::test]
    async fn error_events_are_parsed_in_every_shape() {
        let (gateway, adapter) = setup();
        let cases = [
            (
                json!({"type": "error", "error": {"message": "nested"}}).to_string(),
                "nested",
            ),
            (
                json!({"type": "error", "code": "x", "message": "flat resp_abc123"}).to_string(),
                "flat [provider_id]",
            ),
            ("not json at all".to_owned(), "not json at all"),
        ];
        for (data, expected) in cases {
            gateway.clear_rules();
            gateway.on(
                Method::POST,
                PATH,
                Responder::Sse(vec![SseScript::Raw(format!(
                    "event: error\ndata: {data}\n\n"
                ))]),
            );
            let events = collect(&adapter).await;
            assert!(
                matches!(&events[..], [ProviderEvent::Failed(e)] if e.message == expected),
                "{data}: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn incomplete_carries_reason_and_usage() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::Sse(vec![sse(
                "response.incomplete",
                json!({"type": "response.incomplete", "response": {
                    "id": "resp_3",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"input_tokens": 4, "output_tokens": 9},
                }}),
            )]),
        );
        let events = collect(&adapter).await;
        assert_eq!(
            events,
            vec![ProviderEvent::Incomplete {
                response_id: Some("resp_3".to_owned()),
                usage: Some(ProviderUsage {
                    input_tokens: 4,
                    output_tokens: 9,
                    ..ProviderUsage::default()
                }),
                reason: "max_output_tokens".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn complete_parses_text_and_usage() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(
                200,
                json!({
                    "id": "resp_1",
                    "output": [
                        {"type": "reasoning", "summary": []},
                        {"type": "message", "content": [
                            {"type": "output_text", "text": "Hello "},
                            {"type": "refusal", "refusal": "no"},
                            {"type": "output_text", "text": "world"},
                        ]},
                    ],
                    "usage": {"input_tokens": 10, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 2}},
                }),
            ),
        );
        let result = adapter
            .complete(&target(), provider_request())
            .await
            .expect("complete");
        assert_eq!(result.text, "Hello world");
        assert_eq!(
            result.usage,
            Some(ProviderUsage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_input_tokens: 2,
                ..ProviderUsage::default()
            })
        );
        let body = gateway.requests()[0].json.clone().expect("json");
        assert_eq!(body["stream"], false);

        gateway.clear_rules();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(
                500,
                json!({"error": {"message": "oops file-abcdefghijklmnop"}}),
            ),
        );
        let err = adapter
            .complete(&target(), provider_request())
            .await
            .expect_err("fails");
        assert_eq!(err.kind, ProviderErrorKind::Provider);
        assert_eq!(err.message, "oops [provider_id]");
    }

    #[tokio::test]
    async fn calls_fail_without_an_s2s_context() {
        let gateway = Arc::new(FakeGateway::new());
        let adapter = OpenAiResponsesAdapter::new(Arc::clone(&gateway) as _, S2sContext::new());
        let err = stream_error(&adapter).await;
        assert_eq!(err.kind, ProviderErrorKind::Provider);
        assert!(gateway.requests().is_empty());
    }
}
