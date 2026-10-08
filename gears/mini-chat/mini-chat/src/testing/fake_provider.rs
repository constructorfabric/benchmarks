//! In-process `ServiceGatewayClientV1` emulating OAGW in front of an
//! OpenAI-compatible provider: scripted chat streams (Responses, Chat
//! Completions and Anthropic Messages paths) and completions, the Files and
//! Vector Stores APIs (`/v1/...` and Azure `/openai/...`, also serving the
//! Anthropic Files API), vector-store search, request recording, held
//! requests and injected failures.
//!
//! Requests are routed by the provider path (the URI path without the leading
//! `/{alias}` segment) in [`Endpoint::classify`]; add endpoints there.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use http::{Method, StatusCode, header};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::{BodyStream, BoxError};
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, ServiceGatewayClientV1,
    UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::infra::llm::LlmUsage;

/// Provider response id used by the scripted events.
pub const FAKE_RESPONSE_ID: &str = "resp_fake0001";
/// Provider output item id used by the scripted events.
pub const FAKE_ITEM_ID: &str = "msg_fake0001";

/// A scripted answer to one streaming `POST …/responses`.
#[derive(Debug, Clone)]
pub struct ScriptedStream {
    /// SSE events `(event name, data)`, sent in order.
    pub events: Vec<(String, Value)>,
    /// Pause before each event after the first.
    pub delay_between: Duration,
    /// Pause before event `n` until [`FakeProvider::release`].
    pub hold_after: Option<usize>,
    /// Answer with `(status, JSON body, Retry-After)` instead of a stream.
    pub http_error: Option<(u16, Value, Option<u64>)>,
    /// Origin reported for `http_error` (OAGW `ErrorSource` extension).
    pub error_source: ErrorSource,
}

impl Default for ScriptedStream {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            delay_between: Duration::ZERO,
            hold_after: None,
            http_error: None,
            error_source: ErrorSource::Upstream,
        }
    }
}

fn usage_json(input_tokens: i64, output_tokens: i64) -> Value {
    json!({
        "input_tokens": input_tokens,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": output_tokens,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": input_tokens + output_tokens,
    })
}

fn message_output(text: &str) -> Value {
    json!([{
        "type": "message",
        "id": FAKE_ITEM_ID,
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }])
}

fn created_event() -> (String, Value) {
    (
        "response.created".to_owned(),
        json!({
            "type": "response.created",
            "response": {"id": FAKE_RESPONSE_ID, "status": "in_progress", "output": []},
        }),
    )
}

/// `response.output_text.delta` of the scripted message part.
#[must_use]
pub fn delta_event(text: &str) -> (String, Value) {
    (
        "response.output_text.delta".to_owned(),
        json!({
            "type": "response.output_text.delta",
            "item_id": FAKE_ITEM_ID,
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        }),
    )
}

/// `response.completed` with the full `text` and usage.
#[must_use]
pub fn completed_event(text: &str, input_tokens: i64, output_tokens: i64) -> (String, Value) {
    (
        "response.completed".to_owned(),
        json!({
            "type": "response.completed",
            "response": {
                "id": FAKE_RESPONSE_ID,
                "status": "completed",
                "output": message_output(text),
                "usage": usage_json(input_tokens, output_tokens),
            },
        }),
    )
}

impl ScriptedStream {
    /// `response.created`, one delta per chunk, `response.completed` with usage.
    #[must_use]
    pub fn text(chunks: &[&str], input_tokens: i64, output_tokens: i64) -> Self {
        let mut events = vec![created_event()];
        events.extend(chunks.iter().map(|c| delta_event(c)));
        events.push(completed_event(
            &chunks.concat(),
            input_tokens,
            output_tokens,
        ));
        Self::events(events)
    }

