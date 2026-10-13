//! In-memory [`ServiceGatewayClientV1`] for unit and integration tests.
//!
//! - Proxy: scripted replies are queued per path prefix (`push_*`) and each
//!   is consumed by the first matching request. A prefix matches the full
//!   proxy path (`/{alias}/...`) or the path after the alias segment.
//!   Requests no one-shot reply matches get the sticky fallback of their
//!   prefix ([`FakeOagw::set_fallback_json`]), else a 404 JSON body. Every
//!   request is recorded.
//! - Upstream / route CRUD: stored in memory and succeed, with the OAGW
//!   behaviours provisioning depends on: OAGW's alias rule (a hostname
//!   endpoint's alias is derived and an explicit alias must equal it; an IP
//!   endpoint needs an explicit alias), a duplicate alias or an overlapping
//!   route is `already_exists`, updates are full replacements that cannot
//!   rename the alias, and an unavailable `secret_ref` is
//!   `failed_precondition` (see [`FakeOagw::set_secret_unavailable`]).

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::{Body, BodyStream, BoxError};
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, HttpMethod, ListQuery, Route, Scheme, Server,
    ServiceGatewayClientV1, UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct FakeUpstreamError;

#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
struct FakeRouteError;

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct FakeProxyError;

/// The canonical error OAGW returns for a gateway timeout (HTTP 504
/// `deadline_exceeded`).
#[must_use]
pub fn gateway_timeout_error() -> CanonicalError {
    FakeProxyError::deadline_exceeded("upstream request timed out").create()
}

/// A transient OAGW failure (`service_unavailable`).
#[must_use]
pub fn unavailable_error() -> CanonicalError {
    CanonicalError::service_unavailable().create()
}

/// A deterministic validation failure (`invalid_argument`).
#[must_use]
pub fn invalid_argument_error(detail: &str) -> CanonicalError {
    FakeUpstreamError::invalid_argument()
        .with_field_violation("alias", detail, "INVALID")
        .create()
}

/// Multipart part `(name, filename, content_type)`.
pub type MultipartField = (String, Option<String>, Option<String>);

/// One proxied request as seen by the fake.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    pub method: String,
    /// Proxy URI (`/{alias}{path}?query`).
    pub uri: String,
    /// Body parsed as JSON, when it is JSON.
    pub json_body: Option<Value>,
    /// Multipart parts `(name, filename, content_type)`.
    pub multipart_fields: Vec<MultipartField>,
    /// Raw request body.
    pub raw_body: Bytes,
    /// Request headers `(name, value)` (lowercase names).
    pub headers: Vec<(String, String)>,
}

/// One step of a scripted SSE body ([`FakeOagw::push_sse_script`]).
#[derive(Debug, Clone)]
pub enum SseStep {
    /// Send one `(event, data)` frame (an empty event name omits `event:`).
    Frame(String, Value),
    /// Pause the body.
    Sleep(Duration),
    /// Hold the body open until the test calls `notify_one()` on the handle.
    Wait(Arc<tokio::sync::Notify>),
}

impl SseStep {
    /// `Frame` from borrowed parts.
    #[must_use]
    pub fn frame(event: &str, data: Value) -> Self {
        Self::Frame(event.to_owned(), data)
    }
}

enum Reply {
    Sse {
        events: Vec<(String, Value)>,
        delay: Option<Duration>,
    },
    Script(Vec<SseStep>),
    Json {
        status: u16,
        headers: Vec<(String, String)>,
        body: Value,
        source: ErrorSource,
    },
    Error(CanonicalError),
}

struct Scripted {
    prefix: String,
    /// Only requests with this method match (`None`: any method).
    method: Option<String>,
    reply: Reply,
}

