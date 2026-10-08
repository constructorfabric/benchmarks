//! Chat request dispatch by provider kind: builds the request body, proxies it
//! through OAGW and translates the provider SSE stream into [`LlmEvent`]s
//! without buffering (one provider event in, its translated events out).

#![allow(clippy::result_large_err)]

use std::collections::VecDeque;
use std::pin::Pin;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use oagw_sdk::body::Body;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};

use super::anthropic::{self, AnthropicTranslator};
use super::gateway::{ProxyClient, buffer, classify_http_failure};
use super::openai_chat::{self, ChatTranslator};
use super::openai_responses::{self, BuildOptions, ResponsesTranslator};
use super::provider::ChatTarget;
use super::types::{CompleteResponse, LlmEvent, LlmRequest, ProviderFailure};
use crate::config::ProviderKind;

/// Stream of translated provider events. Dropping it closes the upstream
/// connection (hard cancel).
pub type LlmEventStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

enum Translator {
    Responses(ResponsesTranslator),
    Chat(ChatTranslator),
    Anthropic(AnthropicTranslator),
}

impl Translator {
    fn for_kind(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenaiResponses => Self::Responses(ResponsesTranslator::new(false)),
            ProviderKind::VllmResponses => Self::Responses(ResponsesTranslator::new(true)),
            ProviderKind::OpenaiChatCompletions => Self::Chat(ChatTranslator::new()),
            ProviderKind::AnthropicMessages => Self::Anthropic(AnthropicTranslator::new()),
        }
    }

    fn on_event(&mut self, ev: &ServerEvent) -> Vec<LlmEvent> {
        let name = ev.event.as_deref();
        match self {
            Self::Responses(t) => t.on_event(name, &ev.data),
            Self::Chat(t) => t.on_event(name, &ev.data),
            Self::Anthropic(t) => t.on_event(name, &ev.data),
        }
    }

    fn finish(&mut self) -> Option<LlmEvent> {
        match self {
            Self::Responses(t) => t.finish(),
            Self::Chat(t) => t.finish(),
            Self::Anthropic(t) => t.finish(),
        }
    }
}

/// Build the request body and extra headers of a provider kind.
#[must_use]
pub fn build_body(
    kind: ProviderKind,
    req: &LlmRequest,
) -> (serde_json::Value, Vec<(&'static str, &'static str)>) {
    match kind {
        ProviderKind::OpenaiResponses => (
            openai_responses::build_request(req, BuildOptions::OPENAI),
            Vec::new(),
        ),
        ProviderKind::VllmResponses => (
            openai_responses::build_request(req, BuildOptions::VLLM),
            Vec::new(),
        ),
        ProviderKind::OpenaiChatCompletions => (openai_chat::build_request(req), Vec::new()),
        ProviderKind::AnthropicMessages => {
            let mut headers = vec![("anthropic-version", anthropic::ANTHROPIC_VERSION)];
            if anthropic::uses_files(req) {
                headers.push(("anthropic-beta", anthropic::ANTHROPIC_FILES_BETA));
            }
            (anthropic::build_request(req), headers)
        }
    }
}

/// LLM chat client.
#[derive(Clone)]
pub struct LlmClient {
    proxy: ProxyClient,
}

impl LlmClient {
    #[must_use]
    pub fn new(proxy: ProxyClient) -> Self {
        Self { proxy }
    }

    #[must_use]
    pub fn proxy(&self) -> &ProxyClient {
        &self.proxy
    }

