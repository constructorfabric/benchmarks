//! Scripted in-memory OAGW: [`FakeGateway`] implements `ServiceGatewayClientV1`.
//!
//! # Proxy calls
//! [`FakeGateway::proxy_request`](ServiceGatewayClientV1::proxy_request) records every request as a
//! [`RecordedRequest`] (streamed request bodies are buffered) and answers it with the matching
//! rule registered **last** (so a test can override defaults installed earlier). A rule matches on
//! the HTTP method and a substring of the URI **path** (the query is ignored for matching but kept
//! in [`RecordedRequest::uri`]):
//! - [`FakeGateway::on`] installs a [`Responder`] that answers every matching request;
//! - [`FakeGateway::on_sequence`] installs responders consumed one per matching request; once the
//!   sequence is exhausted the rule no longer matches and earlier rules are tried;
//! - [`FakeGateway::on_matching`] / [`FakeGateway::on_sequence_matching`] additionally require the
//!   JSON request body to satisfy a predicate (e.g. the non-streaming summary call among chat
//!   calls on the same path); non-JSON bodies never match such a rule.
//!
//! [`FakeGateway::clear_rules`] / [`FakeGateway::clear_requests`] reset the script / the record.
//! [`Responder::Delayed`] delays the response head; [`Responder::Hang`] never answers. A
//! `proxy_request` future dropped while such an answer is pending counts in
//! [`FakeGateway::dropped_requests`] (non-streaming timeout / cancellation tests).
//!
//! A request no rule matches gets a gateway `404` (`ErrorSource::Gateway`) whose body names the
//! method and path, so a missing script shows up in assertions instead of hanging.
//!
//! Upstream-style responses carry the `oagw_sdk::api::ErrorSource::Upstream` extension;
//! [`Responder::GatewayStatus`] carries `ErrorSource::Gateway`, like an OAGW-generated error
//! (e.g. a 504 upstream timeout). [`Responder::Err`] makes `proxy_request` itself fail.
//!
//! # SSE
//! [`Responder::Sse`] returns `200 text/event-stream` with a `Body::Stream` that plays the
//! [`SseScript`] items in order, one chunk per `Event`/`Raw` item. `Delay` sleeps (tokio time, so
//! paused-clock tests advance instantly), `Gate` waits until the test notifies it and `Hang` never
//! yields again. When such a body is
//! dropped while script items remain (an `Event`/`Raw` not yet yielded, or a pending `Hang`;
//! dropping during a `Delay` before such an item counts too), [`FakeGateway::dropped_bodies`] is
//! incremented: that is a client cancellation. Once the last `Event`/`Raw` item has been yielded
//! the body counts as complete, whether or not the end of the stream is polled (adapters stop at
//! their terminal event).
//!
//! # Provisioning
//! Upstream and route CRUD is kept in memory (insertion order; `ListQuery` `skip`/`top` honoured;
//! a missing id is a `not_found` error). An upstream created without an alias gets
//! `Endpoint::alias_contribution` of its first endpoint. Inspect with [`FakeGateway::upstreams`] and
//! [`FakeGateway::routes`]. The calling `SecurityContext` is ignored except for the tenant id
//! stored on created entities.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::BodyStream;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, HttpMethod, ListQuery, Route,
    ServiceGatewayClientV1, UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct UpstreamResource;

#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
struct RouteResource;

/// One step of a scripted SSE body.
#[derive(Debug, Clone)]
pub enum SseScript {
    /// `event: {name}\ndata: {compact json}\n\n`.
    Event(String, Value),
    /// Sent verbatim (use for data-only lines, comments, malformed frames, `[DONE]`).
    Raw(String),
    /// Waits before the next item.
    Delay(Duration),
    /// Never yields again; the body stays open until dropped.
    Hang,
    /// Waits until the [`tokio::sync::Notify`] is notified (`notify_one` before the wait also
    /// releases it): a hang the test ends when it chooses.
    Gate(Arc<tokio::sync::Notify>),
    /// The body yields a transport error with this message (a connection lost mid-stream).
    Fail(String),
}

impl SseScript {
    /// `SseScript::Event` from a `&str` name.
    pub fn event(name: &str, data: Value) -> Self {
        Self::Event(name.to_owned(), data)
    }
}

/// Scripted answer of [`FakeGateway::proxy_request`].
#[derive(Debug, Clone)]
pub enum Responder {
    /// Upstream response with `content-type: application/json`.
    Json(u16, Value),
    /// Like `Json`, plus the given response headers.
    JsonWithHeaders(u16, Vec<(String, String)>, Value),
    /// Upstream `200 text/event-stream` streaming the script.
    Sse(Vec<SseScript>),
    /// `proxy_request` returns this error (transport / OAGW failure before any response).
    Err(CanonicalError),
    /// OAGW-generated error response with this status (`ErrorSource::Gateway`).
    GatewayStatus(u16),
    /// Upstream response with this status whose body fails to read with the given message.
    BodyError(u16, String),
    /// Waits (tokio time) before answering with the inner responder: a slow response head.
    Delayed(Duration, Box<Responder>),
    /// Never answers; dropping the `proxy_request` future counts in
    /// [`FakeGateway::dropped_requests`].
    Hang,
}