#[derive(Default)]
struct State {
    scripted: Vec<Scripted>,
    requests: Vec<RecordedRequest>,
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    unavailable_secrets: HashSet<String>,
    upstream_failures: HashMap<String, CanonicalError>,
    /// One-shot `create_route` failures by route path.
    route_failures: Vec<(String, CanonicalError)>,
    /// Sticky JSON replies `(prefix, status, body)` used when no one-shot
    /// reply matches.
    fallbacks: Vec<(String, u16, Value)>,
    /// Successful `update_upstream` calls.
    upstream_updates: usize,
}

impl State {
    /// `failed_precondition` when the auth `secret_ref` is marked unavailable.
    fn check_secret(&self, auth: Option<&oagw_sdk::AuthConfig>) -> Result<(), CanonicalError> {
        match auth
            .and_then(|a| a.config.as_ref())
            .and_then(|c| c.get("secret_ref"))
        {
            Some(secret) if self.unavailable_secrets.contains(secret) => {
                Err(FakeUpstreamError::failed_precondition()
                    .with_precondition_violation(
                        "auth.config.secret_ref",
                        format!("secret_ref '{secret}' is not accessible to this tenant"),
                        "STATE",
                    )
                    .create())
            }
            _ => Ok(()),
        }
    }
}

/// In-memory OAGW (see the module docs).
#[derive(Default)]
pub struct FakeOagw {
    state: Mutex<State>,
    open_streams: Arc<AtomicUsize>,
}