    /// `response.created` then `response.failed` with `message` (no usage).
    #[must_use]
    pub fn failed(message: &str) -> Self {
        Self::events(vec![
            created_event(),
            (
                "response.failed".to_owned(),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": FAKE_RESPONSE_ID,
                        "status": "failed",
                        "error": {"code": "server_error", "message": message},
                        "usage": null,
                    },
                }),
            ),
        ])
    }

    /// Upstream (provider) HTTP error `status` with JSON `body`.
    #[must_use]
    pub fn http(status: u16, body: Value) -> Self {
        Self {
            http_error: Some((status, body, None)),
            ..Self::default()
        }
    }

    /// Provider 429 with a numeric `Retry-After`.
    #[must_use]
    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self {
            http_error: Some((
                429,
                json!({"error": {"message": "Rate limit reached", "type": "requests", "code": "rate_limit_exceeded"}}),
                Some(retry_after_secs),
            )),
            ..Self::default()
        }
    }

    /// OAGW's own HTTP 504 `deadline_exceeded` Problem (gateway timeout).
    #[must_use]
    pub fn gateway_timeout() -> Self {
        Self {
            http_error: Some((
                504,
                json!({
                    "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~",
                    "title": "Deadline Exceeded",
                    "status": 504,
                    "detail": "upstream request timed out",
                }),
                None,
            )),
            error_source: ErrorSource::Gateway,
            ..Self::default()
        }
    }

    /// Deltas only: the stream ends without a terminal event.
    #[must_use]
    pub fn no_terminal(chunks: &[&str]) -> Self {
        let mut events = vec![created_event()];
        events.extend(chunks.iter().map(|c| delta_event(c)));
        Self::events(events)
    }

    /// Arbitrary events.
    #[must_use]
    pub fn events(events: Vec<(String, Value)>) -> Self {
        Self {
            events,
            ..Self::default()
        }
    }
}

/// A scripted answer to one non-streaming `POST …/responses`.
#[derive(Debug, Clone)]
struct ScriptedCompletion {
    text: String,
    usage: Option<LlmUsage>,
    /// Upstream error `(status, JSON body)` returned instead of a response.
    error: Option<(u16, Value)>,
}

/// One request received by the fake.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: Method,
    /// Full proxy path including the leading `/{alias}`.
    pub path: String,
    pub query: Option<String>,
    pub content_type: Option<String>,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    /// Body parsed as JSON, when it is JSON.
    pub json: Option<Value>,
    /// Security context the request was proxied with.
    pub subject_tenant_id: Uuid,
    pub subject_id: Uuid,
}

/// Provider endpoints the fake emulates.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Endpoint {
    /// Chat endpoint (`…/responses`, `…/chat/completions`, `…/messages`).
    Responses,
    UploadFile,
    DeleteFile(String),
    CreateVectorStore,
    DeleteVectorStore(String),
    AddVectorStoreFile(String),
    VectorStoreFileStatus(String, String),
    SearchVectorStore(String),
    NotFound,
}

impl Endpoint {
    fn classify(method: &Method, provider_path: &str) -> Self {
        if *method == Method::POST
            && ["/responses", "/chat/completions", "/messages"]
                .iter()
                .any(|p| provider_path.ends_with(p))
        {
            return Self::Responses;
        }
        // Storage paths: `/v1/...` (OpenAI) or `/openai/...` (Azure).
        let rest = provider_path
            .strip_prefix("/v1")
            .or_else(|| provider_path.strip_prefix("/openai"))
            .unwrap_or(provider_path);
        let segs: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
        match (method.as_str(), segs.as_slice()) {
            ("POST", ["files"]) => Self::UploadFile,
            ("DELETE", ["files", id]) => Self::DeleteFile((*id).to_owned()),
            ("POST", ["vector_stores"]) => Self::CreateVectorStore,
            ("DELETE", ["vector_stores", vs]) => Self::DeleteVectorStore((*vs).to_owned()),
            ("POST", ["vector_stores", vs, "files"]) => Self::AddVectorStoreFile((*vs).to_owned()),
            ("POST", ["vector_stores", vs, "search"]) => Self::SearchVectorStore((*vs).to_owned()),
            ("GET", ["vector_stores", vs, "files", f]) => {
                Self::VectorStoreFileStatus((*vs).to_owned(), (*f).to_owned())
            }
            _ => Self::NotFound,
        }
    }
}

