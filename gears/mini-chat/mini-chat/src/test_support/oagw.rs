//! Fake OAGW: accepts upstream and route CRUD and records every proxied request. The fake provider
//! behind it answers proxied requests with scripted responses.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, Endpoint, ListQuery, Route, Scheme,
    ServiceGatewayClientV1, UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

#[resource_error(gts_id!("cf.core.mini_chat.test_oagw.v1~"))]
struct FakeOagwError;

/// A response the fake provider plays for a proxied request.
#[derive(Debug, Clone)]
pub enum ScriptedResponse {
    /// An `OpenAI` Responses API SSE stream: `(event name, data)` frames.
    Sse(Vec<(String, Value)>),
    /// The same, but the stream stays open after the frames until the client disconnects.
    SseThenHang(Vec<(String, Value)>),
    /// The same frames, then the response body panics when read (a provider adapter or parser
    /// panic in the middle of the stream).
    SseThenPanic(Vec<(String, Value)>),
    /// A plain JSON response.
    Json {
        /// HTTP status.
        status: u16,
        /// JSON body.
        body: Value,
    },
    /// An error response.
    Error {
        /// HTTP status.
        status: u16,
        /// JSON body.
        body: Value,
        /// Extra response headers.
        headers: Vec<(String, String)>,
    },
}

/// A request the fake provider received through the fake OAGW.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// HTTP method.
    pub method: String,
    /// Request path (with query).
    pub path: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Body parsed as JSON, when it is JSON.
    pub body_json: Option<Value>,
    /// Raw body.
    pub body_bytes: Vec<u8>,
    /// The upstream the fake OAGW resolved the first path segment (the alias, up to the first `/`)
    /// to; `None` when no upstream is registered under it.
    pub upstream: Option<Upstream>,
}

#[derive(Default)]
struct ProviderState {
    scripted: Vec<(String, VecDeque<ScriptedResponse>)>,
    requests: Vec<RecordedRequest>,
}

/// The fake LLM and RAG provider behind the fake OAGW.
#[derive(Clone, Default)]
pub struct FakeProvider {
    state: Arc<Mutex<ProviderState>>,
}

/// Paths that are LLM chat requests (as opposed to files and vector stores).
const CHAT_PATH_MARKERS: [&str; 3] = ["/responses", "/chat/completions", "/messages"];

impl FakeProvider {
    /// Queues a response for the next proxied request whose path contains `path_contains`
    /// (first in, first out per pattern).
    pub fn enqueue(&self, path_contains: &str, response: ScriptedResponse) {
        let mut state = self.state.lock();
        if let Some((_, queue)) = state
            .scripted
            .iter_mut()
            .find(|(pattern, _)| pattern == path_contains)
        {
            queue.push_back(response);
        } else {
            state
                .scripted
                .push((path_contains.to_owned(), VecDeque::from([response])));
        }
    }

    /// Every proxied request, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().requests.clone()
    }

    /// The proxied LLM chat requests only.
    pub fn chat_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| is_chat_path(&request.path))
            .collect()
    }

    fn respond(&self, request: RecordedRequest) -> ScriptedResponse {
        let mut state = self.state.lock();
        let path = request.path.clone();
        state.requests.push(request);
        let scripted = state
            .scripted
            .iter_mut()
            .find(|(pattern, queue)| path.contains(pattern.as_str()) && !queue.is_empty())
            .and_then(|(_, queue)| queue.pop_front());
        scripted.unwrap_or_else(|| {
            if is_chat_path(&path) {
                responses_text_stream(&["Hello"], 10, 5)
            } else {
                ScriptedResponse::Json {
                    status: 200,
                    body: json!({}),
                }
            }
        })
    }
}

fn is_chat_path(path: &str) -> bool {
    CHAT_PATH_MARKERS.iter().any(|marker| path.contains(marker))
}

/// A completed Responses API stream: one `response.output_text.delta` per chunk and a
/// `response.completed` frame with the given usage.
pub fn responses_text_stream(
    text_chunks: &[&str],
    input_tokens: u64,
    output_tokens: u64,
) -> ScriptedResponse {
    let mut frames = vec![(
        "response.created".to_owned(),
        json!({"type": "response.created", "response": {"id": "resp_0123456789abcdef0123", "status": "in_progress"}}),
    )];
    for chunk in text_chunks {
        frames.push((
            "response.output_text.delta".to_owned(),
            json!({"type": "response.output_text.delta", "delta": chunk}),
        ));
    }
    frames.push((
        "response.completed".to_owned(),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_0123456789abcdef0123",
                "status": "completed",
                "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
            }
        }),
    ));
    ScriptedResponse::Sse(frames)
}

