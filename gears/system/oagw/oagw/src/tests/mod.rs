//! Integration tests for the OAGW gear (`#[cfg(test)]` only).
//!
//! Two harnesses are used:
//!
//! - in-process router tests, driven with `tower::ServiceExt::oneshot`, for
//!   status codes and JSON bodies;
//! - bound servers on ephemeral loopback ports (`127.0.0.1:0`) for the
//!   transports that need a real connection: streamed responses and
//!   WebSocket upgrades.
//!
//! Upstream origins are provided by `httpmock` (also on an ephemeral port)
//! or by a hand-rolled `tokio::net::TcpListener`, never on a fixed port.

#![allow(dead_code)]

mod config_gating;
mod control_plane;
mod data_plane;
mod streaming;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::test_util::MockCredStoreClient;
use serde_json::{Value, json};
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::util::ServiceExt;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::plugin::{
    AuthPluginRegistry, GuardPluginRegistry, TokenCacheConfig, TransformPluginRegistry,
};
use crate::domain::service::{ControlPlaneService, DataPlaneService};
use crate::infra::proxy::ProxyEngine;
use crate::infra::store::Store;

/// Base of every OAGW error `type` identifier (`DESIGN.md` "Error Response
/// Format"); the implementation appends `ErrorKind::gts_fragment()`.
const ERROR_TYPE_BASE: &str = "gts.cf.core.errors.err.v1~";

/// Value of `X-OAGW-Error-Source` for gateway-produced failures.
const SOURCE_GATEWAY: &str = "gateway";
/// Value of `X-OAGW-Error-Source` for relayed upstream responses.
const SOURCE_UPSTREAM: &str = "upstream";

/// Header distinguishing gateway-produced failures from relayed upstream
/// responses.
const ERROR_SOURCE_HEADER: &str = crate::domain::error::ERROR_SOURCE_HEADER;

// ---------------------------------------------------------------------------
// Tenant resolver double
// ---------------------------------------------------------------------------

/// In-process tenant resolver double: a static parent map.
struct TestResolver {
    parents: HashMap<Uuid, Uuid>,
}

impl TestResolver {
    fn new(parents: &[(Uuid, Uuid)]) -> Self {
        Self {
            parents: parents.iter().copied().collect(),
        }
    }

    /// Ancestor chain of `id`, direct parent first, root last.
    fn chain(&self, id: Uuid) -> Vec<Uuid> {
        let mut chain = Vec::new();
        let mut cursor = self.parents.get(&id).copied();
        while let Some(next) = cursor {
            chain.push(next);
            cursor = self.parents.get(&next).copied();
        }
        chain
    }

    fn parent_of(&self, id: Uuid) -> Option<TenantId> {
        self.parents.get(&id).map(|parent| TenantId(*parent))
    }

    fn reference(&self, id: Uuid) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self.parent_of(id),
            self_managed: false,
        }
    }
}

impl Default for TestResolver {
    fn default() -> Self {
        Self::new(&[])
    }
}