/// A file stored through `POST …/files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeFile {
    pub id: String,
    /// Upstream alias the file was uploaded through.
    pub alias: String,
    /// Filename of the multipart `file` part.
    pub filename: String,
    /// Value of the multipart `purpose` field (empty when absent).
    pub purpose: String,
    /// Content type of the multipart `file` part.
    pub content_type: Option<String>,
    pub bytes: Bytes,
    /// Deleted through `DELETE …/files/{id}`.
    pub deleted: bool,
}

/// A vector store created through `POST …/vector_stores`.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeVectorStore {
    pub id: String,
    pub name: String,
    /// `(file_id, attributes)` added through `POST …/vector_stores/{id}/files`.
    pub files: Vec<(String, Value)>,
    pub deleted: bool,
}

#[derive(Default)]
struct State {
    streams: VecDeque<ScriptedStream>,
    completions: VecDeque<ScriptedCompletion>,
    requests: Vec<RecordedRequest>,
    failures: Vec<(String, u16)>,
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    files: Vec<FakeFile>,
    vector_stores: Vec<FakeVectorStore>,
    /// Scripted vector-store file statuses (add and status reads), in order.
    vs_statuses: VecDeque<String>,
    /// Status once `vs_statuses` is empty (default `completed`).
    default_vs_status: Option<String>,
    /// Path prefixes of requests to hold until [`FakeProvider::release_held`].
    holds: Vec<String>,
    /// Scripted vector-store search answers (text chunks), in order.
    search_results: VecDeque<Vec<String>>,
    next_id: u64,
}

/// Fake OAGW + OpenAI-compatible provider.
pub struct FakeProvider {
    state: Mutex<State>,
    gate: Arc<Semaphore>,
    held: Arc<Semaphore>,
    open_streams: Arc<AtomicUsize>,
}

impl FakeProvider {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            gate: Arc::new(Semaphore::new(0)),
            held: Arc::new(Semaphore::new(0)),
            open_streams: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue the answer to the next streaming request (default when the queue
    /// is empty: `ScriptedStream::text(&["Hello"], 10, 5)`).
    pub fn push_stream(&self, s: ScriptedStream) {
        self.state().streams.push_back(s);
    }

    /// Queue the answer to the next non-streaming `…/responses` request
    /// (default: text `"Summary"` with usage 10/5).
    pub fn push_completion(&self, text: &str, usage: Option<LlmUsage>) {
        self.state().completions.push_back(ScriptedCompletion {
            text: text.to_owned(),
            usage,
            error: None,
        });
    }

    /// Queue an upstream error (`status` with the JSON `body`) as the answer to
    /// the next non-streaming `…/responses` request.
    pub fn push_completion_error(&self, status: u16, body: Value) {
        self.state().completions.push_back(ScriptedCompletion {
            text: String::new(),
            usage: None,
            error: Some((status, body)),
        });
    }

    /// Let one held stream continue past its `hold_after` point (a release
    /// before the stream reaches it is remembered).
    pub fn release(&self) {
        self.gate.add_permits(1);
    }

    /// Answer the next request whose path (with or without the alias segment)
    /// starts with `path_prefix` with an upstream error `status`.
    pub fn fail_next(&self, path_prefix: &str, status: u16) {
        self.state().failures.push((path_prefix.to_owned(), status));
    }

    /// Queue the result chunks of the next vector-store search (default: no results).
    pub fn push_search_results(&self, chunks: Vec<&str>) {
        self.state()
            .search_results
            .push_back(chunks.into_iter().map(str::to_owned).collect());
    }

    /// Statuses returned, in order, by the next vector-store file adds and status
    /// reads (`""` = a response without `status`); afterwards the default status
    /// applies.
    pub fn set_vector_store_file_statuses(&self, statuses: Vec<&str>) {
        self.state().vs_statuses = statuses.into_iter().map(str::to_owned).collect();
    }

    /// Status of vector-store files once the scripted statuses are used up
    /// (initially `completed`).
    pub fn set_default_vector_store_file_status(&self, status: &str) {
        self.state().default_vs_status = Some(status.to_owned());
    }

    /// Hold the next request whose path (with or without the alias segment)
    /// starts with `path_prefix` until [`Self::release_held`].
    pub fn hold_next(&self, path_prefix: &str) {
        self.state().holds.push(path_prefix.to_owned());
    }

    /// Let one held request continue (a release before the request arrives is
    /// remembered).
    pub fn release_held(&self) {
        self.held.add_permits(1);
    }

    /// Files uploaded so far (deleted ones flagged).
    #[must_use]
    pub fn files(&self) -> Vec<FakeFile> {
        self.state().files.clone()
    }

    /// Vector stores created so far (deleted ones flagged).
    #[must_use]
    pub fn vector_stores(&self) -> Vec<FakeVectorStore> {
        self.state().vector_stores.clone()
    }

    /// Every request received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state().requests.clone()
    }