/// A Responses API stream that ends with `response.failed`.
pub fn responses_failed(message: &str) -> ScriptedResponse {
    ScriptedResponse::Sse(vec![
        (
            "response.created".to_owned(),
            json!({"type": "response.created", "response": {"id": "resp_0123456789abcdef0123", "status": "in_progress"}}),
        ),
        (
            "response.failed".to_owned(),
            json!({
                "type": "response.failed",
                "response": {"id": "resp_0123456789abcdef0123", "status": "failed", "error": {"message": message}}
            }),
        ),
    ])
}

fn sse_text(frames: &[(String, Value)]) -> String {
    frames.iter().fold(String::new(), |text, (event, data)| {
        text + &format!("event: {event}\ndata: {data}\n\n")
    })
}

fn into_http(response: ScriptedResponse) -> http::Response<Body> {
    let builder = http::Response::builder();
    let built = match response {
        ScriptedResponse::Sse(frames) => builder
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::Bytes(Bytes::from(sse_text(&frames)))),
        ScriptedResponse::SseThenHang(frames) => {
            let head = futures::stream::once(async move {
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Bytes::from(sse_text(&frames)))
            });
            let hang = futures::stream::pending();
            builder
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::Stream(Box::pin(futures::StreamExt::chain(
                    head, hang,
                ))))
        }
        ScriptedResponse::SseThenPanic(frames) => {
            let head = futures::stream::once(async move {
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Bytes::from(sse_text(&frames)))
            });
            let boom = futures::stream::poll_fn(|_| -> std::task::Poll<Option<_>> {
                panic!("injected provider body panic")
            });
            builder
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::Stream(Box::pin(futures::StreamExt::chain(
                    head, boom,
                ))))
        }
        ScriptedResponse::Json { status, body } => builder
            .status(status)
            .header("content-type", "application/json")
            .body(Body::Bytes(Bytes::from(body.to_string()))),
        ScriptedResponse::Error {
            status,
            body,
            headers,
        } => {
            let mut builder = builder
                .status(status)
                .header("content-type", "application/json");
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            builder.body(Body::Bytes(Bytes::from(body.to_string())))
        }
    };
    built.unwrap_or_else(|_| http::Response::new(Body::Empty))
}

#[derive(Default)]
struct Registry {
    upstreams: HashMap<Uuid, Upstream>,
    routes: HashMap<Uuid, Route>,
}

/// The fake OAGW client registered in the `ClientHub`.
pub struct FakeOagw {
    provider: FakeProvider,
    registry: Mutex<Registry>,
}

impl FakeOagw {
    pub fn new(provider: FakeProvider) -> Self {
        Self {
            provider,
            registry: Mutex::new(Registry::default()),
        }
    }
}

/// OAGW's host normalization: IPv6 brackets stripped, lowercase, trailing dots stripped.
fn normalized_host(endpoint: &Endpoint) -> String {
    let host = &endpoint.host;
    let inner = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    inner.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

fn is_ip(endpoint: &Endpoint) -> bool {
    normalized_host(endpoint).parse::<IpAddr>().is_ok()
}

/// OAGW's scheme-aware standard port: 80 for HTTP, 443 for HTTPS and the other schemes.
fn is_standard_port(endpoint: &Endpoint) -> bool {
    match endpoint.scheme {
        Scheme::Http => endpoint.port == 80,
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => endpoint.port == 443,
    }
}

/// OAGW's `compute_derived_alias` for the single-host case (all the gear provisions): a hostname
/// derives `host`, plus `:port` for a non-standard port; an IP host derives nothing.
fn derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints.iter().all(is_ip) {
        return None;
    }
    let contribution = |endpoint: &Endpoint| {
        let host = normalized_host(endpoint);
        if is_standard_port(endpoint) {
            host
        } else {
            format!("{host}:{}", endpoint.port)
        }
    };
    let own = contribution(first);
    endpoints
        .iter()
        .all(|endpoint| contribution(endpoint) == own)
        .then_some(own)
}

fn invalid_alias(message: &str) -> CanonicalError {
    FakeOagwError::invalid_argument()
        .with_field_violation("alias", message, "INVALID_VALUE")
        .create()
}

/// OAGW's `validate_alias`: non-empty, at most 253 characters, alphanumerics and `.:-_` only, at
/// least one alphanumeric, not a dot-segment.
fn validate_alias(alias: &str) -> Result<(), CanonicalError> {
    let charset = alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_'));
    if alias.is_empty() || alias.len() > 253 || !charset || alias == "." || alias == ".." {
        return Err(invalid_alias("alias is invalid"));
    }
    if !alias.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(invalid_alias(
            "alias must contain an alphanumeric character",
        ));
    }
    Ok(())
}

