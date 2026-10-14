//! The mock OAGW of the message-streaming slice S2-provider-path: a `ServiceGatewayClientV1`
//! that records every proxied request (URI, method, headers, S2S security context, JSON body)
//! and answers per script with an OpenAI-Responses-format SSE stream, an HTTP error, a gateway
//! error, a hang or a slow delivery; plus the fixtures of the Responses wire events.
//!
//! No network is used. A response stream that is dropped before its script ended counts as an
//! aborted (hard-cancelled) provider request.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

#[toolkit_canonical_errors::resource_error(toolkit_gts::gts_id!("cf.core.oagw.proxy.v1~"))]
struct OagwProxyScope;

/// The provider response id of the fixtures: it must never reach a client.
pub const RESPONSE_ID: &str = "resp_0aB1c2D3e4F5g6H7";

// ── gates ────────────────────────────────────────────────────────────────────

/// A level-triggered gate a script waits on until the test opens it.
#[derive(Default)]
pub struct Gate {
    open: AtomicBool,
    notify: Notify,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Let every waiting and future waiter pass.
    pub fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

// ── scripts ──────────────────────────────────────────────────────────────────

/// One step of a scripted SSE response body.
#[derive(Clone)]
pub enum Step {
    /// One SSE event (`event:` line when named, one `data:` line), delivered as one body chunk.
    Event { name: Option<String>, data: String },
    /// Raw body bytes (to split or join events across chunks).
    Raw(String),
    /// Wait until the gate is opened.
    Wait(Arc<Gate>),
    /// Wait a while.
    Delay(Duration),
    /// Never deliver another byte and never end.
    Hang,
    /// The body stream fails.
    Break,
}

impl Step {
    /// A named event whose `data` is the JSON value.
    pub fn named(name: &str, data: &Value) -> Self {
        Self::Event {
            name: Some(name.to_owned()),
            data: data.to_string(),
        }
    }

    /// An event without an `event:` line.
    pub fn unnamed(data: &Value) -> Self {
        Self::Event {
            name: None,
            data: data.to_string(),
        }
    }
}

/// A failure the gateway itself reports (`Err(CanonicalError)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayFailure {
    /// `deadline_exceeded`: a gateway timeout.
    Timeout,
    /// `service_unavailable`.
    Unavailable,
    /// `internal`.
    Internal,
}

impl GatewayFailure {
    fn error(self) -> CanonicalError {
        match self {
            Self::Timeout => {
                OagwProxyScope::deadline_exceeded("upstream did not respond in time").create()
            }
            Self::Unavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("the upstream is unavailable")
                .create(),
            Self::Internal => CanonicalError::internal("oagw exploded").create(),
        }
    }
}

/// How the mock answers one `proxy_request`.
#[derive(Clone)]
pub enum Reply {
    /// HTTP 200 `text/event-stream` with a streamed body.
    Sse(Vec<Step>),
    /// A buffered response with `status`, headers and `body` (JSON content type unless a
    /// `content-type` header is given); `from_gateway` sets `ErrorSource::Gateway`, else
    /// `ErrorSource::Upstream`.
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        from_gateway: bool,
    },
    /// `Err(CanonicalError)` from `proxy_request`.
    Fail(GatewayFailure),
    /// `proxy_request` never answers (the provider does not respond); dropping the call counts
    /// as an aborted request.
    Pending,
}

impl Reply {
    /// An upstream HTTP error with a JSON body.
    pub fn http(status: u16, body: &Value) -> Self {
        Self::Http {
            status,
            headers: Vec::new(),
            body: body.to_string(),
            from_gateway: false,
        }
    }

    /// An upstream HTTP error with a header (for example `retry-after`).
    pub fn http_with(status: u16, headers: &[(&str, &str)], body: &Value) -> Self {
        Self::Http {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.to_string(),
            from_gateway: false,
        }
    }

    /// The gateway's own Problem with `status` (for example 504 `deadline_exceeded`).
    pub fn gateway_problem(status: u16, body: &Value) -> Self {
        Self::Http {
            status,
            headers: vec![(
                "content-type".to_owned(),
                "application/problem+json".to_owned(),
            )],
            body: body.to_string(),
            from_gateway: true,
        }
    }
}

// ── the mock gateway ─────────────────────────────────────────────────────────

/// One recorded `proxy_request`.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The subject of the security context the call was made under.
    pub subject: Uuid,
    pub tenant: Uuid,
    pub method: String,
    /// The request URI as the gear built it (`/{alias}{api_path}` with its query).
    pub uri: String,
    pub headers: Vec<(String, String)>,
    /// The JSON body.
    pub body: Value,
}