impl FakeOagw {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, prefix: &str, reply: Reply) {
        self.lock().scripted.push(Scripted {
            prefix: prefix.to_owned(),
            method: None,
            reply,
        });
    }

    /// 200 `text/event-stream` with the given `(event, data)` frames; an
    /// empty event name omits the `event:` line.
    pub fn push_sse(&self, path_prefix: &str, events: Vec<(&str, Value)>) {
        self.push(
            path_prefix,
            Reply::Sse {
                events: to_owned_events(events),
                delay: None,
            },
        );
    }

    /// Like [`push_sse`](Self::push_sse), but waits `delay` after every frame
    /// (the stream stays open `delay` after the last one).
    pub fn push_sse_slow(&self, path_prefix: &str, events: Vec<(&str, Value)>, delay: Duration) {
        self.push(
            path_prefix,
            Reply::Sse {
                events: to_owned_events(events),
                delay: Some(delay),
            },
        );
    }

    /// 200 `text/event-stream` whose body follows `steps` (frames, pauses,
    /// waits on a test-controlled [`tokio::sync::Notify`]).
    pub fn push_sse_script(&self, path_prefix: &str, steps: Vec<SseStep>) {
        self.push(path_prefix, Reply::Script(steps));
    }

    /// Upstream JSON response with `status`.
    pub fn push_json(&self, path_prefix: &str, status: u16, json: Value) {
        self.push_json_with_headers(path_prefix, status, &[], json);
    }

    /// Like [`push_json`](Self::push_json), but only `method` requests match.
    pub fn push_json_for(&self, method: &str, path_prefix: &str, status: u16, json: Value) {
        self.lock().scripted.push(Scripted {
            prefix: path_prefix.to_owned(),
            method: Some(method.to_owned()),
            reply: Reply::Json {
                status,
                headers: Vec::new(),
                body: json,
                source: ErrorSource::Upstream,
            },
        });
    }

    /// Upstream JSON response with extra headers (e.g. `retry-after`).
    pub fn push_json_with_headers(
        &self,
        path_prefix: &str,
        status: u16,
        headers: &[(&str, &str)],
        json: Value,
    ) {
        self.push(
            path_prefix,
            Reply::Json {
                status,
                headers: headers
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                body: json,
                source: ErrorSource::Upstream,
            },
        );
    }

    /// Gateway-originated error response (`ErrorSource::Gateway`, Problem body).
    pub fn push_gateway_response(&self, path_prefix: &str, status: u16, problem: Value) {
        self.push(
            path_prefix,
            Reply::Json {
                status,
                headers: vec![(
                    "content-type".to_owned(),
                    "application/problem+json".to_owned(),
                )],
                body: problem,
                source: ErrorSource::Gateway,
            },
        );
    }

    /// Sticky upstream JSON reply for requests matching `path_prefix` that no
    /// one-shot reply matches (replaces an earlier fallback of the same
    /// prefix). One-shot replies always win.
    pub fn set_fallback_json(&self, path_prefix: &str, status: u16, json: Value) {
        let mut st = self.lock();
        st.fallbacks.retain(|(p, _, _)| p != path_prefix);
        st.fallbacks.push((path_prefix.to_owned(), status, json));
    }

    /// Remove the fallback of `path_prefix`.
    pub fn clear_fallback(&self, path_prefix: &str) {
        self.lock().fallbacks.retain(|(p, _, _)| p != path_prefix);
    }

    /// `proxy_request` fails with `err` (gateway-side error).
    pub fn push_error(&self, path_prefix: &str, err: CanonicalError) {
        self.push(path_prefix, Reply::Error(err));
    }

    /// Every proxied request so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().requests.clone()
    }

    /// Number of SSE response bodies not yet finished or dropped.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.open_streams.load(Ordering::SeqCst)
    }

    /// Created upstreams.
    #[must_use]
    pub fn upstreams(&self) -> Vec<Upstream> {
        self.lock().upstreams.clone()
    }

    /// Created routes.
    #[must_use]
    pub fn routes(&self) -> Vec<Route> {
        self.lock().routes.clone()
    }

    /// Number of successful `update_upstream` calls.
    #[must_use]
    pub fn upstream_updates(&self) -> usize {
        self.lock().upstream_updates
    }

    /// Routes of the upstream registered under `alias`.
    #[must_use]
    pub fn routes_for_alias(&self, alias: &str) -> Vec<Route> {
        let st = self.lock();
        let Some(up) = st.upstreams.iter().find(|u| u.alias == alias) else {
            return Vec::new();
        };
        st.routes
            .iter()
            .filter(|r| r.upstream_id == up.id)
            .cloned()
            .collect()
    }

    /// While set, creating an upstream whose auth `secret_ref` is `secret_ref`
    /// fails with `failed_precondition` (credstore secret not readable yet).
    pub fn set_secret_unavailable(&self, secret_ref: &str, unavailable: bool) {
        let mut st = self.lock();
        if unavailable {
            st.unavailable_secrets.insert(secret_ref.to_owned());
        } else {
            st.unavailable_secrets.remove(secret_ref);
        }
    }

    /// The next `create_route` for `path` fails with `err` (once).
    pub fn fail_create_route_once(&self, path: &str, err: CanonicalError) {
        self.lock().route_failures.push((path.to_owned(), err));
    }

    /// Creating an upstream under `alias` fails with `err`.
    pub fn fail_create_upstream(&self, alias: &str, err: CanonicalError) {
        self.lock().upstream_failures.insert(alias.to_owned(), err);
    }

    fn take_reply(&self, method: &str, path: &str) -> Option<Reply> {
        let after_alias = path
            .strip_prefix('/')
            .and_then(|p| p.find('/').map(|i| &p[i..]))
            .unwrap_or("");
        let matches = |prefix: &str| path.starts_with(prefix) || after_alias.starts_with(prefix);
        let mut st = self.lock();
        if let Some(idx) = st
            .scripted
            .iter()
            .position(|s| matches(&s.prefix) && s.method.as_deref().is_none_or(|m| m == method))
        {
            return Some(st.scripted.remove(idx).reply);
        }
        st.fallbacks
            .iter()
            .find(|(prefix, _, _)| matches(prefix))
            .map(|(_, status, body)| Reply::Json {
                status: *status,
                headers: Vec::new(),
                body: body.clone(),
                source: ErrorSource::Upstream,
            })
    }

    fn open_guard(&self) -> OpenGuard {
        self.open_streams.fetch_add(1, Ordering::SeqCst);
        OpenGuard(Arc::clone(&self.open_streams))
    }

    fn script_body(&self, steps: Vec<SseStep>) -> BodyStream {
        let guard = self.open_guard();
        let stream = futures::stream::unfold(
            (steps.into_iter(), Some(guard)),
            |(mut steps, guard)| async move {
                let guard = guard?;
                loop {
                    match steps.next() {
                        Some(SseStep::Frame(event, data)) => {
                            let frame = Ok::<Bytes, BoxError>(sse_frame(&event, &data));
                            return Some((frame, (steps, Some(guard))));
                        }
                        Some(SseStep::Sleep(d)) => tokio::time::sleep(d).await,
                        Some(SseStep::Wait(n)) => n.notified().await,
                        None => {
                            drop(guard);
                            return None;
                        }
                    }
                }
            },
        );
        Box::pin(stream)
    }

    fn sse_body(&self, events: Vec<(String, Value)>, delay: Option<Duration>) -> BodyStream {
        let guard = self.open_guard();
        let frames = events
            .into_iter()
            .map(|(event, data)| sse_frame(&event, &data));
        let stream = futures::stream::unfold(
            (frames, Some(guard), false),
            move |(mut frames, guard, mut first)| async move {
                let guard = guard?;
                if let Some(delay) = delay
                    && first
                {
                    tokio::time::sleep(delay).await;
                }
                first = true;
                if let Some(frame) = frames.next() {
                    Some((Ok::<Bytes, BoxError>(frame), (frames, Some(guard), first)))
                } else {
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    drop(guard);
                    None
                }
            },
        );
        Box::pin(stream)
    }
}