#[async_trait]
impl TenantResolverClient for TestResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(TenantInfo {
            id,
            name: format!("tenant-{}", id.0),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self.parent_of(id.0),
            self_managed: false,
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        let id = self
            .chain(Uuid::nil())
            .last()
            .copied()
            .unwrap_or_else(Uuid::nil);
        Ok(TenantInfo {
            id: TenantId(id),
            name: "root".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids
            .iter()
            .map(|id| TenantInfo {
                id: *id,
                name: format!("tenant-{}", id.0),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: self.parent_of(id.0),
                self_managed: false,
            })
            .collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        // The platform resolver has no tenant for the nil UUID, and the data
        // plane hands it exactly that for an anonymous caller whose alias named
        // no upstream. Mirroring the fault keeps this double honest: an
        // unknown-alias request must not depend on this call succeeding.
        if id.0.is_nil() {
            return Err(TenantResolverError::TenantNotFound { tenant_id: id });
        }
        Ok(GetAncestorsResponse {
            tenant: self.reference(id.0),
            ancestors: self
                .chain(id.0)
                .iter()
                .map(|id| self.reference(*id))
                .collect(),
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Ok(GetDescendantsResponse {
            tenant: self.reference(id.0),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(self.chain(descendant_id.0).contains(&ancestor_id.0))
    }
}

// ---------------------------------------------------------------------------
// Gateway assembly
// ---------------------------------------------------------------------------

/// An in-process OAGW deployment: control plane, data plane, and router.
struct Gateway {
    router: Router,
    control: Arc<ControlPlaneService>,
    data: Arc<DataPlaneService>,
    store: Arc<Store>,
    /// The `OpenAPI` registry the router's operations were registered on, so a
    /// test can assert what the generated document advertises.
    openapi: Arc<OpenApiRegistryImpl>,
}

/// Build a gateway with the default configuration and an empty credential
/// store.
fn gateway() -> Gateway {
    gateway_with(OagwConfig::default())
}

/// Build a gateway with `config` and an empty credential store.
fn gateway_with(config: OagwConfig) -> Gateway {
    gateway_with_creds(config, Arc::new(MockCredStoreClient::empty()))
}

/// The one assembly the other constructors share: `credstore` backs the
/// built-in auth plugins unless `auth` is given, and `parents` seeds the
/// tenant resolver's hierarchy.
fn assemble(
    config: OagwConfig,
    credstore: Arc<dyn CredStoreClientV1>,
    parents: &[(Uuid, Uuid)],
    auth: Option<AuthPluginRegistry>,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
) -> Gateway {
    let store = Arc::new(Store::new());
    let engine = Arc::new(ProxyEngine::new(config.clone()).expect("proxy engine builds"));
    let auth_plugins = Arc::new(auth.unwrap_or_else(move || {
        AuthPluginRegistry::with_builtins(credstore, TokenCacheConfig::default())
    }));
    let resolver = Arc::new(TestResolver::new(parents));
    let guards = Arc::new(guards);
    let transforms = Arc::new(transforms);
    let control = Arc::new(ControlPlaneService::new(
        store.clone(),
        resolver.clone(),
        auth_plugins.clone(),
        guards.clone(),
        transforms.clone(),
    ));
    let data = Arc::new(DataPlaneService::new(
        config,
        store.clone(),
        engine,
        auth_plugins,
        guards,
        transforms,
        resolver,
    ));
    let openapi = Arc::new(OpenApiRegistryImpl::new());
    let router = crate::api::routes::register_routes(
        Router::new(),
        openapi.as_ref(),
        control.clone(),
        data.clone(),
    );
    Gateway {
        router,
        control,
        data,
        store,
        openapi,
    }
}

/// Build a gateway with `config`, a custom credential store, and custom
/// guard/transform plugin registries.
fn gateway_custom(
    config: OagwConfig,
    credstore: Arc<dyn CredStoreClientV1>,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
) -> Gateway {
    assemble(config, credstore, &[], None, guards, transforms)
}

/// Build a gateway with `config` and an explicitly built auth registry, for the
/// tests that need a plugin the built-in registry does not carry.
fn gateway_custom_auth(config: OagwConfig, auth_plugins: AuthPluginRegistry) -> Gateway {
    assemble(
        config,
        Arc::new(MockCredStoreClient::empty()),
        &[],
        Some(auth_plugins),
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
    )
}

/// Build a gateway with `config`, a custom credential store, and the built-in
/// plugin registries.
fn gateway_with_creds(config: OagwConfig, credstore: Arc<dyn CredStoreClientV1>) -> Gateway {
    gateway_custom(
        config,
        credstore,
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
    )
}

/// Build a gateway whose tenant resolver knows the `child -> parent` links of
/// `hierarchy`, for the sharing and inheritance tests.
fn gateway_with_parents(config: OagwConfig, parents: &[(Uuid, Uuid)]) -> Gateway {
    assemble(
        config,
        Arc::new(MockCredStoreClient::empty()),
        parents,
        None,
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
    )
}

/// [`gateway_with_parents`] with a custom transform registry, for the tests
/// that observe how the ancestor and descendant plugin chains merge.
fn gateway_with_parents_transforms(
    config: OagwConfig,
    parents: &[(Uuid, Uuid)],
    transforms: TransformPluginRegistry,
) -> Gateway {
    assemble(
        config,
        Arc::new(MockCredStoreClient::empty()),
        parents,
        None,
        GuardPluginRegistry::with_builtins(),
        transforms,
    )
}

// ---------------------------------------------------------------------------
// Request/response helpers
// ---------------------------------------------------------------------------

/// The security context a control-plane caller presents.
fn sec_ctx(tenant: Uuid) -> SecurityContext {
    sec_ctx_as(tenant, Uuid::new_v4())
}

/// A security context with an explicit subject, for `scope: user` rate limits.
fn sec_ctx_as(tenant: Uuid, subject: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_type("oagw-test")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// An anonymous data-plane request: the alias identifies the tenant.
fn anon_request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request builds")
}

/// An authenticated request with no body.
fn request(method: Method, uri: &str, tenant: Uuid) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(sec_ctx(tenant))
        .body(Body::empty())
        .expect("request builds")
}

/// An authenticated JSON request.
fn json_request(method: Method, uri: &str, tenant: Uuid, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(sec_ctx(tenant))
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

/// A data-plane request with arbitrary headers, body, and optional identity.
fn proxy_request(
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    identity: Option<SecurityContext>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(identity) = identity {
        builder = builder.extension(identity);
    }
    builder
        .body(Body::from(body.to_vec()))
        .expect("request builds")
}

/// A data-plane request that presents `peer` as the TCP peer address, the way
/// `into_make_service_with_connect_info` does in a deployed gateway.
fn proxy_request_from(method: Method, uri: &str, peer: std::net::SocketAddr) -> Request<Body> {
    proxy_request_with(method, uri, &[], b"", None, Some(peer))
}

/// [`proxy_request`] that also presents `peer` as the TCP peer address.
fn proxy_request_with(
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    identity: Option<SecurityContext>,
    peer: Option<std::net::SocketAddr>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(identity) = identity {
        builder = builder.extension(identity);
    }
    if let Some(peer) = peer {
        builder = builder.extension(axum::extract::ConnectInfo(peer));
    }
    builder
        .body(Body::from(body.to_vec()))
        .expect("request builds")
}

// ---------------------------------------------------------------------------
// `tracing` capture
// ---------------------------------------------------------------------------

/// A `tracing` subscriber that records every event it sees as one flat
/// `name=value` line, so a test can assert on a field without a formatter
/// dependency: the crate carries no `tracing-subscriber`.
#[derive(Default, Clone)]
struct LogCapture {
    events: CaptureBuffer,
}

/// One capture's event buffer, shared by the subscriber and its test.
type CaptureBuffer = Arc<std::sync::Mutex<Vec<String>>>;

impl LogCapture {
    /// Every event recorded while the subscriber was installed.
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("log capture lock").clone()
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::Id {
        tracing::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!(
            "level={:?} target={}",
            event.metadata().level(),
            event.metadata().target()
        );
        event.record(&mut FieldWriter { line: &mut line });
        for buffer in active_captures()
            .lock()
            .expect("log capture registry")
            .iter()
        {
            buffer.lock().expect("log capture lock").push(line.clone());
        }
    }

    fn enter(&self, _span: &tracing::Id) {}

    fn exit(&self, _span: &tracing::Id) {}
}

/// Flattens one event's fields into the captured line.
struct FieldWriter<'a> {
    line: &'a mut String,
}

/// Renders a `Debug` value through `Display`, so the field writer formats with
/// `{}` only: `value` is a `&dyn Debug` and has no `Display` of its own, and
/// the output is byte-identical either way.
struct DebugOf<'a>(&'a dyn std::fmt::Debug);

impl std::fmt::Display for DebugOf<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.0, formatter)
    }
}

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        write!(self.line, " {}={}", field.name(), DebugOf(value))
            .expect("a String accepts every write");
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        write!(self.line, " {}={}", field.name(), value).expect("a String accepts every write");
    }
}

