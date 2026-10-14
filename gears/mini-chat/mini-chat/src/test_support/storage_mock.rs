//! The mock OAGW of the attachments feature: a `ServiceGatewayClientV1` that records every
//! proxied Files, Vector Stores and Anthropic Files request (URI, method, S2S security context,
//! headers, raw body and the parsed multipart parts) and answers per script by route.
//!
//! No network is used. The route of a request is derived from its URI `/{alias}{prefix}{path}`
//! (prefix `/v1` or `/openai`). A request whose body stream fails before it ends (the gear's byte
//! counter aborted the upload) is not counted as a completed upload and is answered with a
//! gateway error.
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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route as OagwRoute,
    UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::provider::{Gate, GatewayFailure};

/// The RAG route kinds of the provider storage APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    /// `POST {prefix}/files`
    FilesUpload,
    /// `DELETE {prefix}/files/{id}`
    FilesDelete,
    /// `POST {prefix}/vector_stores`
    VsCreate,
    /// `DELETE {prefix}/vector_stores/{id}`
    VsDelete,
    /// `POST {prefix}/vector_stores/{id}/files`
    VsAddFile,
    /// `GET {prefix}/vector_stores/{id}/files/{file_id}`
    VsReadFile,
    /// `POST {prefix}/vector_stores/{id}/search` (the knowledge search of FEATURE 0005).
    VsSearch,
    /// Anything else.
    Other,
}

/// One part of a recorded multipart body.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Bytes,
}

/// One recorded proxied request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub subject: Uuid,
    pub tenant: Uuid,
    pub method: String,
    /// The request URI as the gear built it, with its query.
    pub uri: String,
    /// The first path segment: the OAGW upstream alias.
    pub alias: String,
    /// The path after the alias, without the query (`/v1/files`).
    pub path: String,
    pub query: Option<String>,
    pub route: Route,
    /// The vector store id or file id of the path, when the route has one.
    pub resource_id: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// The multipart parts, when the body is `multipart/form-data`.
    pub parts: Vec<Part>,
}

impl Recorded {
    /// The value of a header (names are lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The JSON body (`Null` when it is not JSON).
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The multipart part named `name`.
    pub fn part(&self, name: &str) -> Option<&Part> {
        self.parts.iter().find(|part| part.name == name)
    }

    /// The text of the multipart part named `name`.
    pub fn text_part(&self, name: &str) -> Option<String> {
        self.part(name)
            .map(|part| String::from_utf8_lossy(&part.data).into_owned())
    }
}

/// How the mock answers one request.
#[derive(Clone)]
pub enum Answer {
    /// A JSON response with `status`; a non-2xx answer is an upstream error.
    Json { status: u16, body: Value },
    /// `Err(CanonicalError)` from `proxy_request`.
    Fail(GatewayFailure),
    /// The provider never answers.
    Pending,
    /// The provider answers 200 and then never sends the body.
    StalledBody,
    /// Answer `then` once the gate is open.
    Gated(Arc<Gate>, Box<Answer>),
}