/// Decrements the open-stream counter when an SSE body ends or is dropped.
struct OpenGuard(Arc<AtomicUsize>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn sse_frame(event: &str, data: &Value) -> Bytes {
    let event_line = if event.is_empty() {
        String::new()
    } else {
        format!("event: {event}\n")
    };
    Bytes::from(format!("{event_line}data: {data}\n\n"))
}

fn to_owned_events(events: Vec<(&str, Value)>) -> Vec<(String, Value)> {
    events.into_iter().map(|(e, v)| (e.to_owned(), v)).collect()
}

fn single_chunk(bytes: Bytes) -> Body {
    Body::Stream(Box::pin(futures::stream::once(async move {
        Ok::<Bytes, BoxError>(bytes)
    })))
}

async fn multipart_fields(content_type: &str, body: Bytes) -> Vec<MultipartField> {
    let Ok(boundary) = multer::parse_boundary(content_type) else {
        return Vec::new();
    };
    let stream = futures::stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
    let mut mp = multer::Multipart::new(stream, boundary);
    let mut out = Vec::new();
    while let Ok(Some(field)) = mp.next_field().await {
        out.push((
            field.name().unwrap_or_default().to_owned(),
            field.file_name().map(ToOwned::to_owned),
            field.content_type().map(ToString::to_string),
        ));
        // Drain the part so the next one can be read.
        if field.bytes().await.is_err() {
            break;
        }
    }
    out
}

fn normalize_alias(alias: &str) -> String {
    alias.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// OAGW's alias rule for a single-endpoint upstream (oagw
/// `management/alias.rs`): a hostname endpoint derives its alias (normalized
/// host, `:port` unless it is the scheme's standard port) and an explicit
/// alias must equal it; an IP endpoint requires an explicit alias.
fn resolve_alias(server: &Server, requested: Option<&str>) -> Result<String, CanonicalError> {
    let invalid = |detail: String| {
        FakeUpstreamError::invalid_argument()
            .with_field_violation("alias", detail, "INVALID")
            .create()
    };
    let Some(ep) = server.endpoints.first() else {
        return Err(invalid("server must have at least one endpoint".to_owned()));
    };
    let host = ep
        .host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&ep.host);
    let host = normalize_alias(host);
    let standard = match ep.scheme {
        Scheme::Http => ep.port == 80,
        _ => ep.port == 443,
    };
    let derived = if standard {
        host.clone()
    } else {
        format!("{host}:{}", ep.port)
    };
    match (
        host.parse::<IpAddr>().is_ok(),
        requested.map(normalize_alias),
    ) {
        (false, None) => Ok(derived),
        (false, Some(a)) if a == derived => Ok(derived),
        (false, Some(_)) => Err(invalid(format!(
            "alias is auto-derived for hostname-based endpoints; remove the 'alias' field (derived: '{derived}')"
        ))),
        (true, Some(a)) => Ok(a),
        (true, None) => Err(invalid(
            "explicit alias is required for IP-based or heterogeneous-host endpoints".to_owned(),
        )),
    }
}

fn page<T: Clone>(items: impl Iterator<Item = T>, query: &ListQuery) -> Vec<T> {
    items
        .skip(query.skip as usize)
        .take(query.top as usize)
        .collect()
}

fn method_str(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Patch => "PATCH",
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let alias = resolve_alias(req.server(), req.alias())?;
        let mut st = self.lock();
        if let Some(err) = st.upstream_failures.get(&alias) {
            return Err(err.clone());
        }
        st.check_secret(req.auth())?;
        if st.upstreams.iter().any(|u| u.alias == alias) {
            return Err(FakeUpstreamError::already_exists("alias already exists")
                .with_resource(alias)
                .create());
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
        st.upstreams.push(upstream.clone());
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.lock()
            .upstreams
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .ok_or_else(|| {
                FakeUpstreamError::not_found("upstream not found")
                    .with_resource(id.to_string())
                    .create()
            })
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(page(self.lock().upstreams.iter().cloned(), query))
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
        req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let mut st = self.lock();
        st.check_secret(req.auth())?;
        let Some(u) = st.upstreams.iter_mut().find(|u| u.id == id) else {
            return Err(FakeUpstreamError::not_found("upstream not found")
                .with_resource(id.to_string())
                .create());
        };
        if let Some(alias) = req.alias()
            && normalize_alias(alias) != u.alias
        {
            return Err(FakeUpstreamError::invalid_argument()
                .with_field_violation("alias", "alias cannot be changed", "INVALID")
                .create());
        }
        // Full replacement (like OAGW).
        u.server = req.server().clone();
        req.protocol().clone_into(&mut u.protocol);
        u.auth = req.auth().cloned();
        u.headers = req.headers().cloned();
        u.plugins = req.plugins().cloned();
        u.rate_limit = req.rate_limit().cloned();
        u.cors = req.cors().cloned();
        u.tags = req.tags().to_vec();
        u.enabled = req.enabled();
        let updated = u.clone();
        st.upstream_updates += 1;
        Ok(updated)
    }

    async fn delete_upstream(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        let mut st = self.lock();
        st.upstreams.retain(|u| u.id != id);
        st.routes.retain(|r| r.upstream_id != id);
        Ok(())
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let mut st = self.lock();
        let path = req.match_rules().http.as_ref().map(|h| h.path.clone());
        if let Some(idx) = st
            .route_failures
            .iter()
            .position(|(p, _)| Some(p) == path.as_ref())
        {
            return Err(st.route_failures.remove(idx).1);
        }
        if !st.upstreams.iter().any(|u| u.id == req.upstream_id()) {
            return Err(FakeRouteError::invalid_argument()
                .with_field_violation(
                    "upstream_id",
                    "upstream not found for this tenant",
                    "INVALID",
                )
                .create());
        }
        let overlaps = |r: &Route| {
            let (Some(a), Some(b)) = (&r.match_rules.http, &req.match_rules().http) else {
                return false;
            };
            r.upstream_id == req.upstream_id()
                && r.enabled
                && r.priority == req.priority()
                && a.path == b.path
                && a.methods.iter().any(|m| b.methods.contains(m))
        };
        if req.enabled() && st.routes.iter().any(overlaps) {
            return Err(FakeRouteError::already_exists("route overlap")
                .with_resource(req.upstream_id().to_string())
                .create());
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
        st.routes.push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.lock()
            .routes
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| {
                FakeRouteError::not_found("route not found")
                    .with_resource(id.to_string())
                    .create()
            })
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        let st = self.lock();
        let items = st
            .routes
            .iter()
            .filter(|r| upstream_id.is_none_or(|u| r.upstream_id == u))
            .cloned();
        Ok(page(items, query))
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
        req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let mut st = self.lock();
        let Some(r) = st.routes.iter_mut().find(|r| r.id == id) else {
            return Err(FakeRouteError::not_found("route not found")
                .with_resource(id.to_string())
                .create());
        };
        r.match_rules = req.match_rules().clone();
        r.priority = req.priority();
        r.enabled = req.enabled();
        Ok(r.clone())
    }

    async fn delete_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.lock().routes.retain(|r| r.id != id);
        Ok(())
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        let st = self.lock();
        let not_found = || {
            FakeProxyError::not_found("no matching route")
                .with_resource(alias.to_owned())
                .create()
        };
        let upstream = st
            .upstreams
            .iter()
            .find(|u| u.alias == alias)
            .ok_or_else(not_found)?;
        let route = st
            .routes
            .iter()
            .filter(|r| r.upstream_id == upstream.id)
            .filter_map(|r| r.match_rules.http.as_ref().map(|h| (r, h)))
            .filter(|(_, h)| {
                path.starts_with(&h.path)
                    && h.methods
                        .iter()
                        .any(|m| method_str(*m).eq_ignore_ascii_case(method))
            })
            .max_by_key(|(_, h)| h.path.len())
            .map(|(r, _)| r.clone())
            .ok_or_else(not_found)?;
        Ok((upstream.clone(), route))
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let uri = parts.uri.to_string();
        let path = parts.uri.path().to_owned();
        let content_type = parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let headers = parts
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    v.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let bytes = body.into_bytes().await.unwrap_or_default();
        let multipart = if content_type.starts_with("multipart/form-data") {
            multipart_fields(&content_type, bytes.clone()).await
        } else {
            Vec::new()
        };
        self.lock().requests.push(RecordedRequest {
            method: parts.method.to_string(),
            uri,
            json_body: serde_json::from_slice(&bytes).ok(),
            multipart_fields: multipart,
            raw_body: bytes,
            headers,
        });

        let reply = self
            .take_reply(parts.method.as_str(), &path)
            .unwrap_or_else(|| Reply::Json {
                status: 404,
                headers: Vec::new(),
                body: json!({"error": {"message": format!("no scripted response for {path}")}}),
                source: ErrorSource::Upstream,
            });
        let (status, headers, body, source) = match reply {
            Reply::Error(err) => return Err(err),
            Reply::Sse { events, delay } => (
                200,
                vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                Body::Stream(self.sse_body(events, delay)),
                ErrorSource::Upstream,
            ),
            Reply::Script(steps) => (
                200,
                vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                Body::Stream(self.script_body(steps)),
                ErrorSource::Upstream,
            ),
            Reply::Json {
                status,
                mut headers,
                body,
                source,
            } => {
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    headers.push(("content-type".to_owned(), "application/json".to_owned()));
                }
                (
                    status,
                    headers,
                    single_chunk(Bytes::from(body.to_string())),
                    source,
                )
            }
        };
        let mut resp = http::Response::new(body);
        *resp.status_mut() =
            http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
        for (k, v) in headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(&v),
            ) {
                resp.headers_mut().append(name, value);
            }
        }
        resp.extensions_mut().insert(source);
        Ok(resp)
    }
}