/// Run `body` under a capturing subscriber and return what it logged.
///
/// The capture is installed as the process-wide default, because a callsite's
/// interest is computed once, when the callsite is first evaluated, against the
/// dispatchers installed at that moment — and with no global subscriber
/// installed, `tracing` caches `Interest::NEVER` for every callsite it
/// evaluated first. Re-registering the callsites against a subscriber that
/// accepts everything un-caches them, so a test sees the events the code under
/// test actually emitted.
///
/// The process-wide default also matters for *routing*: an event goes to the
/// calling thread's dispatch when there is one and to the global default
/// otherwise, and the relay does some of its work off the thread that drove the
/// request. The installed subscriber therefore records every event it sees into
/// the buffers of every capture whose body is running ([`RegisteredCapture`]).
///
/// Concurrent tests may capture at the same time, so a capture can also hold
/// events another test emitted: assertions filter by the aliases, identifiers,
/// and correlation ids that test alone created.
fn capture_logs<F, R>(body: F) -> (LogCapture, R)
where
    F: FnOnce() -> R,
{
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    // Only the first call wins, and that is enough: the subscriber is
    // stateless with respect to *which* capture records, and it records into
    // whatever is active at the time of the event.
    if INSTALLED.set(()).is_ok() {
        // Only the first call installs, so a competing install is not a failure
        // this harness can act on; the subscriber records into any capture.
        #[allow(clippy::let_underscore_must_use)]
        let _ = tracing::subscriber::set_global_default(LogCapture::default());
    }
    // Every call re-registers the callsites, so callsites first evaluated by
    // another test that ran before any capture was installed are un-cached.
    tracing::callsite::rebuild_interest_cache();

    let capture = LogCapture::default();
    let registered = RegisteredCapture::register(capture.events.clone());
    let result = body();
    drop(registered);
    (capture, result)
}