impl RecordedRequest {
    /// The value of a header (names are lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct Stats {
    delivered: AtomicUsize,
    finished: AtomicUsize,
    aborted: AtomicUsize,
    changed: Notify,
}

/// Counts a response stream as finished (its script was fully delivered) or aborted (dropped
/// with steps left) when it is dropped.
struct Probe {
    stats: Arc<Stats>,
    complete: bool,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.complete {
            self.stats.finished.fetch_add(1, Ordering::SeqCst);
        } else {
            self.stats.aborted.fetch_add(1, Ordering::SeqCst);
        }
        self.stats.changed.notify_waiters();
    }
}

struct Script {
    steps: VecDeque<Step>,
    probe: Probe,
    hung: bool,
}

type ChunkResult = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>;

fn chunk_of(step: &Step) -> Option<Bytes> {
    match step {
        Step::Event { name, data } => {
            let mut text = String::new();
            if let Some(name) = name {
                text.push_str("event: ");
                text.push_str(name);
                text.push('\n');
            }
            for line in data.split('\n') {
                text.push_str("data: ");
                text.push_str(line);
                text.push('\n');
            }
            text.push('\n');
            Some(Bytes::from(text))
        }
        Step::Raw(raw) => Some(Bytes::from(raw.clone())),
        Step::Wait(_) | Step::Delay(_) | Step::Hang | Step::Break => None,
    }
}

/// The streamed body of a script.
fn script_body(steps: Vec<Step>, stats: Arc<Stats>) -> Body {
    let complete = steps.is_empty();
    let script = Script {
        steps: steps.into(),
        probe: Probe { stats, complete },
        hung: false,
    };
    let stream = futures::stream::unfold(script, |mut script| async move {
        loop {
            if script.hung {
                std::future::pending::<()>().await;
            }
            let step = script.steps.pop_front()?;
            match &step {
                Step::Wait(gate) => {
                    gate.wait().await;
                    script.probe.complete = script.steps.is_empty();
                }
                Step::Delay(duration) => {
                    tokio::time::sleep(*duration).await;
                    script.probe.complete = script.steps.is_empty();
                }
                Step::Hang => {
                    script.probe.complete = false;
                    script.hung = true;
                }
                Step::Break => {
                    script.probe.complete = script.steps.is_empty();
                    let error: Box<dyn std::error::Error + Send + Sync> =
                        "the provider connection broke".into();
                    return Some((ChunkResult::Err(error), script));
                }
                Step::Event { .. } | Step::Raw(_) => {
                    script.probe.complete = script.steps.is_empty();
                    if let Some(chunk) = chunk_of(&step) {
                        script.probe.stats.delivered.fetch_add(1, Ordering::SeqCst);
                        script.probe.stats.changed.notify_waiters();
                        return Some((Ok(chunk), script));
                    }
                }
            }
        }
    });
    Body::Stream(Box::pin(stream))
}

/// The recording and scripted `ServiceGatewayClientV1`.
pub struct MockProvider {
    queued: Mutex<VecDeque<Reply>>,
    default: Mutex<Reply>,
    calls: Mutex<Vec<RecordedRequest>>,
    stats: Arc<Stats>,
    arrived: Notify,
}