impl Answer {
    pub fn ok(body: &Value) -> Self {
        Self::Json {
            status: 200,
            body: body.clone(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self::Json {
            status,
            body: json!({ "error": { "message": "provider-secret-error-text", "code": status } }),
        }
    }

    /// The indexing status of a vector store file: `{"id": "...", "status": status}`.
    pub fn indexing(status: &str) -> Self {
        Self::ok(&json!({ "id": "file-x", "object": "vector_store.file", "status": status }))
    }

    /// A vector store file response without a `status` field.
    pub fn no_status() -> Self {
        Self::ok(&json!({ "id": "file-x", "object": "vector_store.file" }))
    }

    pub fn gated(gate: &Arc<Gate>, then: Self) -> Self {
        Self::Gated(Arc::clone(gate), Box::new(then))
    }
}

#[derive(Default)]
struct Scripts {
    queued: HashMap<(Option<String>, Route), VecDeque<Answer>>,
    defaults: HashMap<(Option<String>, Route), Answer>,
}

/// The recording and scripted mock gateway.
pub struct MockStorage {
    scripts: Mutex<Scripts>,
    calls: Mutex<Vec<Recorded>>,
    counter: AtomicUsize,
    completed_uploads: AtomicUsize,
    arrived: Notify,
}

impl MockStorage {
    /// A mock that answers every route with a success.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(Scripts::default()),
            calls: Mutex::new(Vec::new()),
            counter: AtomicUsize::new(0),
            completed_uploads: AtomicUsize::new(0),
            arrived: Notify::new(),
        })
    }

    /// Answer the next unanswered `route` call with `answer` (queued answers are used in order,
    /// then the default).
    pub fn queue(&self, route: Route, answer: Answer) {
        self.scripts
            .lock()
            .unwrap()
            .queued
            .entry((None, route))
            .or_default()
            .push_back(answer);
    }

    /// As [`queue`](Self::queue), for calls to one upstream alias only.
    pub fn queue_for(&self, alias: &str, route: Route, answer: Answer) {
        self.scripts
            .lock()
            .unwrap()
            .queued
            .entry((Some(alias.to_owned()), route))
            .or_default()
            .push_back(answer);
    }

    /// Queue `answer` `count` times.
    pub fn queue_n(&self, route: Route, answer: &Answer, count: usize) {
        for _ in 0..count {
            self.queue(route, answer.clone());
        }
    }

    /// The answer of `route` when nothing is queued.
    pub fn set_default(&self, route: Route, answer: Answer) {
        self.scripts
            .lock()
            .unwrap()
            .defaults
            .insert((None, route), answer);
    }

    /// As [`set_default`](Self::set_default), for one upstream alias only.
    pub fn set_default_for(&self, alias: &str, route: Route, answer: Answer) {
        self.scripts
            .lock()
            .unwrap()
            .defaults
            .insert((Some(alias.to_owned()), route), answer);
    }

    /// Every recorded request, in arrival order.
    pub fn calls(&self) -> Vec<Recorded> {
        self.calls.lock().unwrap().clone()
    }

    /// The recorded requests of `route`.
    pub fn calls_of(&self, route: Route) -> Vec<Recorded> {
        self.calls()
            .into_iter()
            .filter(|call| call.route == route)
            .collect()
    }

    pub fn count_of(&self, route: Route) -> usize {
        self.calls_of(route).len()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// Uploads whose body arrived completely.
    pub fn completed_uploads(&self) -> usize {
        self.completed_uploads.load(Ordering::SeqCst)
    }

    /// Wait (5 s of real time at most) until `count` requests of `route` arrived.
    pub async fn wait_for(&self, route: Route, count: usize) {
        let wait = async {
            loop {
                let notified = self.arrived.notified();
                if self.count_of(route) >= count {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("{count} {route:?} request(s) never arrived"));
    }

    fn next_answer(&self, alias: &str, route: Route) -> Option<Answer> {
        let mut scripts = self.scripts.lock().unwrap();
        let alias_key = Some(alias.to_owned());
        if let Some(answer) = scripts
            .queued
            .get_mut(&(alias_key.clone(), route))
            .and_then(VecDeque::pop_front)
        {
            return Some(answer);
        }
        if let Some(answer) = scripts
            .queued
            .get_mut(&(None, route))
            .and_then(VecDeque::pop_front)
        {
            return Some(answer);
        }
        scripts
            .defaults
            .get(&(alias_key, route))
            .or_else(|| scripts.defaults.get(&(None, route)))
            .cloned()
    }

    fn builtin(&self, alias: &str, recorded: &Recorded) -> Answer {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        match recorded.route {
            Route::FilesUpload if alias.contains("anthropic") => {
                Answer::ok(&json!({ "id": format!("file_ant_{n}"), "type": "file" }))
            }
            Route::FilesUpload => Answer::ok(&json!({
                "id": format!("file-{n}"), "object": "file", "purpose": "assistants",
                "bytes": recorded.part("file").map_or(0, |part| part.data.len())
            })),
            Route::FilesDelete | Route::VsDelete => {
                Answer::ok(&json!({ "id": recorded.resource_id, "deleted": true }))
            }
            Route::VsCreate => Answer::ok(&json!({ "id": format!("vs_{n}"),
                "object": "vector_store" })),
            Route::VsAddFile | Route::VsReadFile => Answer::indexing("completed"),
            Route::VsSearch => Answer::ok(&json!({
                "object": "vector_store.search_results.page", "data": []
            })),
            Route::Other => Answer::status(404),
        }
    }
}

/// Split `/{alias}{prefix}{rest}` into the alias, the path after the alias, the route and the
/// resource id of the path.
fn classify(method: &str, path: &str) -> (String, String, Route, Option<String>) {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let (alias, rest) = trimmed
        .split_once('/')
        .map_or((trimmed, ""), |(alias, rest)| (alias, rest));
    let rest = format!("/{rest}");
    let tail = rest
        .strip_prefix("/v1")
        .or_else(|| rest.strip_prefix("/openai"))
        .unwrap_or(&rest);
    let segments: Vec<&str> = tail.trim_start_matches('/').split('/').collect();
    let (route, id) = match (method, segments.as_slice()) {
        ("POST", ["files"]) => (Route::FilesUpload, None),
        ("DELETE", ["files", id]) => (Route::FilesDelete, Some((*id).to_owned())),
        ("POST", ["vector_stores"]) => (Route::VsCreate, None),
        ("DELETE", ["vector_stores", id]) => (Route::VsDelete, Some((*id).to_owned())),
        ("POST", ["vector_stores", id, "files"]) => (Route::VsAddFile, Some((*id).to_owned())),
        ("GET", ["vector_stores", id, "files", _]) => (Route::VsReadFile, Some((*id).to_owned())),
        ("POST", ["vector_stores", id, "search"]) => (Route::VsSearch, Some((*id).to_owned())),
        _ => (Route::Other, None),
    };
    (alias.to_owned(), rest, route, id)
}

async fn parse_multipart(content_type: Option<&str>, body: &Bytes) -> Vec<Part> {
    let Some(boundary) = content_type.and_then(|value| multer::parse_boundary(value).ok()) else {
        return Vec::new();
    };
    let stream = futures::stream::once({
        let body = body.clone();
        async move { Ok::<Bytes, std::convert::Infallible>(body) }
    });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut parts = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_owned();
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(ToString::to_string);
        let data = field.bytes().await.unwrap_or_default();
        parts.push(Part {
            name,
            filename,
            content_type,
            data,
        });
    }
    parts
}

#[async_trait]
impl ServiceGatewayClientV1 for MockStorage {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        _req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        _req: CreateRouteRequest,
    ) -> Result<OagwRoute, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn get_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<OagwRoute, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<OagwRoute>, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<OagwRoute, CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, OagwRoute), CanonicalError> {
        unimplemented!("the storage mock only proxies")
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let method = parts.method.to_string();
        let (alias, path, route, resource_id) = classify(&method, parts.uri.path());
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let is_upload = route == Route::FilesUpload;
        let Ok(bytes) = body.into_bytes().await else {
            // the request body stream failed: the gear dropped the upload
            return Err(CanonicalError::service_unavailable()
                .with_retry_after_seconds(1)
                .with_detail("the request body stream failed")
                .create());
        };
        if is_upload {
            self.completed_uploads.fetch_add(1, Ordering::SeqCst);
        }
        let content_type = headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.clone());
        let multipart = parse_multipart(content_type.as_deref(), &bytes).await;
        let recorded = Recorded {
            subject: ctx.subject_id(),
            tenant: ctx.subject_tenant_id(),
            method,
            uri: parts.uri.to_string(),
            alias: alias.clone(),
            path,
            query: parts.uri.query().map(str::to_owned),
            route,
            resource_id,
            headers,
            body: bytes,
            parts: multipart,
        };
        self.calls.lock().unwrap().push(recorded.clone());
        self.arrived.notify_waiters();
        let mut answer = self
            .next_answer(&alias, route)
            .unwrap_or_else(|| self.builtin(&alias, &recorded));
        loop {
            match answer {
                Answer::Gated(gate, then) => {
                    gate.wait().await;
                    answer = *then;
                }
                Answer::Json { status, body } => {
                    let mut response = http::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .expect("response");
                    response.extensions_mut().insert(ErrorSource::Upstream);
                    return Ok(response);
                }
                Answer::Fail(failure) => return Err(failure_error(failure)),
                Answer::StalledBody => {
                    let stalled: oagw_sdk::body::BodyStream = Box::pin(futures::stream::pending());
                    let mut response = http::Response::builder()
                        .status(200)
                        .body(Body::from(stalled))
                        .expect("response");
                    response.extensions_mut().insert(ErrorSource::Upstream);
                    return Ok(response);
                }
                Answer::Pending => {
                    std::future::pending::<()>().await;
                    unreachable!("pending never completes")
                }
            }
        }
    }
}

fn failure_error(failure: GatewayFailure) -> CanonicalError {
    match failure {
        GatewayFailure::Timeout => CanonicalError::service_unavailable()
            .with_retry_after_seconds(1)
            .with_detail("gateway timeout")
            .create(),
        GatewayFailure::Unavailable => CanonicalError::service_unavailable()
            .with_retry_after_seconds(5)
            .with_detail("the upstream is unavailable")
            .create(),
        GatewayFailure::Internal => CanonicalError::internal("oagw exploded").create(),
    }
}