    fn request(
        target: &ChatTarget,
        model: &str,
        body: &serde_json::Value,
        headers: &[(&str, &str)],
        stream: bool,
    ) -> Result<http::Request<Body>, ProviderFailure> {
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(target.uri(model))
            .header(http::header::CONTENT_TYPE, "application/json");
        if stream {
            builder = builder.header(http::header::ACCEPT, "text/event-stream");
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder
            .body(Body::Bytes(Bytes::from(body.to_string())))
            .map_err(|e| ProviderFailure::provider(format!("invalid request: {e}")))
    }

    /// Start a streaming request.
    ///
    /// # Errors
    /// Gateway failure or a non-success provider status (classified).
    pub async fn stream(
        &self,
        target: &ChatTarget,
        req: &LlmRequest,
    ) -> Result<LlmEventStream, ProviderFailure> {
        let (body, headers) = build_body(target.kind, req);
        let request = Self::request(target, &req.model, &body, &headers, true)?;
        let response = self.proxy.send(request).await?;
        if !response.status().is_success() {
            let buffered = buffer(response).await?;
            return Err(classify_http_failure(&buffered));
        }
        let kind = target.kind;
        match ServerEventsStream::from_response::<ServerEvent>(response) {
            ServerEventsResponse::Events(events) => {
                Ok(translate(events, Translator::for_kind(kind)))
            }
            ServerEventsResponse::Response(resp) => {
                // Not an event stream: treat a JSON body as a complete result.
                let buffered = buffer(resp).await?;
                let json = buffered.json().map_err(ProviderFailure::provider)?;
                let (text, usage) = parse_complete(kind, &json);
                let mut out = Vec::new();
                if !text.is_empty() {
                    out.push(LlmEvent::TextDelta(text));
                }
                out.push(LlmEvent::Completed(super::types::Completion {
                    usage,
                    response_id: json.get("id").and_then(|v| v.as_str()).map(str::to_owned),
                    incomplete_reason: None,
                }));
                Ok(Box::pin(futures::stream::iter(out)))
            }
        }
    }

    /// Non-streaming completion (thread summary).
    ///
    /// # Errors
    /// Gateway failure, a non-success provider status or an unparsable body.
    pub async fn complete(
        &self,
        target: &ChatTarget,
        req: &LlmRequest,
    ) -> Result<CompleteResponse, ProviderFailure> {
        let mut req = req.clone();
        req.stream = false;
        let (body, headers) = build_body(target.kind, &req);
        let request = Self::request(target, &req.model, &body, &headers, false)?;
        let response = self.proxy.send(request).await?;
        if !response.status().is_success() {
            let buffered = buffer(response).await?;
            return Err(classify_http_failure(&buffered));
        }
        if oagw_sdk::sse::is_server_events_response(response.headers()) {
            // A provider that streams anyway: collect the text.
            let ServerEventsResponse::Events(events) =
                ServerEventsStream::from_response::<ServerEvent>(response)
            else {
                return Err(ProviderFailure::provider("unexpected response"));
            };
            let mut stream = translate(events, Translator::for_kind(target.kind));
            let mut text = String::new();
            let mut usage = super::types::Usage::default();
            while let Some(ev) = stream.next().await {
                match ev {
                    LlmEvent::TextDelta(t) => text.push_str(&t),
                    LlmEvent::Completed(c) => usage = c.usage.unwrap_or_default(),
                    LlmEvent::Failed(f) => return Err(f),
                    _ => {}
                }
            }
            return Ok(CompleteResponse { text, usage });
        }
        let buffered = buffer(response).await?;
        let json = buffered.json().map_err(ProviderFailure::provider)?;
        if let Some(status) = json.get("status").and_then(|s| s.as_str())
            && status == "failed"
        {
            let (code, message) = openai_responses::parse_stream_error(&json, "provider error");
            let mut f = ProviderFailure::provider(message);
            f.provider_code = code;
            return Err(f);
        }
        let (text, usage) = parse_complete(target.kind, &json);
        Ok(CompleteResponse {
            text,
            usage: usage.unwrap_or_default(),
        })
    }
}

fn parse_complete(
    kind: ProviderKind,
    json: &serde_json::Value,
) -> (String, Option<super::types::Usage>) {
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            openai_responses::parse_complete(json)
        }
        ProviderKind::OpenaiChatCompletions => openai_chat::parse_complete(json),
        ProviderKind::AnthropicMessages => anthropic::parse_complete(json),
    }
}

struct TranslateState {
    events: ServerEventsStream<ServerEvent>,
    translator: Translator,
    queue: VecDeque<LlmEvent>,
    done: bool,
}

fn translate(events: ServerEventsStream<ServerEvent>, translator: Translator) -> LlmEventStream {
    let state = TranslateState {
        events,
        translator,
        queue: VecDeque::new(),
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.queue.pop_front() {
                return Some((ev, st));
            }
            if st.done {
                return None;
            }
            match st.events.next().await {
                Some(Ok(ev)) => {
                    st.queue.extend(st.translator.on_event(&ev));
                }
                Some(Err(e)) => {
                    st.done = true;
                    let msg = e.to_string();
                    let failure = if msg.to_ascii_lowercase().contains("timed out")
                        || msg.to_ascii_lowercase().contains("timeout")
                    {
                        ProviderFailure::timeout(format!("provider stream timed out: {msg}"))
                    } else {
                        ProviderFailure::provider(format!("provider stream error: {msg}"))
                    };
                    // Only report the failure if no terminal event was seen.
                    if st.translator.finish().is_some() {
                        st.queue.push_back(LlmEvent::Failed(failure));
                    }
                }
                None => {
                    st.done = true;
                    if let Some(ev) = st.translator.finish() {
                        st.queue.push_back(ev);
                    }
                }
            }
        }
    }))
}