impl MockProvider {
    /// A mock whose default answer is a completed stream with the text `hello`.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::new(VecDeque::new()),
            default: Mutex::new(Reply::Sse(happy("hello", (120, 30)))),
            calls: Mutex::new(Vec::new()),
            stats: Arc::new(Stats::default()),
            arrived: Notify::new(),
        })
    }

    /// A mock that answers every call with `reply`.
    pub fn answering(reply: Reply) -> Arc<Self> {
        let mock = Self::new();
        mock.set_default(reply);
        mock
    }

    /// Answer the next unanswered call with `reply` (queued answers are used in order, then the
    /// default).
    pub fn queue(&self, reply: Reply) {
        self.queued.lock().unwrap().push_back(reply);
    }

    /// The answer when nothing is queued.
    pub fn set_default(&self, reply: Reply) {
        *self.default.lock().unwrap() = reply;
    }

    pub fn calls(&self) -> Vec<RecordedRequest> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The only recorded request.
    pub fn only_call(&self) -> RecordedRequest {
        let calls = self.calls();
        assert_eq!(calls.len(), 1, "expected exactly one proxied request");
        calls.into_iter().next().unwrap()
    }

    /// Response streams dropped before their script ended (hard-cancelled provider requests).
    pub fn aborted_streams(&self) -> usize {
        self.stats.aborted.load(Ordering::SeqCst)
    }

    /// Response streams dropped after their script was fully delivered.
    pub fn finished_streams(&self) -> usize {
        self.stats.finished.load(Ordering::SeqCst)
    }

    /// The events (body chunks) the consumer pulled so far.
    pub fn delivered_events(&self) -> usize {
        self.stats.delivered.load(Ordering::SeqCst)
    }

    /// Wait until `count` requests arrived (5 s at most).
    pub async fn wait_for_calls(&self, count: usize) {
        let wait = async {
            loop {
                let notified = self.arrived.notified();
                if self.call_count() >= count {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the provider request never arrived");
    }

    /// Wait until `count` response streams were dropped before their script ended (5 s at
    /// most).
    pub async fn wait_for_aborted(&self, count: usize) {
        let wait = async {
            loop {
                let changed = self.stats.changed.notified();
                if self.aborted_streams() >= count {
                    return;
                }
                changed.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the provider response stream was never dropped");
    }

    fn next_reply(&self) -> Reply {
        self.queued
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.default.lock().unwrap().clone())
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for MockProvider {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        _req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        _req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        unimplemented!("the provider mock only proxies")
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.expect("request body");
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        self.calls.lock().unwrap().push(RecordedRequest {
            subject: ctx.subject_id(),
            tenant: ctx.subject_tenant_id(),
            method: parts.method.to_string(),
            uri: parts.uri.to_string(),
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        });
        self.arrived.notify_waiters();
        match self.next_reply() {
            Reply::Sse(steps) => Ok(http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(script_body(steps, Arc::clone(&self.stats)))
                .expect("response")),
            Reply::Http {
                status,
                headers,
                body,
                from_gateway,
            } => {
                let mut builder = http::Response::builder().status(status);
                if !headers.iter().any(|(name, _)| name == "content-type") {
                    builder = builder.header("content-type", "application/json");
                }
                for (name, value) in &headers {
                    builder = builder.header(name, value);
                }
                let mut response = builder.body(Body::from(body)).expect("response");
                response.extensions_mut().insert(if from_gateway {
                    ErrorSource::Gateway
                } else {
                    ErrorSource::Upstream
                });
                Ok(response)
            }
            Reply::Fail(failure) => Err(failure.error()),
            Reply::Pending => {
                let _probe = Probe {
                    stats: Arc::clone(&self.stats),
                    complete: false,
                };
                std::future::pending::<()>().await;
                unreachable!("pending never completes")
            }
        }
    }
}

// ── OpenAI Responses wire fixtures ───────────────────────────────────────────

fn usage_json(usage: (u64, u64)) -> Value {
    json!({
        "input_tokens": usage.0,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": usage.1,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": usage.0 + usage.1
    })
}

/// `response.output_text.delta`.
pub fn delta(text: &str) -> Step {
    Step::named(
        "response.output_text.delta",
        &json!({
            "type": "response.output_text.delta",
            "item_id": "msg_0aB1c2D3e4F5",
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        }),
    )
}

/// An `output_text` part with annotations.
pub fn output_part(text: &str, annotations: &[Value]) -> Value {
    json!({ "type": "output_text", "text": text, "annotations": annotations })
}

/// A `url_citation` annotation.
pub fn url_annotation(url: &str, title: &str, start: usize, end: usize) -> Value {
    json!({
        "type": "url_citation", "url": url, "title": title,
        "start_index": start, "end_index": end
    })
}

/// `response.completed` with the given output parts and `(input, output)` usage.
pub fn completed_parts(parts: &[Value], usage: (u64, u64)) -> Step {
    Step::named(
        "response.completed",
        &json!({
            "type": "response.completed",
            "response": {
                "id": RESPONSE_ID,
                "object": "response",
                "status": "completed",
                "output": [{ "type": "message", "id": "msg_0aB1c2D3e4F5", "role": "assistant",
                              "content": parts }],
                "usage": usage_json(usage),
            }
        }),
    )
}

/// `response.completed` whose single part holds `text`.
pub fn completed(text: &str, usage: (u64, u64)) -> Step {
    completed_parts(&[output_part(text, &[])], usage)
}

/// `response.incomplete` with the provider reason.
pub fn incomplete(reason: &str, usage: (u64, u64)) -> Step {
    Step::named(
        "response.incomplete",
        &json!({
            "type": "response.incomplete",
            "response": {
                "id": RESPONSE_ID,
                "object": "response",
                "status": "incomplete",
                "incomplete_details": { "reason": reason },
                "output": [],
                "usage": usage_json(usage),
            }
        }),
    )
}

/// `response.failed` with the provider error and optional usage.
pub fn failed(code: &str, message: &str, usage: Option<(u64, u64)>) -> Step {
    let mut response = json!({
        "id": RESPONSE_ID,
        "object": "response",
        "status": "failed",
        "error": { "code": code, "message": message },
    });
    if let Some(usage) = usage {
        response["usage"] = usage_json(usage);
    }
    Step::named(
        "response.failed",
        &json!({ "type": "response.failed", "response": response }),
    )
}

/// A bare `error` event.
pub fn error_event(code: &str, message: &str) -> Step {
    Step::named(
        "error",
        &json!({ "type": "error", "code": code, "message": message }),
    )
}

/// `response.web_search_call.searching`.
pub fn web_start() -> Step {
    Step::named(
        "response.web_search_call.searching",
        &json!({ "type": "response.web_search_call.searching", "item_id": "ws_1", "output_index": 0 }),
    )
}

/// `response.web_search_call.completed`.
pub fn web_done() -> Step {
    Step::named(
        "response.web_search_call.completed",
        &json!({ "type": "response.web_search_call.completed", "item_id": "ws_1", "output_index": 0 }),
    )
}

/// `response.file_search_call.searching`.
pub fn fs_start() -> Step {
    Step::named(
        "response.file_search_call.searching",
        &json!({ "type": "response.file_search_call.searching", "item_id": "fs_1", "output_index": 0 }),
    )
}

/// `response.file_search_call.completed` without results (the real API sends none).
pub fn fs_done() -> Step {
    Step::named(
        "response.file_search_call.completed",
        &json!({ "type": "response.file_search_call.completed", "item_id": "fs_1", "output_index": 0 }),
    )
}

/// `response.output_item.done` of a `code_interpreter_call` item with one logs output.
pub fn ci_done(logs: &str) -> Step {
    Step::named(
        "response.output_item.done",
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "code_interpreter_call", "id": "ci_1",
                       "outputs": [{ "type": "logs", "logs": logs }] }
        }),
    )
}

/// `response.code_interpreter_call.in_progress`.
pub fn ci_start() -> Step {
    Step::named(
        "response.code_interpreter_call.in_progress",
        &json!({ "type": "response.code_interpreter_call.in_progress", "item_id": "ci_1", "output_index": 0 }),
    )
}

/// `response.output_item.done` of a `function_call` item (a tool-use outcome).
pub fn function_call_done(name: &str) -> Step {
    Step::named(
        "response.output_item.done",
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "function_call", "name": name, "call_id": "call_0123456789abcdef",
                       "arguments": "{}" }
        }),
    )
}