/// The buffers of the captures currently running their body.
fn active_captures() -> &'static std::sync::Mutex<Vec<CaptureBuffer>> {
    static ACTIVE: std::sync::OnceLock<std::sync::Mutex<Vec<CaptureBuffer>>> =
        std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Adds its capture's buffer to the registry for the duration of the body,
/// panic or not.
struct RegisteredCapture(Arc<std::sync::Mutex<Vec<String>>>);

impl RegisteredCapture {
    fn register(buffer: Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        active_captures()
            .lock()
            .expect("log capture registry")
            .push(buffer.clone());
        Self(buffer)
    }
}

impl Drop for RegisteredCapture {
    fn drop(&mut self) {
        let mut active = active_captures().lock().expect("log capture registry");
        if let Some(index) = active
            .iter()
            .position(|buffer| Arc::ptr_eq(buffer, &self.0))
        {
            active.swap_remove(index);
        }
    }
}

/// Drive `request` through the router.
async fn send(router: &Router, request: Request<Body>) -> Response {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router is infallible")
}

/// The response body as bytes.
async fn body_bytes(response: &mut Response) -> Vec<u8> {
    http_body_util::BodyExt::collect(response.body_mut())
        .await
        .expect("response body collects")
        .to_bytes()
        .to_vec()
}

/// The response body as a lossy UTF-8 string.
async fn body_text(response: &mut Response) -> String {
    String::from_utf8_lossy(&body_bytes(response).await).into_owned()
}

/// The response body as JSON.
async fn body_json(response: &mut Response) -> Value {
    serde_json::from_slice(&body_bytes(response).await).expect("response body is JSON")
}

// ---------------------------------------------------------------------------
// Payload builders
// ---------------------------------------------------------------------------

/// An upstream payload pointing at `host:port` with an explicit alias.
fn upstream_json(host: &str, port: u16, alias: &str) -> Value {
    json!({
        "alias": alias,
        "server": {
            "endpoints": [{ "scheme": "http", "host": host, "port": port }]
        }
    })
}