    /// JSON bodies of the requests to the Responses endpoint (chat and summary).
    #[must_use]
    pub fn chat_requests(&self) -> Vec<Value> {
        self.state()
            .requests
            .iter()
            .filter(|r| {
                Endpoint::classify(&r.method, &provider_path(&r.path)) == Endpoint::Responses
            })
            .filter_map(|r| r.json.clone())
            .collect()
    }

    /// Streaming response bodies not yet dropped by the consumer.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.open_streams.load(Ordering::SeqCst)
    }

    fn take_hold(&self, path: &str, provider_path: &str) -> bool {
        let mut st = self.state();
        let Some(pos) = st
            .holds
            .iter()
            .position(|p| path.starts_with(p.as_str()) || provider_path.starts_with(p.as_str()))
        else {
            return false;
        };
        st.holds.remove(pos);
        true
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut st = self.state();
        st.next_id += 1;
        format!("{prefix}{:016}", st.next_id)
    }

    fn next_vs_status(&self) -> String {
        let mut st = self.state();
        st.vs_statuses.pop_front().unwrap_or_else(|| {
            st.default_vs_status
                .clone()
                .unwrap_or_else(|| "completed".to_owned())
        })
    }

    async fn upload_file(
        &self,
        alias: &str,
        content_type: Option<&str>,
        body: Bytes,
    ) -> http::Response<Body> {
        let Some(boundary) = content_type.and_then(|ct| multer::parse_boundary(ct).ok()) else {
            return bad_request("multipart boundary required");
        };
        let stream = futures::stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
        let mut mp = multer::Multipart::new(stream, boundary);
        let (mut purpose, mut file) = (None, None);
        loop {
            match mp.next_field().await {
                Ok(Some(field)) => {
                    let name = field.name().map(str::to_owned);
                    let filename = field.file_name().map(str::to_owned);
                    let ct = field.content_type().map(ToString::to_string);
                    let Ok(data) = field.bytes().await else {
                        return bad_request("unreadable multipart field");
                    };
                    match name.as_deref() {
                        Some("purpose") => {
                            purpose = Some(String::from_utf8_lossy(&data).into_owned());
                        }
                        Some("file") => file = Some((filename.unwrap_or_default(), ct, data)),
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(_) => return bad_request("unreadable multipart body"),
            }
        }
        // The Anthropic Files API takes only the `file` part.
        let Some((filename, ct, data)) = file else {
            return bad_request("file is required");
        };
        let purpose = purpose.unwrap_or_default();
        let id = self.next_id("file-fake");
        let size = data.len();
        self.state().files.push(FakeFile {
            id: id.clone(),
            alias: alias.to_owned(),
            filename: filename.clone(),
            purpose: purpose.clone(),
            content_type: ct,
            bytes: data,
            deleted: false,
        });
        ok_json(&json!({
            "id": id, "object": "file", "bytes": size, "filename": filename, "purpose": purpose,
        }))
    }

    fn delete_file(&self, id: &str) -> http::Response<Body> {
        let mut st = self.state();
        match st.files.iter_mut().find(|f| f.id == id && !f.deleted) {
            Some(f) => {
                f.deleted = true;
                ok_json(&json!({"id": id, "object": "file", "deleted": true}))
            }
            None => not_found_json(),
        }
    }

    fn create_vector_store(&self, json: Option<&Value>) -> http::Response<Body> {
        let name = json
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let id = self.next_id("vs_fake");
        self.state().vector_stores.push(FakeVectorStore {
            id: id.clone(),
            name: name.clone(),
            files: Vec::new(),
            deleted: false,
        });
        ok_json(&json!({"id": id, "object": "vector_store", "name": name}))
    }

    fn delete_vector_store(&self, id: &str) -> http::Response<Body> {
        let mut st = self.state();
        match st
            .vector_stores
            .iter_mut()
            .find(|v| v.id == id && !v.deleted)
        {
            Some(v) => {
                v.deleted = true;
                ok_json(&json!({"id": id, "object": "vector_store.deleted", "deleted": true}))
            }
            None => not_found_json(),
        }
    }

    fn add_vector_store_file(&self, vs: &str, json: Option<&Value>) -> http::Response<Body> {
        let file_id = json
            .and_then(|v| v.get("file_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let attributes = json
            .and_then(|v| v.get("attributes"))
            .cloned()
            .unwrap_or(Value::Null);
        {
            let mut st = self.state();
            let Some(store) = st
                .vector_stores
                .iter_mut()
                .find(|v| v.id == vs && !v.deleted)
            else {
                return not_found_json();
            };
            store.files.push((file_id.clone(), attributes));
        }
        self.vs_file_response(vs, &file_id)
    }

    fn vs_file_response(&self, vs: &str, file_id: &str) -> http::Response<Body> {
        let status = self.next_vs_status();
        let mut body = json!({"id": file_id, "object": "vector_store.file", "vector_store_id": vs});
        if !status.is_empty() {
            body["status"] = json!(status);
        }
        ok_json(&body)
    }

    fn vector_store_file_status(&self, vs: &str, file_id: &str) -> http::Response<Body> {
        let known = self
            .state()
            .vector_stores
            .iter()
            .any(|v| v.id == vs && !v.deleted && v.files.iter().any(|(f, _)| f == file_id));
        if known {
            self.vs_file_response(vs, file_id)
        } else {
            not_found_json()
        }
    }

    fn search_vector_store(&self, vs: &str, json: Option<&Value>) -> http::Response<Body> {
        let query = json
            .and_then(|v| v.get("query"))
            .cloned()
            .unwrap_or(Value::Null);
        let chunks = self.state().search_results.pop_front().unwrap_or_default();
        let data: Vec<Value> = chunks
            .iter()
            .enumerate()
            .map(|(i, text)| {
                json!({
                    "file_id": format!("file-kb{i}"),
                    "filename": format!("kb{i}.pdf"),
                    "score": 0.9,
                    "attributes": {},
                    "content": [{"type": "text", "text": text}],
                })
            })
            .collect();
        ok_json(&json!({
            "object": "vector_store.search_results.page",
            "search_query": query,
            "vector_store_id": vs,
            "data": data,
            "has_more": false,
            "next_page": null,
        }))
    }

    fn take_failure(&self, path: &str, provider_path: &str) -> Option<u16> {
        let mut st = self.state();
        let pos = st.failures.iter().position(|(p, _)| {
            path.starts_with(p.as_str()) || provider_path.starts_with(p.as_str())
        })?;
        Some(st.failures.remove(pos).1)
    }

    fn responses(&self, json: Option<&Value>) -> http::Response<Body> {
        let streaming = json
            .and_then(|v| v.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if streaming {
            let script = self
                .state()
                .streams
                .pop_front()
                .unwrap_or_else(|| ScriptedStream::text(&["Hello"], 10, 5));
            self.stream_response(script)
        } else {
            let c = self
                .state()
                .completions
                .pop_front()
                .unwrap_or_else(|| ScriptedCompletion {
                    text: "Summary".to_owned(),
                    usage: Some(LlmUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..LlmUsage::default()
                    }),
                    error: None,
                });
            if let Some((status, body)) = c.error {
                return json_response(
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    &body,
                    None,
                    ErrorSource::Upstream,
                );
            }
            let usage = c.usage.map_or(Value::Null, |u| {
                json!({
                    "input_tokens": u.input_tokens,
                    "input_tokens_details": {"cached_tokens": u.cache_read_input_tokens},
                    "output_tokens": u.output_tokens,
                    "output_tokens_details": {"reasoning_tokens": u.reasoning_tokens},
                    "total_tokens": u.input_tokens + u.output_tokens,
                })
            });
            json_response(
                StatusCode::OK,
                &json!({
                    "id": FAKE_RESPONSE_ID,
                    "object": "response",
                    "status": "completed",
                    "output": message_output(&c.text),
                    "usage": usage,
                }),
                None,
                ErrorSource::Upstream,
            )
        }
    }

    fn stream_response(&self, script: ScriptedStream) -> http::Response<Body> {
        if let Some((status, body, retry_after)) = script.http_error {
            return json_response(
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                &body,
                retry_after,
                script.error_source,
            );
        }
        let frames: VecDeque<Bytes> = script
            .events
            .iter()
            .map(|(name, data)| Bytes::from(format!("event: {name}\ndata: {data}\n\n")))
            .collect();
        let state = FrameState {
            frames,
            next: 0,
            delay: script.delay_between,
            hold_at: script.hold_after,
            gate: Arc::clone(&self.gate),
            _guard: OpenStreamGuard::new(Arc::clone(&self.open_streams)),
        };
        let body: BodyStream = futures::stream::unfold(state, |mut st| async move {
            let frame = st.frames.pop_front()?;
            if st.hold_at == Some(st.next)
                && let Ok(permit) = Arc::clone(&st.gate).acquire_owned().await
            {
                permit.forget();
            }
            if st.next > 0 && !st.delay.is_zero() {
                tokio::time::sleep(st.delay).await;
            }
            st.next += 1;
            Some((Ok::<Bytes, BoxError>(frame), st))
        })
        .boxed();
        let mut resp = http::Response::new(Body::Stream(body));
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        resp.extensions_mut().insert(ErrorSource::Upstream);
        resp
    }
}

struct FrameState {
    frames: VecDeque<Bytes>,
    next: usize,
    delay: Duration,
    hold_at: Option<usize>,
    gate: Arc<Semaphore>,
    _guard: OpenStreamGuard,
}

/// Counts live streaming bodies.
struct OpenStreamGuard(Arc<AtomicUsize>);

impl OpenStreamGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for OpenStreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Path without the leading `/{alias}` segment.
fn provider_path(path: &str) -> String {
    path.trim_start_matches('/')
        .split_once('/')
        .map(|(_, rest)| format!("/{rest}"))
        .unwrap_or_default()
}

fn json_response(
    status: StatusCode,
    body: &Value,
    retry_after: Option<u64>,
    source: ErrorSource,
) -> http::Response<Body> {
    let mut resp = http::Response::new(Body::from(body.to_string()));
    *resp.status_mut() = status;
    let content_type = if source == ErrorSource::Gateway {
        "application/problem+json"
    } else {
        "application/json"
    };
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static(content_type),
    );
    if let Some(secs) = retry_after {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, http::HeaderValue::from(secs));
    }
    resp.extensions_mut().insert(source);
    resp
}

fn ok_json(body: &Value) -> http::Response<Body> {
    json_response(StatusCode::OK, body, None, ErrorSource::Upstream)
}

fn not_found_json() -> http::Response<Body> {
    json_response(
        StatusCode::NOT_FOUND,
        &json!({"error": {"message": "not found", "type": "invalid_request_error"}}),
        None,
        ErrorSource::Upstream,
    )
}

fn bad_request(message: &str) -> http::Response<Body> {
    json_response(
        StatusCode::BAD_REQUEST,
        &json!({"error": {"message": message, "type": "invalid_request_error"}}),
        None,
        ErrorSource::Upstream,
    )
}

fn not_found(what: &str) -> CanonicalError {
    CanonicalError::internal(format!("fake provider: {what} not found")).create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeProvider {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let alias = req.alias().map_or_else(
            || {
                req.server()
                    .endpoints
                    .first()
                    .map(|e| e.host.clone())
                    .unwrap_or_default()
            },
            str::to_owned,
        );
        let up = Upstream {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            alias,
            server: req.server().clone(),
            protocol: req.protocol().to_owned(),
            enabled: req.enabled(),
            auth: req.auth().cloned(),
            headers: req.headers().cloned(),
            plugins: req.plugins().cloned(),
            rate_limit: req.rate_limit().cloned(),
            cors: req.cors().cloned(),
            tags: req.tags().to_vec(),
        };
        self.state().upstreams.push(up.clone());
        Ok(up)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.state()
            .upstreams
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .ok_or_else(|| not_found("upstream"))
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(self.state().upstreams.clone())
    }

    async fn update_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.get_upstream(ctx, id).await
    }

    async fn delete_upstream(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.state().upstreams.retain(|u| u.id != id);
        Ok(())
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: req.plugins().cloned(),
            rate_limit: req.rate_limit().cloned(),
            cors: req.cors().cloned(),
            tags: req.tags().to_vec(),
            priority: req.priority(),
            enabled: req.enabled(),
        };
        self.state().routes.push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.state()
            .routes
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| not_found("route"))
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Ok(self
            .state()
            .routes
            .iter()
            .filter(|r| upstream_id.is_none_or(|id| r.upstream_id == id))
            .cloned()
            .collect())
    }

    async fn update_route(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.get_route(ctx, id).await
    }

    async fn delete_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.state().routes.retain(|r| r.id != id);
        Ok(())
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        let st = self.state();
        let up = st
            .upstreams
            .iter()
            .find(|u| u.alias == alias)
            .cloned()
            .ok_or_else(|| not_found("upstream"))?;
        let route = st
            .routes
            .iter()
            .find(|r| r.upstream_id == up.id)
            .cloned()
            .ok_or_else(|| not_found("route"))?;
        Ok((up, route))
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body = body.into_bytes().await.unwrap_or_default();
        let json = serde_json::from_slice::<Value>(&body).ok();
        let path = parts.uri.path().to_owned();
        let provider_path = provider_path(&path);
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.state().requests.push(RecordedRequest {
            method: parts.method.clone(),
            path: path.clone(),
            query: parts.uri.query().map(str::to_owned),
            content_type: content_type.clone(),
            headers: parts.headers.clone(),
            body: body.clone(),
            json: json.clone(),
            subject_tenant_id: ctx.subject_tenant_id(),
            subject_id: ctx.subject_id(),
        });

        if self.take_hold(&path, &provider_path)
            && let Ok(permit) = Arc::clone(&self.held).acquire_owned().await
        {
            permit.forget();
        }
        if let Some(status) = self.take_failure(&path, &provider_path) {
            return Ok(json_response(
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                &json!({"error": {"message": format!("injected failure {status}"), "type": "fake_error"}}),
                None,
                ErrorSource::Upstream,
            ));
        }
        Ok(match Endpoint::classify(&parts.method, &provider_path) {
            Endpoint::Responses => self.responses(json.as_ref()),
            Endpoint::UploadFile => {
                let alias = path
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap_or_default();
                self.upload_file(alias, content_type.as_deref(), body).await
            }
            Endpoint::DeleteFile(id) => self.delete_file(&id),
            Endpoint::CreateVectorStore => self.create_vector_store(json.as_ref()),
            Endpoint::DeleteVectorStore(id) => self.delete_vector_store(&id),
            Endpoint::AddVectorStoreFile(vs) => self.add_vector_store_file(&vs, json.as_ref()),
            Endpoint::VectorStoreFileStatus(vs, f) => self.vector_store_file_status(&vs, &f),
            Endpoint::SearchVectorStore(vs) => self.search_vector_store(&vs, json.as_ref()),
            Endpoint::NotFound => not_found_json(),
        })
    }
}