impl Responder {
    /// [`Responder::Json`].
    pub fn json(status: u16, body: Value) -> Self {
        Self::Json(status, body)
    }

    /// [`Responder::JsonWithHeaders`] from string-slice pairs.
    pub fn json_with_headers(status: u16, headers: &[(&str, &str)], body: Value) -> Self {
        Self::JsonWithHeaders(
            status,
            headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body,
        )
    }
}

/// A request seen by [`FakeGateway::proxy_request`].
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: Method,
    /// Path and query, e.g. `/127.0.0.1/v1/responses`.
    pub uri: String,
    pub headers: HeaderMap,
    /// The full request body (streams are buffered).
    pub body: Bytes,
    /// `body` parsed as JSON, when it is JSON.
    pub json: Option<Value>,
}

enum Script {
    Always(Responder),
    Sequence(VecDeque<Responder>),
}

/// Predicate on the JSON request body of a rule.
pub type BodyMatcher = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

struct Rule {
    method: Method,
    path: String,
    /// `None` matches any body.
    body: Option<BodyMatcher>,
    script: Script,
}

impl Rule {
    fn matches(&self, method: &Method, path: &str, json: Option<&Value>) -> bool {
        self.method == *method
            && path.contains(self.path.as_str())
            && self
                .body
                .as_ref()
                .is_none_or(|matcher| json.is_some_and(|body| matcher(body)))
    }
}

#[derive(Default)]
struct State {
    rules: Vec<Rule>,
    requests: Vec<RecordedRequest>,
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    create_upstream_script: VecDeque<Result<(), CanonicalError>>,
    create_route_script: VecDeque<Result<(), CanonicalError>>,
    create_upstream_calls: usize,
    create_route_calls: usize,
}

/// Scripted in-memory OAGW; see the [module docs](self).
#[derive(Default)]
pub struct FakeGateway {
    state: Mutex<State>,
    dropped_bodies: Arc<AtomicUsize>,
    dropped_requests: Arc<AtomicUsize>,
}