/// A route payload for `upstream` matching `path` with `methods`.
fn route_json(upstream: Uuid, path: &str, methods: &[&str]) -> Value {
    json!({
        "upstream_id": upstream,
        "match": {
            "http": {
                "methods": methods,
                "path": path,
                "query_allowlist": [],
                "path_suffix_mode": "append"
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Problem-response assertions
// ---------------------------------------------------------------------------

/// Assert the RFC 9457 shape, GTS `type`, status, and gateway error source.
#[track_caller]
fn assert_problem(response: &Response, fragment: &str, status: u16) {
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        crate::domain::error::PROBLEM_JSON,
        "gateway failures must be application/problem+json"
    );
    assert_eq!(
        response
            .headers()
            .get(crate::domain::error::ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        SOURCE_GATEWAY,
        "gateway failures must carry the gateway error source"
    );
    assert_eq!(
        response.status().as_u16(),
        status,
        "unexpected status for the `{fragment}` problem"
    );
}

/// Assert a rendered problem body: `type`, `status`, `title`, and `instance`.
async fn assert_problem_body(response: &mut Response, fragment: &str, status: u16, instance: &str) {
    assert_problem(response, fragment, status);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        json!(format!("{ERROR_TYPE_BASE}{fragment}")),
        "unexpected GTS error type"
    );
    assert_eq!(body["status"], json!(status));
    assert_eq!(body["instance"], json!(instance));
    assert!(
        body["title"]
            .as_str()
            .is_some_and(|title| !title.is_empty()),
        "problem body must carry a title"
    );
    assert!(
        body["error_code"]
            .as_str()
            .is_some_and(|code| !code.is_empty()),
        "problem body must carry a machine-readable error_code"
    );
}

/// Extract the `field` context member of a validation problem body.
#[track_caller]
fn problem_field(body: &Value) -> String {
    body["field"]
        .as_str()
        .or_else(|| body["context"]["field"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// `GET` convenience for the control plane.
async fn get_json(gateway: &Gateway, uri: &str, tenant: Uuid) -> Response {
    send(&gateway.router, request(Method::GET, uri, tenant)).await
}

/// `POST` a JSON payload to the control plane.
async fn post_json(gateway: &Gateway, uri: &str, tenant: Uuid, body: Value) -> Response {
    send(
        &gateway.router,
        json_request(Method::POST, uri, tenant, &body),
    )
    .await
}

/// `PUT` a JSON payload to the control plane.
async fn put_json(gateway: &Gateway, uri: &str, tenant: Uuid, body: Value) -> Response {
    send(
        &gateway.router,
        json_request(Method::PUT, uri, tenant, &body),
    )
    .await
}

/// `DELETE` a control-plane resource.
async fn delete(gateway: &Gateway, uri: &str, tenant: Uuid) -> Response {
    send(&gateway.router, request(Method::DELETE, uri, tenant)).await
}

/// Status code of a response.
fn status_of(response: &Response) -> StatusCode {
    response.status()
}

// ---------------------------------------------------------------------------
// Loopback upstream origin
// ---------------------------------------------------------------------------

/// A request captured by an [`Origin`].
#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    target: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    /// Value of a captured request header, last occurrence wins.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(name_, _)| name_ == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether a captured request header is present.
    fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// The captured body as a lossy UTF-8 string.
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// What an [`Origin`] answers with.
enum Reply {
    /// `200 OK` whose JSON body echoes the request back to the caller.
    Echo,
    /// A fixed status, header set, and body.
    Fixed {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
    /// A `200 OK` with a chunked body delivered piecewise, so a test can
    /// observe the gateway streaming the first frames before the origin has
    /// finished answering.
    Chunks {
        /// Value of the `content-type` response header.
        content_type: &'static str,
        /// Body pieces, each written as its own chunked-encoding chunk.
        chunks: Vec<&'static [u8]>,
        /// Milliseconds to wait between two pieces.
        pause_ms: u64,
    },
    /// A `200 OK` chunked response that is terminated by closing the socket
    /// mid-stream: the terminal chunk is never written.
    Cut {
        /// Value of the `content-type` response header.
        content_type: &'static str,
        /// The single chunk written before the connection drops.
        head: &'static [u8],
    },
    /// Accept the connection and never answer (exercises the upstream
    /// deadline).
    Stall,
    /// A `200 OK` chunked response that writes one chunk and then stops
    /// writing anything more without closing the socket, so the relay is left
    /// waiting for a frame that never comes.
    StallAfter {
        /// Value of the `content-type` response header.
        content_type: &'static str,
        /// The single chunk written before the origin goes silent.
        head: &'static [u8],
    },
}
/// A hand-rolled loopback origin: the minimal HTTP/1.1 server the data-plane
/// tests need in order to observe exactly what the gateway forwarded.
///
/// The listener is always bound to an ephemeral port on `127.0.0.1`, and the
/// accept loop is aborted when the handle is dropped.
struct Origin {
    host: String,
    port: u16,
    requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Origin {
    /// Start an origin answering every request with `reply`.
    async fn spawn(reply: Reply) -> Self {
        Self::bind("127.0.0.1:0", reply).await
    }

    /// Start an origin on the IPv6 loopback, for the authority-format tests.
    async fn spawn_ipv6(reply: Reply) -> Self {
        Self::bind("[::1]:0", reply).await
    }

    /// Bind an origin on `address` answering every request with `reply`.
    async fn bind(address: &str, reply: Reply) -> Self {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .expect("origin listener binds");
        let bound = listener.local_addr().expect("local addr");
        let port = bound.port();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                loop {
                    let Some(request) = read_request(&mut socket).await else {
                        break;
                    };
                    captured.lock().expect("capture lock").push(request.clone());
                    if !write_reply(&mut socket, &reply, &request).await {
                        break;
                    }
                }
            }
        });
        Self {
            // The endpoint spelling is the bare address; bracketing an IPv6
            // literal is the gateway's job when it builds an authority.
            host: bound.ip().to_string(),
            port,
            requests,
            task,
        }
    }

    /// Hostname to register in an upstream endpoint.
    fn host(&self) -> &str {
        &self.host
    }

    /// Port to register in an upstream endpoint.
    fn port(&self) -> u16 {
        self.port
    }

    /// Every request the origin has captured, oldest first.
    fn captured(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("capture lock").clone()
    }

    /// The most recent captured request.
    fn last(&self) -> CapturedRequest {
        self.captured()
            .pop()
            .expect("the origin captured at least one request")
    }

    /// The single captured request, asserting nothing else arrived.
    fn only(&self) -> CapturedRequest {
        let captured = self.captured();
        assert_eq!(
            captured.len(),
            1,
            "exactly one request should have reached the origin, got {captured:?}"
        );
        captured.into_iter().next().expect("one captured request")
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Read one HTTP request off the socket, or `None` at end of stream.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<CapturedRequest> {
    use tokio::io::AsyncReadExt;

    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.clone(), None),
    };
    Some(CapturedRequest {
        method,
        target,
        path,
        query,
        headers,
        body,
    })
}

/// Write the reply, returning whether the connection may be reused.
///
/// It never is: every head declares `connection: close`, so the caller drops
/// the socket after one exchange, and a refused write cuts the reply short,
/// which ends the exchange too.
async fn write_reply(
    socket: &mut tokio::net::TcpStream,
    reply: &Reply,
    request: &CapturedRequest,
) -> bool {
    write_reply_parts(socket, reply, request)
        .await
        .unwrap_or(false)
}

/// Write the whole reply, stopping at the first write the socket refuses.
async fn write_reply_parts(
    socket: &mut tokio::net::TcpStream,
    reply: &Reply,
    request: &CapturedRequest,
) -> std::io::Result<bool> {
    use tokio::io::AsyncWriteExt;

    if let Reply::Chunks {
        content_type,
        chunks,
        pause_ms,
    } = reply
    {
        return write_chunked_reply(socket, content_type, chunks, *pause_ms).await;
    }

    if let Reply::Cut { content_type, head } = reply {
        // No length is declared at all, so the body is delimited by the
        // connection closing: exactly what "the upstream died mid-stream"
        // looks like on the wire.
        let cut =
            format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n");
        socket.write_all(cut.as_bytes()).await?;
        socket.write_all(head).await?;
        socket.flush().await?;
        return Ok(false);
    }

    if let Reply::StallAfter { content_type, head } = reply {
        write_stalled_head(socket, content_type, head).await?;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        return Ok(false);
    }

    write_plain_reply(socket, reply, request).await
}

/// A `200 OK` whose body is streamed one piece per `pause_ms`.
async fn write_chunked_reply(
    socket: &mut tokio::net::TcpStream,
    content_type: &str,
    chunks: &[&[u8]],
    pause_ms: u64,
) -> std::io::Result<bool> {
    use tokio::io::AsyncWriteExt;

    let mut head = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n");
    if !chunks.is_empty() {
        head.push_str("transfer-encoding: chunked\r\n");
    }
    head.push_str("connection: close\r\n\r\n");
    socket.write_all(head.as_bytes()).await?;
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
        }
        let piece = format!("{:x}\r\n", chunk.len());
        socket.write_all(piece.as_bytes()).await?;
        socket.write_all(chunk).await?;
        socket.write_all(b"\r\n").await?;
        socket.flush().await?;
    }
    socket.write_all(b"0\r\n\r\n").await?;
    socket.flush().await?;
    Ok(false)
}

/// The head of a stalled reply: one chunk, then silence, with the connection
/// left open and no terminal chunk ever written.
async fn write_stalled_head(
    socket: &mut tokio::net::TcpStream,
    content_type: &str,
    head: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let start = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n"
    );
    socket.write_all(start.as_bytes()).await?;
    let piece = format!("{:x}\r\n", head.len());
    socket.write_all(piece.as_bytes()).await?;
    socket.write_all(head).await?;
    socket.write_all(b"\r\n").await?;
    socket.flush().await
}

/// A reply with a declared length: status line, headers, body.
struct PlainReply {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl PlainReply {
    /// The plain reply for `reply`, which the caller has already narrowed to
    /// one that is answered in full.
    fn of(reply: &Reply, request: &CapturedRequest) -> Self {
        match reply {
            Reply::Echo => Self {
                status: 200,
                reason: "OK",
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: serde_json::to_vec(&echo_of(request)).unwrap_or_default(),
            },
            Reply::Fixed {
                status,
                headers,
                body,
            } => Self {
                status: *status,
                reason: "Fixed",
                headers: headers
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect(),
                body: body.clone(),
            },
            Reply::Chunks { .. } | Reply::Cut { .. } | Reply::StallAfter { .. } | Reply::Stall => {
                unreachable!("every other reply kind is written elsewhere")
            }
        }
    }

    /// The head bytes: status line, headers, length, and `connection: close`.
    fn head(&self) -> String {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (name, value) in &self.headers {
            write!(head, "{name}: {value}\r\n").expect("a String accepts every write");
        }
        write!(head, "content-length: {}\r\n", self.body.len())
            .expect("a String accepts every write");
        head.push_str("connection: close\r\n\r\n");
        head
    }
}

/// Write a reply in full, then close the connection.
async fn write_plain_reply(
    socket: &mut tokio::net::TcpStream,
    reply: &Reply,
    request: &CapturedRequest,
) -> std::io::Result<bool> {
    use tokio::io::AsyncWriteExt;

    if matches!(reply, Reply::Stall) {
        // Hold the connection open without ever answering, so the caller
        // exercises the upstream deadline.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        return Ok(false);
    }

    let plain = PlainReply::of(reply, request);
    socket.write_all(plain.head().as_bytes()).await?;
    socket.write_all(&plain.body).await?;
    socket.flush().await?;
    Ok(false)
}

/// The JSON echo an [`Origin`] sends back for a captured request.
fn echo_of(request: &CapturedRequest) -> serde_json::Value {
    serde_json::json!({
        "method": request.method,
        "path": request.path,
        "query": request.query,
        "headers": request.headers,
        "body": request.body_text(),
    })
}