/// OAGW's alias rules on upstream creation (`enforce_alias_create_with` and
/// `enforce_alias_create_derived`): a hostname endpoint derives its alias and rejects any other
/// explicit one; an IP endpoint requires an explicit alias.
fn enforce_alias_create(
    user_alias: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<String, CanonicalError> {
    let normalize = |alias: &str| alias.to_ascii_lowercase().trim_end_matches('.').to_owned();
    match (derived_alias(endpoints), user_alias) {
        (Some(derived), Some(user)) => {
            if normalize(user) != derived {
                return Err(invalid_alias(&format!(
                    "alias is auto-derived for hostname-based endpoints; remove the 'alias' field (derived: '{derived}')"
                )));
            }
            validate_alias(&derived)?;
            Ok(derived)
        }
        (Some(derived), None) => {
            validate_alias(&derived)?;
            Ok(derived)
        }
        (None, Some(user)) => {
            let alias = normalize(user);
            validate_alias(&alias)?;
            Ok(alias)
        }
        (None, None) => Err(invalid_alias(
            "explicit alias is required for IP-based or heterogeneous-host endpoints",
        )),
    }
}

fn not_found(kind: &str, id: Uuid) -> CanonicalError {
    FakeOagwError::not_found(format!("{kind} not found"))
        .with_resource(id.to_string())
        .create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        let alias = enforce_alias_create(req.alias(), &req.server().endpoints)?;
        if self
            .registry
            .lock()
            .upstreams
            .values()
            .any(|upstream| upstream.tenant_id == tenant_id && upstream.alias == alias)
        {
            return Err(
                FakeOagwError::already_exists("an upstream with this alias exists")
                    .with_resource(alias)
                    .create(),
            );
        }
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
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
        self.registry
            .lock()
            .upstreams
            .insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.registry
            .lock()
            .upstreams
            .get(&id)
            .cloned()
            .ok_or_else(|| not_found("upstream", id))
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(self.registry.lock().upstreams.values().cloned().collect())
    }

    async fn update_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let mut registry = self.registry.lock();
        let existing = registry
            .upstreams
            .get(&id)
            .cloned()
            .ok_or_else(|| not_found("upstream", id))?;
        let upstream = Upstream {
            id,
            tenant_id: ctx.subject_tenant_id(),
            alias: req.alias().map_or(existing.alias, str::to_owned),
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
        registry.upstreams.insert(id, upstream.clone());
        Ok(upstream)
    }

    async fn delete_upstream(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.registry
            .lock()
            .upstreams
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| not_found("upstream", id))
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
        self.registry.lock().routes.insert(route.id, route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.registry
            .lock()
            .routes
            .get(&id)
            .cloned()
            .ok_or_else(|| not_found("route", id))
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Ok(self
            .registry
            .lock()
            .routes
            .values()
            .filter(|route| upstream_id.is_none_or(|id| route.upstream_id == id))
            .cloned()
            .collect())
    }

    async fn update_route(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let mut registry = self.registry.lock();
        let existing = registry
            .routes
            .get(&id)
            .cloned()
            .ok_or_else(|| not_found("route", id))?;
        let route = Route {
            id,
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: existing.upstream_id,
            match_rules: req.match_rules().clone(),
            plugins: req.plugins().cloned(),
            rate_limit: req.rate_limit().cloned(),
            cors: req.cors().cloned(),
            tags: req.tags().to_vec(),
            priority: req.priority(),
            enabled: req.enabled(),
        };
        registry.routes.insert(id, route.clone());
        Ok(route)
    }

    async fn delete_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.registry
            .lock()
            .routes
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| not_found("route", id))
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        let registry = self.registry.lock();
        let upstream = registry
            .upstreams
            .values()
            .find(|upstream| upstream.alias == alias)
            .cloned()
            .ok_or_else(|| {
                FakeOagwError::not_found("alias not found")
                    .with_resource(alias)
                    .create()
            })?;
        let route = registry
            .routes
            .values()
            .find(|route| route.upstream_id == upstream.id)
            .cloned()
            .ok_or_else(|| not_found("route", upstream.id))?;
        Ok((upstream, route))
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body_bytes = body
            .into_bytes()
            .await
            .map_err(|err| CanonicalError::internal(format!("fake OAGW body: {err}")).create())?
            .to_vec();
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let alias = parts
            .uri
            .path()
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let upstream = self
            .registry
            .lock()
            .upstreams
            .values()
            .find(|upstream| upstream.alias == alias)
            .cloned();
        let recorded = RecordedRequest {
            method: parts.method.to_string(),
            path: parts
                .uri
                .path_and_query()
                .map_or_else(|| parts.uri.path().to_owned(), |pq| pq.as_str().to_owned()),
            headers,
            body_json: serde_json::from_slice(&body_bytes).ok(),
            body_bytes,
            upstream,
        };
        Ok(into_http(self.provider.respond(recorded)))
    }
}