impl FakeGateway {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("FakeGateway lock")
    }

    /// Answers every request with `method` whose path contains `path` with `responder`.
    pub fn on(&self, method: Method, path: &str, responder: Responder) -> &Self {
        self.push_rule(method, path, None, Script::Always(responder))
    }

    /// Answers matching requests with `responders`, one each, in order; afterwards the rule no
    /// longer matches.
    pub fn on_sequence(&self, method: Method, path: &str, responders: Vec<Responder>) -> &Self {
        self.push_rule(method, path, None, Script::Sequence(responders.into()))
    }

    /// Like [`Self::on`], only for requests whose JSON body satisfies `body`.
    pub fn on_matching(
        &self,
        method: Method,
        path: &str,
        body: BodyMatcher,
        responder: Responder,
    ) -> &Self {
        self.push_rule(method, path, Some(body), Script::Always(responder))
    }

    /// Like [`Self::on_sequence`], only for requests whose JSON body satisfies `body`.
    pub fn on_sequence_matching(
        &self,
        method: Method,
        path: &str,
        body: BodyMatcher,
        responders: Vec<Responder>,
    ) -> &Self {
        self.push_rule(
            method,
            path,
            Some(body),
            Script::Sequence(responders.into()),
        )
    }

    fn push_rule(
        &self,
        method: Method,
        path: &str,
        body: Option<BodyMatcher>,
        script: Script,
    ) -> &Self {
        self.state().rules.push(Rule {
            method,
            path: path.to_owned(),
            body,
            script,
        });
        self
    }

    /// Removes every rule (later requests get the unmatched gateway 404 until new rules exist).
    pub fn clear_rules(&self) -> &Self {
        self.state().rules.clear();
        self
    }

    /// Forgets the recorded requests.
    pub fn clear_requests(&self) -> &Self {
        self.state().requests.clear();
        self
    }

    /// Every proxied request so far, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state().requests.clone()
    }

    /// Proxied requests with `method` whose path contains `path`.
    pub fn requests_to(&self, method: &Method, path: &str) -> Vec<RecordedRequest> {
        self.state()
            .requests
            .iter()
            .filter(|r| r.method == *method && uri_path(&r.uri).contains(path))
            .cloned()
            .collect()
    }

    /// Number of SSE response bodies dropped while script items remained (cancellations).
    pub fn dropped_bodies(&self) -> usize {
        self.dropped_bodies.load(Ordering::SeqCst)
    }

    /// Number of `proxy_request` futures dropped before they answered (while a
    /// [`Responder::Hang`] or [`Responder::Delayed`] was pending).
    pub fn dropped_requests(&self) -> usize {
        self.dropped_requests.load(Ordering::SeqCst)
    }

    /// Queues one scripted result per `create_upstream` call, consumed in order: `Err` fails the
    /// call, `Ok(())` lets it proceed normally. Once drained, calls proceed normally.
    pub fn script_create_upstream(&self, results: Vec<Result<(), CanonicalError>>) -> &Self {
        self.state().create_upstream_script.extend(results);
        self
    }

    /// Like [`Self::script_create_upstream`] for `create_route`.
    pub fn script_create_route(&self, results: Vec<Result<(), CanonicalError>>) -> &Self {
        self.state().create_route_script.extend(results);
        self
    }

    /// Number of `create_upstream` calls so far (scripted failures included).
    pub fn create_upstream_calls(&self) -> usize {
        self.state().create_upstream_calls
    }

    /// Number of `create_route` calls so far (scripted failures included).
    #[allow(dead_code)] // for later provisioning-related tests
    pub fn create_route_calls(&self) -> usize {
        self.state().create_route_calls
    }

    /// Upstreams currently provisioned, in creation order.
    pub fn upstreams(&self) -> Vec<Upstream> {
        self.state().upstreams.clone()
    }

    /// Routes currently provisioned, in creation order.
    pub fn routes(&self) -> Vec<Route> {
        self.state().routes.clone()
    }

    fn next_responder(
        &self,
        method: &Method,
        path: &str,
        json: Option<&Value>,
    ) -> Option<Responder> {
        let mut state = self.state();
        for rule in state.rules.iter_mut().rev() {
            if !rule.matches(method, path, json) {
                continue;
            }
            match &mut rule.script {
                Script::Always(r) => return Some(r.clone()),
                Script::Sequence(queue) => {
                    if let Some(r) = queue.pop_front() {
                        return Some(r);
                    }
                }
            }
        }
        None
    }

    fn sse_body(&self, script: Vec<SseScript>) -> Body {
        let script = VecDeque::from(script);
        let mut guard = DropGuard(Some(Arc::clone(&self.dropped_bodies)));
        if !has_pending_items(&script) {
            guard.disarm();
        }
        let stream =
            futures::stream::unfold((script, guard), |(mut script, mut guard)| async move {
                loop {
                    let chunk = match script.pop_front()? {
                        SseScript::Delay(d) => {
                            tokio::time::sleep(d).await;
                            continue;
                        }
                        SseScript::Hang => futures::future::pending::<String>().await,
                        SseScript::Gate(gate) => {
                            gate.notified().await;
                            continue;
                        }
                        SseScript::Raw(raw) => raw,
                        SseScript::Event(name, data) => format!("event: {name}\ndata: {data}\n\n"),
                        SseScript::Fail(message) => {
                            if !has_pending_items(&script) {
                                guard.disarm();
                            }
                            let err: oagw_sdk::body::BoxError = message.into();
                            return Some((Err(err), (script, guard)));
                        }
                    };
                    // The consumer got the last item: dropping the body now is not a
                    // cancellation, even if the end of the stream is never polled.
                    if !has_pending_items(&script) {
                        guard.disarm();
                    }
                    return Some((Ok(Bytes::from(chunk)), (script, guard)));
                }
            });
        let stream: BodyStream = Box::pin(stream);
        Body::Stream(stream)
    }
}

/// Whether the script still has an item a consumer would wait for (`Event`, `Raw` or `Hang`);
/// trailing `Delay`s and `Gate`s do not count.
fn has_pending_items(script: &VecDeque<SseScript>) -> bool {
    script
        .iter()
        .any(|item| !matches!(item, SseScript::Delay(_) | SseScript::Gate(_)))
}

/// Counts a drop while still armed (a body with items left, a request not yet answered).
struct DropGuard(Option<Arc<AtomicUsize>>);

impl DropGuard {
    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if let Some(counter) = self.0.take() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn uri_path(uri: &str) -> &str {
    uri.split_once('?').map_or(uri, |(path, _)| path)
}

fn response(
    status: u16,
    source: ErrorSource,
    content_type: &str,
    body: Body,
) -> http::Response<Body> {
    let mut resp = http::Response::new(body);
    *resp.status_mut() = StatusCode::from_u16(status).expect("valid scripted status");
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("content type"),
    );
    resp.extensions_mut().insert(source);
    resp
}

fn json_body(value: &Value) -> Body {
    Body::from(serde_json::to_vec(value).expect("serialize json"))
}

fn page<T: Clone>(items: &[T], query: &ListQuery) -> Vec<T> {
    items
        .iter()
        .skip(query.skip as usize)
        .take(query.top as usize)
        .cloned()
        .collect()
}

fn route_matches(route: &Route, method: &str, path: &str) -> bool {
    let Some(http) = route.match_rules.http.as_ref() else {
        return false;
    };
    let method_ok = http.methods.iter().any(|m| {
        let name = match m {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Patch => "PATCH",
        };
        name.eq_ignore_ascii_case(method)
    });
    method_ok && path.starts_with(http.path.as_str())
}

