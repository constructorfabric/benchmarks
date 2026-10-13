//! The non-streaming summary call
//! (`cpt-cf-mini-chat-algo-background-workers-call-summary-model`): the request
//! builders and the response readers of the four provider kinds, and the client
//! that sends the request through the OAGW in-process proxy. No credential
//! header is ever set (OAGW injects it).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use mini_chat_sdk::UsageTokens;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::{Map, Value, json};
use toolkit_security::SecurityContext;

use crate::config::ProviderKind;
use crate::domain::service::stream::ports::BuiltRequest;
use crate::domain::service::workers::ports::{
    SummaryAnswer, SummaryCallError, SummaryModelClient, SummaryRequest,
};
use crate::domain::service::workers::{SUMMARY_RESPONSE_MAX_BYTES, SYSTEM_SUBJECT};

/// `anthropic-version` of the Messages API.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How much of an error body is read to classify a context-length error.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// The `user` value: the tenant and the system subject as simple UUIDs.
fn user_value(request: &SummaryRequest) -> String {
    format!("{}{}", request.tenant_id.simple(), SYSTEM_SUBJECT.simple())
}

/// Builds the non-streaming request in the wire format of the provider kind:
/// the provider model id, the system prompt as instructions, one user input with
/// the user prompt, the maximum output tokens set to the catalog
/// `max_output_tokens`, no tools.
// @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-build-summary-request
#[must_use]
pub fn build_summary_request(request: &SummaryRequest) -> BuiltRequest {
    let entry = &request.entry;
    let kind = request.target.kind;
    let uri = format!(
        "/{}{}",
        request.target.alias,
        request
            .target
            .api_path
            .replace("{model}", &entry.provider_model_id)
    );
    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
    ];
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(entry.provider_model_id));
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            body.insert("instructions".to_owned(), json!(request.system));
            body.insert(
                "input".to_owned(),
                json!([{ "role": "user", "content": [ { "type": "input_text", "text": request.user } ] }]),
            );
            body.insert(
                "max_output_tokens".to_owned(),
                json!(entry.max_output_tokens),
            );
            body.insert("stream".to_owned(), json!(false));
            body.insert("store".to_owned(), json!(false));
            body.insert("user".to_owned(), json!(user_value(request)));
            if kind == ProviderKind::OpenaiResponses {
                body.insert(
                    "metadata".to_owned(),
                    json!({
                        "tenant_id": request.tenant_id.to_string(),
                        "user_id": SYSTEM_SUBJECT.to_string(),
                        "chat_id": request.chat_id.to_string(),
                        "request_type": "summary",
                        "feature": "none",
                    }),
                );
            }
        }
        ProviderKind::OpenaiChatCompletions => {
            body.insert(
                "messages".to_owned(),
                json!([
                    { "role": "system", "content": request.system },
                    { "role": "user", "content": request.user },
                ]),
            );
            body.insert(
                "max_completion_tokens".to_owned(),
                json!(entry.max_output_tokens),
            );
            body.insert("stream".to_owned(), json!(false));
            body.insert("user".to_owned(), json!(user_value(request)));
        }
        ProviderKind::AnthropicMessages => {
            body.insert("system".to_owned(), json!(request.system));
            body.insert(
                "messages".to_owned(),
                json!([{ "role": "user", "content": [ { "type": "text", "text": request.user } ] }]),
            );
            body.insert("max_tokens".to_owned(), json!(entry.max_output_tokens));
            body.insert("stream".to_owned(), json!(false));
            body.insert(
                "metadata".to_owned(),
                json!({ "user_id": user_value(request) }),
            );
            headers.push(("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()));
        }
    }
    BuiltRequest {
        kind,
        uri,
        headers,
        body: Value::Object(body),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-build-summary-request

fn count(value: &Value, path: &[&str]) -> u64 {
    path.iter()
        .try_fold(value, |value, key| value.get(*key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Reads the output text and the usage of a non-streaming answer in the
/// format of `kind`; a missing usage field is 0. `None` when the body has no
/// output of its format.
// @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-extract-summary
#[must_use]
pub fn read_summary_answer(kind: ProviderKind, body: &Value) -> Option<SummaryAnswer> {
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            let output = body.get("output")?.as_array()?;
            let text: String = output
                .iter()
                .filter(|item| item["type"] == "message")
                .filter_map(|item| item["content"].as_array())
                .flatten()
                .filter(|part| part["type"] == "output_text")
                .filter_map(|part| part["text"].as_str())
                .collect();
            Some(SummaryAnswer {
                text,
                usage: UsageTokens {
                    input_tokens: count(body, &["usage", "input_tokens"]),
                    output_tokens: count(body, &["usage", "output_tokens"]),
                    cache_read_input_tokens: count(
                        body,
                        &["usage", "input_tokens_details", "cached_tokens"],
                    ),
                    cache_write_input_tokens: 0,
                    reasoning_tokens: count(
                        body,
                        &["usage", "output_tokens_details", "reasoning_tokens"],
                    ),
                },
            })
        }
        ProviderKind::OpenaiChatCompletions => {
            let choice = body.get("choices")?.as_array()?.first()?;
            let text = choice["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Some(SummaryAnswer {
                text,
                usage: UsageTokens {
                    input_tokens: count(body, &["usage", "prompt_tokens"]),
                    output_tokens: count(body, &["usage", "completion_tokens"]),
                    cache_read_input_tokens: count(
                        body,
                        &["usage", "prompt_tokens_details", "cached_tokens"],
                    ),
                    cache_write_input_tokens: 0,
                    reasoning_tokens: count(
                        body,
                        &["usage", "completion_tokens_details", "reasoning_tokens"],
                    ),
                },
            })
        }
        ProviderKind::AnthropicMessages => {
            let content = body.get("content")?.as_array()?;
            let text: String = content
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect();
            Some(SummaryAnswer {
                text,
                usage: UsageTokens {
                    input_tokens: count(body, &["usage", "input_tokens"]),
                    output_tokens: count(body, &["usage", "output_tokens"]),
                    cache_read_input_tokens: count(body, &["usage", "cache_read_input_tokens"]),
                    cache_write_input_tokens: count(
                        body,
                        &["usage", "cache_creation_input_tokens"],
                    ),
                    reasoning_tokens: 0,
                },
            })
        }
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-extract-summary

/// Whether an error response says that the context is too long (classified like
/// the streaming path classifies a provider error body): a 4xx status whose
/// error code is `context_length_exceeded` or whose message names the context
/// length.
#[must_use]
pub fn is_context_length_error(status: u16, body: &[u8]) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let text = |value: &Value| value.as_str().map(str::to_ascii_lowercase);
    let code = text(&parsed["error"]["code"]).or_else(|| text(&parsed["code"]));
    if code.as_deref() == Some("context_length_exceeded") {
        return true;
    }
    let message = text(&parsed["error"]["message"])
        .or_else(|| text(&parsed["message"]))
        .unwrap_or_default();
    [
        "context length",
        "context_length",
        "maximum context",
        "context window",
        "prompt is too long",
        "too many tokens",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Reads at most `limit` bytes of `body`; `None` when the body is longer or a
/// chunk fails.
async fn read_limited(body: Body, limit: usize) -> Option<Vec<u8>> {
    let mut stream = body.into_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if bytes.len() + chunk.len() > limit {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

/// The summary client over the OAGW in-process proxy.
pub struct OagwSummaryClient {
    oagw: Arc<dyn ServiceGatewayClientV1>,
}

impl OagwSummaryClient {
    /// Creates the client.
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { oagw }
    }
}

#[async_trait]
impl SummaryModelClient for OagwSummaryClient {
    async fn summarize(
        &self,
        ctx: &SecurityContext,
        request: &SummaryRequest,
    ) -> Result<SummaryAnswer, SummaryCallError> {
        let built = build_summary_request(request);
        let payload = serde_json::to_vec(&built.body)
            .map_err(|_| SummaryCallError::Failed("request encoding".to_owned()))?;
        let mut builder = http::Request::builder().method("POST").uri(&built.uri);
        for (name, value) in &built.headers {
            builder = builder.header(name, value);
        }
        let http_request = builder
            .body(Body::from(payload))
            .map_err(|_| SummaryCallError::Failed("request encoding".to_owned()))?;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-send-summary-request
        let response = self
            .oagw
            .proxy_request(ctx.clone(), http_request)
            .await
            .map_err(|_| SummaryCallError::Failed("gateway error".to_owned()))?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-send-summary-request
        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-ptl-retry
        if !parts.status.is_success() {
            let error_body = read_limited(body, ERROR_BODY_LIMIT)
                .await
                .unwrap_or_default();
            return Err(if is_context_length_error(status, &error_body) {
                SummaryCallError::ContextLength
            } else {
                SummaryCallError::Failed(format!("http {status}"))
            });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-ptl-retry
        let bytes = read_limited(body, SUMMARY_RESPONSE_MAX_BYTES)
            .await
            .ok_or_else(|| {
                SummaryCallError::Failed("response too large or unreadable".to_owned())
            })?;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|_| SummaryCallError::Failed("unreadable response".to_owned()))?;
        read_summary_answer(built.kind, &json)
            .ok_or_else(|| SummaryCallError::Failed("unreadable response".to_owned()))
    }
}

#[cfg(test)]
#[path = "summary_client_tests.rs"]
mod tests;