/// `response.output_item.done` of a `function_call` item with a call id and JSON arguments.
pub fn function_call_with(name: &str, call_id: &str, arguments: &Value) -> Step {
    Step::named(
        "response.output_item.done",
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "function_call", "name": name, "call_id": call_id,
                       "arguments": arguments.to_string() }
        }),
    )
}

/// A `file_citation` annotation.
pub fn file_annotation(file_id: &str, filename: &str) -> Value {
    json!({ "type": "file_citation", "file_id": file_id, "filename": filename, "index": 0 })
}

/// The script of an iteration that ends with a tool-use outcome: the function call item, then
/// `response.completed` with the usage of the iteration.
pub fn tool_use_steps(
    name: &str,
    call_id: &str,
    arguments: &Value,
    usage: (u64, u64),
) -> Vec<Step> {
    vec![
        function_call_with(name, call_id, arguments),
        Step::named(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": {
                    "id": RESPONSE_ID,
                    "object": "response",
                    "status": "completed",
                    "output": [{ "type": "function_call", "name": name, "call_id": call_id,
                                  "arguments": arguments.to_string() }],
                    "usage": usage_json(usage),
                }
            }),
        ),
    ]
}

/// `d0;d1;...;d{count-1};`: the text of [`numbered_deltas`].
pub fn numbered_text(count: usize) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for n in 0..count {
        write!(text, "d{n};").expect("write to a string");
    }
    text
}

/// One delta per number `d{n};` for `n` in `0..count`.
pub fn numbered_deltas(count: usize) -> Vec<Step> {
    (0..count).map(|n| delta(&format!("d{n};"))).collect()
}

/// The script of a normal answer: one delta per text, then `response.completed` with the whole
/// text and the usage.
pub fn happy_steps(texts: &[&str], usage: (u64, u64)) -> Vec<Step> {
    let mut steps: Vec<Step> = texts.iter().map(|text| delta(text)).collect();
    steps.push(completed(&texts.concat(), usage));
    steps
}

/// A normal answer with one delta.
pub fn happy(text: &str, usage: (u64, u64)) -> Vec<Step> {
    happy_steps(&[text], usage)
}