/// OAGW's alias conflict on `create_upstream`.
pub fn already_exists_upstream(alias: &str) -> CanonicalError {
    UpstreamResource::already_exists("Upstream already exists")
        .with_resource(alias)
        .create()
}

/// OAGW's route conflict on `create_route`.
pub fn already_exists_route() -> CanonicalError {
    RouteResource::already_exists("Route already exists")
        .with_resource("route")
        .create()
}

/// OAGW's "secret not readable (yet)": `FailedPrecondition(auth.config.secret_ref)`.
pub fn secret_not_readable() -> CanonicalError {
    UpstreamResource::failed_precondition()
        .with_precondition_violation("auth.config.secret_ref", "secret not found", "STATE")
        .create()
}

/// A deterministic OAGW validation failure.
pub fn validation_error() -> CanonicalError {
    UpstreamResource::invalid_argument()
        .with_field_violation("server", "bad endpoint", "INVALID")
        .create()
}

/// OAGW temporarily unavailable.
pub fn unavailable_error() -> CanonicalError {
    CanonicalError::service_unavailable()
        .with_retry_after_seconds(1)
        .create()
}

/// An internal OAGW failure (e.g. credstore down).
pub fn internal_error() -> CanonicalError {
    CanonicalError::internal("credstore down").create()
}

fn upstream_not_found(id: impl Into<String>) -> CanonicalError {
    UpstreamResource::not_found("Upstream not found")
        .with_resource(id)
        .create()
}

fn route_not_found(id: impl Into<String>) -> CanonicalError {
    RouteResource::not_found("Route not found")
        .with_resource(id)
        .create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGateway {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        {
            let mut state = self.state();
            state.create_upstream_calls += 1;
            if let Some(Err(err)) = state.create_upstream_script.pop_front() {
                return Err(err);
            }
        }
        let alias = req.alias().map_or_else(
            || {
                req.server()
                    .endpoints
                    .first()
                    .map(oagw_sdk::Endpoint::alias_contribution)
                    .unwrap_or_default()
            },
            str::to_owned,
        );
        if self
            .state()
            .upstreams
            .iter()
            .any(|u| u.alias.eq_ignore_ascii_case(&alias))
        {
            return Err(already_exists_upstream(&alias));
        }
        let upstream = Upstream {
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
        self.state().upstreams.push(upstream.clone());
        Ok(upstream)
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
            .ok_or_else(|| upstream_not_found(id.to_string()))
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(page(&self.state().upstreams, query))
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
        req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let mut state = self.state();
        let upstream = state
            .upstreams
            .iter_mut()
            .find(|u| u.id == id)
            .ok_or_else(|| upstream_not_found(id.to_string()))?;
        if let Some(alias) = req.alias() {
            alias.clone_into(&mut upstream.alias);
        }
        upstream.server = req.server().clone();
        req.protocol().clone_into(&mut upstream.protocol);
        upstream.enabled = req.enabled();
        upstream.auth = req.auth().cloned();
        upstream.headers = req.headers().cloned();
        upstream.plugins = req.plugins().cloned();
        upstream.rate_limit = req.rate_limit().cloned();
        upstream.cors = req.cors().cloned();
        upstream.tags = req.tags().to_vec();
        Ok(upstream.clone())
    }

    async fn delete_upstream(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        let mut state = self.state();
        let before = state.upstreams.len();
        state.upstreams.retain(|u| u.id != id);
        if state.upstreams.len() == before {
            return Err(upstream_not_found(id.to_string()));
        }
        state.routes.retain(|r| r.upstream_id != id);
        Ok(())
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let mut state = self.state();
        state.create_route_calls += 1;
        if let Some(Err(err)) = state.create_route_script.pop_front() {
            return Err(err);
        }
        if !state.upstreams.iter().any(|u| u.id == req.upstream_id()) {
            return Err(upstream_not_found(req.upstream_id().to_string()));
        }
        if let Some(new) = &req.match_rules().http {
            let overlaps = state.routes.iter().any(|r| {
                r.upstream_id == req.upstream_id()
                    && r.priority == req.priority()
                    && r.match_rules.http.as_ref().is_some_and(|h| {
                        h.path == new.path && h.methods.iter().any(|m| new.methods.contains(m))
                    })
            });
            if overlaps {
                return Err(already_exists_route());
            }
        }
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
        state.routes.push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.state()
            .routes
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| route_not_found(id.to_string()))
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        let routes: Vec<Route> = self
            .state()
            .routes
            .iter()
            .filter(|r| upstream_id.is_none_or(|id| r.upstream_id == id))
            .cloned()
            .collect();
        Ok(page(&routes, query))
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
        req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let mut state = self.state();
        let route = state
            .routes
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| route_not_found(id.to_string()))?;
        route.match_rules = req.match_rules().clone();
        route.plugins = req.plugins().cloned();
        route.rate_limit = req.rate_limit().cloned();
        route.cors = req.cors().cloned();
        route.tags = req.tags().to_vec();
        route.priority = req.priority();
        route.enabled = req.enabled();
        Ok(route.clone())
    }

    async fn delete_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        let mut state = self.state();
        let before = state.routes.len();
        state.routes.retain(|r| r.id != id);
        if state.routes.len() == before {
            return Err(route_not_found(id.to_string()));
        }
        Ok(())
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        let state = self.state();
        let upstream = state
            .upstreams
            .iter()
            .find(|u| u.alias == alias)
            .cloned()
            .ok_or_else(|| upstream_not_found(alias))?;
        let route = state
            .routes
            .iter()
            .find(|r| r.upstream_id == upstream.id && route_matches(r, method, path))
            .cloned()
            .ok_or_else(|| route_not_found(format!("{method} {path}")))?;
        Ok((upstream, route))
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body = body.into_bytes().await.map_err(|e| {
            CanonicalError::internal(format!("FakeGateway: request body: {e}")).create()
        })?;
        let uri = parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.to_string(), ToString::to_string);
        let path = parts.uri.path().to_owned();
        let json: Option<Value> = serde_json::from_slice(&body).ok();
        self.state().requests.push(RecordedRequest {
            method: parts.method.clone(),
            uri,
            headers: parts.headers,
            body: body.clone(),
            json: json.clone(),
        });

        let Some(mut responder) = self.next_responder(&parts.method, &path, json.as_ref()) else {
            let message = format!("FakeGateway: no responder for {} {path}", parts.method);
            return Ok(response(
                404,
                ErrorSource::Gateway,
                "application/problem+json",
                json_body(&serde_json::json!({"status": 404, "detail": message})),
            ));
        };
        // Armed while the answer is pending: dropping the future here is a dropped request.
        let mut pending = DropGuard(Some(Arc::clone(&self.dropped_requests)));
        while let Responder::Delayed(delay, inner) = responder {
            tokio::time::sleep(delay).await;
            responder = *inner;
        }
        if matches!(responder, Responder::Hang) {
            futures::future::pending::<()>().await;
        }
        pending.disarm();

        Ok(match responder {
            Responder::Json(status, value) => response(
                status,
                ErrorSource::Upstream,
                "application/json",
                json_body(&value),
            ),
            Responder::JsonWithHeaders(status, headers, value) => {
                let mut resp = response(
                    status,
                    ErrorSource::Upstream,
                    "application/json",
                    json_body(&value),
                );
                for (name, value) in headers {
                    resp.headers_mut().insert(
                        http::header::HeaderName::try_from(name).expect("header name"),
                        HeaderValue::try_from(value).expect("header value"),
                    );
                }
                resp
            }
            Responder::Sse(script) => response(
                200,
                ErrorSource::Upstream,
                "text/event-stream",
                self.sse_body(script),
            ),
            Responder::Err(err) => return Err(err),
            Responder::GatewayStatus(status) => response(
                status,
                ErrorSource::Gateway,
                "application/problem+json",
                json_body(&serde_json::json!({"status": status, "title": "gateway error"})),
            ),
            Responder::BodyError(status, message) => {
                let err: oagw_sdk::body::BoxError = message.into();
                let stream: BodyStream = Box::pin(futures::stream::once(async move { Err(err) }));
                response(
                    status,
                    ErrorSource::Upstream,
                    "application/json",
                    Body::Stream(stream),
                )
            }
            Responder::Delayed(..) | Responder::Hang => unreachable!("resolved above"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use futures::StreamExt;
    use http::Method;
    use oagw_sdk::api::ErrorSource;
    use oagw_sdk::body::BodyStream;
    use oagw_sdk::{
        Body, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
        HttpMethod, ListQuery, MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1,
        UpdateRouteRequest, UpdateUpstreamRequest,
    };
    use serde_json::json;
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::*;

    fn s2s() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("ctx")
    }

    fn request(method: &str, uri: &str, body: Body) -> http::Request<Body> {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body)
            .expect("request")
    }

    async fn body_text(resp: http::Response<Body>) -> String {
        let bytes = resp.into_body().into_bytes().await.expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn body_matching_rules_select_by_request_body() {
        let gw = FakeGateway::new();
        gw.on(
            Method::POST,
            "/v1/responses",
            Responder::json(200, json!({"r": "any"})),
        );
        let non_streaming: BodyMatcher = Arc::new(|b: &Value| b["stream"] == false);
        gw.on_sequence_matching(
            Method::POST,
            "/v1/responses",
            non_streaming,
            vec![Responder::json(200, json!({"r": "summary"}))],
        );
        let send = |body: &'static str| {
            let gw = &gw;
            async move {
                let resp = gw
                    .proxy_request(s2s(), request("POST", "/a/v1/responses", Body::from(body)))
                    .await
                    .expect("response");
                body_text(resp).await
            }
        };
        assert_eq!(send(r#"{"stream":true}"#).await, r#"{"r":"any"}"#);
        assert_eq!(
            send("not json").await,
            r#"{"r":"any"}"#,
            "non-JSON never matches"
        );
        assert_eq!(send(r#"{"stream":false}"#).await, r#"{"r":"summary"}"#);
        assert_eq!(
            send(r#"{"stream":false}"#).await,
            r#"{"r":"any"}"#,
            "an exhausted matching sequence falls back to earlier rules"
        );
    }

    #[tokio::test]
    async fn json_responder_records_request_and_marks_upstream() {
        let gw = FakeGateway::new();
        gw.on(
            Method::POST,
            "/v1/responses",
            Responder::json(200, json!({"id": "r1"})),
        );

        let sent = json!({"model": "m", "stream": false});
        let resp = gw
            .proxy_request(
                s2s(),
                request(
                    "POST",
                    "/127.0.0.1/v1/responses?x=1",
                    Body::from(serde_json::to_vec(&sent).unwrap()),
                ),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.extensions().get::<ErrorSource>(),
            Some(&ErrorSource::Upstream)
        );
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert_eq!(body_text(resp).await, r#"{"id":"r1"}"#);

        let recorded = gw.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, Method::POST);
        assert_eq!(recorded[0].uri, "/127.0.0.1/v1/responses?x=1");
        assert_eq!(recorded[0].headers["content-type"], "application/json");
        assert_eq!(recorded[0].json, Some(sent));
        assert_eq!(gw.requests_to(&Method::POST, "/v1/responses").len(), 1);
        assert!(gw.requests_to(&Method::GET, "/v1/responses").is_empty());
    }

    #[tokio::test]
    async fn streamed_request_body_is_buffered_and_non_json_has_no_json() {
        let gw = FakeGateway::new();
        gw.on(Method::POST, "/v1/files", Responder::json(200, json!({})));
        let chunks: oagw_sdk::body::BodyStream = Box::pin(futures::stream::iter(vec![
            Ok(Bytes::from_static(b"--b\r\n")),
            Ok(Bytes::from_static(b"payload")),
        ]));
        gw.proxy_request(s2s(), request("POST", "/a/v1/files", Body::Stream(chunks)))
            .await
            .expect("response");
        let recorded = &gw.requests()[0];
        assert_eq!(recorded.body, Bytes::from_static(b"--b\r\npayload"));
        assert_eq!(recorded.json, None);
    }

    #[tokio::test]
    async fn headers_gateway_status_and_canonical_errors() {
        let gw = FakeGateway::new();
        gw.on(
            Method::POST,
            "/limited",
            Responder::json_with_headers(
                429,
                &[("retry-after", "7")],
                json!({"error": {"message": "slow down"}}),
            ),
        );
        gw.on(Method::POST, "/timeout", Responder::GatewayStatus(504));
        gw.on(
            Method::POST,
            "/broken",
            Responder::Err(CanonicalError::internal("boom").create()),
        );

        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/limited", Body::Empty))
            .await
            .unwrap();
        assert_eq!(resp.status(), 429);
        assert_eq!(resp.headers()["retry-after"], "7");
        assert_eq!(
            resp.extensions().get::<ErrorSource>(),
            Some(&ErrorSource::Upstream)
        );

        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/timeout", Body::Empty))
            .await
            .unwrap();
        assert_eq!(resp.status(), 504);
        assert_eq!(
            resp.extensions().get::<ErrorSource>(),
            Some(&ErrorSource::Gateway)
        );

        let err = gw
            .proxy_request(s2s(), request("POST", "/a/broken", Body::Empty))
            .await
            .expect_err("scripted error");
        assert!(matches!(err, CanonicalError::Internal { .. }), "{err:?}");
        assert_eq!(gw.requests().len(), 3, "errors are recorded too");
    }

    #[tokio::test]
    async fn sequence_is_consumed_in_order_then_falls_through() {
        let gw = FakeGateway::new();
        // The fallback is registered first: later rules are tried first.
        gw.on(
            Method::DELETE,
            "/v1/files/",
            Responder::json(404, json!({"n": 3})),
        );
        gw.on_sequence(
            Method::DELETE,
            "/v1/files/",
            vec![
                Responder::json(500, json!({"n": 1})),
                Responder::json(200, json!({"n": 2})),
            ],
        );

        let mut statuses = Vec::new();
        for _ in 0..4 {
            let resp = gw
                .proxy_request(s2s(), request("DELETE", "/a/v1/files/f1", Body::Empty))
                .await
                .unwrap();
            statuses.push(resp.status().as_u16());
        }
        assert_eq!(statuses, [500, 200, 404, 404]);
    }

    #[tokio::test]
    async fn unmatched_request_is_a_recorded_gateway_404() {
        let gw = FakeGateway::new();
        gw.on(Method::GET, "/v1/files", Responder::json(200, json!({})));
        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/v1/files", Body::Empty))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(
            resp.extensions().get::<ErrorSource>(),
            Some(&ErrorSource::Gateway)
        );
        assert!(body_text(resp).await.contains("POST /a/v1/files"));
        assert_eq!(gw.requests().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn sse_responder_streams_script_and_honours_delay() {
        let gw = FakeGateway::new();
        gw.on(
            Method::POST,
            "/v1/responses",
            Responder::Sse(vec![
                SseScript::event("response.created", json!({"id": 1})),
                SseScript::Delay(Duration::from_secs(3)),
                SseScript::Raw("data: [DONE]\n\n".to_owned()),
            ]),
        );
        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/v1/responses", Body::Empty))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        assert_eq!(
            resp.extensions().get::<ErrorSource>(),
            Some(&ErrorSource::Upstream)
        );
        let mut stream = match resp.into_body() {
            Body::Stream(s) => s,
            other => panic!("expected a stream body, got {other:?}"),
        };

        let started = tokio::time::Instant::now();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(
            first,
            Bytes::from_static(b"event: response.created\ndata: {\"id\":1}\n\n")
        );
        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(second, Bytes::from_static(b"data: [DONE]\n\n"));
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "delay honoured"
        );
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(gw.dropped_bodies(), 0, "a finished body is not a drop");
    }

    #[tokio::test(start_paused = true)]
    async fn sse_hang_keeps_body_open_and_drop_is_recorded() {
        let gw = FakeGateway::new();
        gw.on(
            Method::POST,
            "/v1/responses",
            Responder::Sse(vec![
                SseScript::event("response.output_text.delta", json!({"delta": "hi"})),
                SseScript::Hang,
            ]),
        );
        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/v1/responses", Body::Empty))
            .await
            .unwrap();
        let mut stream = resp.into_body().into_stream();
        assert!(stream.next().await.is_some());
        let pending = tokio::time::timeout(Duration::from_secs(3600), stream.next()).await;
        assert!(pending.is_err(), "Hang never yields");
        assert_eq!(gw.dropped_bodies(), 0);
        drop(stream);
        assert_eq!(gw.dropped_bodies(), 1);
    }

    async fn read_chunks(gw: &FakeGateway, script: Vec<SseScript>, n: usize) -> BodyStream {
        gw.clear_rules();
        gw.on(Method::POST, "/v1/responses", Responder::Sse(script));
        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/v1/responses", Body::Empty))
            .await
            .unwrap();
        let mut stream = resp.into_body().into_stream();
        for _ in 0..n {
            stream.next().await.unwrap().unwrap();
        }
        stream
    }

    #[tokio::test(start_paused = true)]
    async fn body_is_dropped_only_while_script_items_remain() {
        let gw = FakeGateway::new();
        // Every item read, end never polled (an adapter stopping at its terminal event).
        let script = vec![
            SseScript::event("response.created", json!({})),
            SseScript::Raw("data: [DONE]\n\n".to_owned()),
            SseScript::Delay(Duration::from_secs(1)),
        ];
        drop(read_chunks(&gw, script, 2).await);
        assert_eq!(
            gw.dropped_bodies(),
            0,
            "terminal item read: not a cancellation"
        );

        // Dropped during a Delay with an item left.
        let script = vec![
            SseScript::event("a", json!({})),
            SseScript::Delay(Duration::from_secs(1)),
            SseScript::event("b", json!({})),
        ];
        drop(read_chunks(&gw, script, 1).await);
        assert_eq!(gw.dropped_bodies(), 1);

        // Dropped during Hang.
        let mut stream = read_chunks(
            &gw,
            vec![SseScript::event("a", json!({})), SseScript::Hang],
            1,
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_secs(60), stream.next())
                .await
                .is_err()
        );
        drop(stream);
        assert_eq!(gw.dropped_bodies(), 2);

        // Never read at all.
        drop(read_chunks(&gw, vec![SseScript::event("a", json!({}))], 0).await);
        assert_eq!(gw.dropped_bodies(), 3);
    }

    async fn get_x_status(gw: &FakeGateway) -> u16 {
        gw.proxy_request(s2s(), request("GET", "/a/x", Body::Empty))
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn later_rules_override_and_rules_and_requests_can_be_cleared() {
        let gw = FakeGateway::new();
        gw.on(Method::GET, "/x", Responder::json(200, json!({})));
        gw.on(Method::GET, "/x", Responder::json(500, json!({})));
        assert_eq!(get_x_status(&gw).await, 500, "the later rule wins");
        assert_eq!(gw.requests().len(), 1);

        gw.clear_requests();
        assert!(gw.requests().is_empty());
        gw.clear_rules();
        assert_eq!(get_x_status(&gw).await, 404, "no rules left");
        gw.on(Method::GET, "/x", Responder::json(201, json!({})));
        assert_eq!(get_x_status(&gw).await, 201);
        assert_eq!(gw.requests().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_and_hanging_responses() {
        let gw = FakeGateway::new();
        gw.on(
            Method::POST,
            "/slow",
            Responder::Delayed(
                Duration::from_secs(40),
                Box::new(Responder::GatewayStatus(504)),
            ),
        );
        gw.on(Method::POST, "/hang", Responder::Hang);

        let started = tokio::time::Instant::now();
        let resp = gw
            .proxy_request(s2s(), request("POST", "/a/slow", Body::Empty))
            .await
            .unwrap();
        assert_eq!(resp.status(), 504);
        assert!(started.elapsed() >= Duration::from_secs(40));
        assert_eq!(gw.dropped_requests(), 0, "answered requests are not drops");

        let hang = tokio::time::timeout(
            Duration::from_secs(3600),
            gw.proxy_request(s2s(), request("POST", "/a/hang", Body::Empty)),
        )
        .await;
        assert!(hang.is_err(), "Hang never answers");
        assert_eq!(gw.dropped_requests(), 1);

        // Dropped while delayed counts too.
        let slow = tokio::time::timeout(
            Duration::from_secs(1),
            gw.proxy_request(s2s(), request("POST", "/a/slow", Body::Empty)),
        )
        .await;
        assert!(slow.is_err());
        assert_eq!(gw.dropped_requests(), 2);
        assert_eq!(gw.requests().len(), 3, "all three were recorded");
    }

    fn server(host: &str, port: u16) -> Server {
        Server {
            endpoints: vec![Endpoint {
                scheme: Scheme::Http,
                host: host.to_owned(),
                port,
            }],
        }
    }

    fn http_rules(path: &str, methods: Vec<HttpMethod>) -> MatchRules {
        MatchRules {
            http: Some(HttpMatch {
                methods,
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        }
    }

    #[tokio::test]
    async fn provisioning_crud_is_kept_in_memory() {
        let gw = FakeGateway::new();
        let ctx = s2s();

        let up = gw
            .create_upstream(
                ctx.clone(),
                CreateUpstreamRequest::builder(server("127.0.0.1", 9), HTTP_PROTOCOL_ID)
                    .alias("openai-local")
                    .tags(vec!["mini-chat".to_owned()])
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(up.alias, "openai-local");
        assert_eq!(up.tenant_id, ctx.subject_tenant_id());
        assert_eq!(up.tags, ["mini-chat"]);
        let defaulted = gw
            .create_upstream(
                ctx.clone(),
                CreateUpstreamRequest::builder(server("example.com", 8443), HTTP_PROTOCOL_ID)
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(defaulted.alias, "example.com:8443");

        assert_eq!(gw.get_upstream(ctx.clone(), up.id).await.unwrap(), up);
        assert_eq!(
            gw.list_upstreams(ctx.clone(), &ListQuery { top: 1, skip: 1 })
                .await
                .unwrap(),
            std::slice::from_ref(&defaulted)
        );
        let updated = gw
            .update_upstream(
                ctx.clone(),
                up.id,
                UpdateUpstreamRequest::builder(server("127.0.0.1", 10), HTTP_PROTOCOL_ID)
                    .alias("openai-local")
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(updated.server.endpoints[0].port, 10);
        assert_eq!(updated.id, up.id);

        let route = gw
            .create_route(
                ctx.clone(),
                CreateRouteRequest::builder(
                    up.id,
                    http_rules("/v1/responses", vec![HttpMethod::Post]),
                )
                .build(),
            )
            .await
            .unwrap();
        assert_eq!(route.upstream_id, up.id);
        assert_eq!(gw.get_route(ctx.clone(), route.id).await.unwrap(), route);
        assert_eq!(
            gw.list_routes(ctx.clone(), Some(up.id), &ListQuery::default())
                .await
                .unwrap(),
            std::slice::from_ref(&route)
        );
        assert!(
            gw.list_routes(ctx.clone(), Some(defaulted.id), &ListQuery::default())
                .await
                .unwrap()
                .is_empty()
        );
        let (target_up, target_route) = gw
            .resolve_proxy_target(ctx.clone(), "openai-local", "POST", "/v1/responses/x")
            .await
            .unwrap();
        assert_eq!((target_up.id, target_route.id), (up.id, route.id));
        assert!(
            gw.resolve_proxy_target(ctx.clone(), "openai-local", "GET", "/v1/responses")
                .await
                .is_err()
        );

        let route = gw
            .update_route(
                ctx.clone(),
                route.id,
                UpdateRouteRequest::builder(http_rules(
                    "/v1/files",
                    vec![HttpMethod::Post, HttpMethod::Delete],
                ))
                .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            route.match_rules.http.as_ref().unwrap().path,
            "/v1/files",
            "route replaced"
        );
        assert_eq!(gw.routes(), std::slice::from_ref(&route));

        gw.delete_route(ctx.clone(), route.id).await.unwrap();
        assert!(gw.get_route(ctx.clone(), route.id).await.is_err());
        gw.delete_upstream(ctx.clone(), up.id).await.unwrap();
        assert!(gw.get_upstream(ctx.clone(), up.id).await.is_err());
        assert_eq!(gw.upstreams(), [defaulted]);
    }
}
