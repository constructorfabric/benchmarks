//! Shared harness for the OAGW integration tests.
//!
//! The harness builds a **real** axum router through
//! [`oagw::api::rest::register_routes`] — the same registration path the gear
//! uses — over a real [`oagw::infra::proxy::pipeline::DataPlaneService`]. Only
//! two things are swapped out for doubles:
//!
//! * the secret source is an [`InMemorySecretSource`], so credential-injection
//!   plugins can resolve `*_ref` keys without the credstore gear;
//! * the calling tenant is fixed (the anonymous tenant) so every test talks to
//!   one tenant namespace unless it overrides the `SecurityContext`.
//!
//! Upstreams are real HTTP servers (httpmock / axum), so the proxy tests are
//! end to end: gateway router → transport → mock upstream.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// The harness is shared by every integration test binary, so each binary sees
// a handful of helpers it does not use itself.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;

use oagw::config::OagwConfig;
use oagw::domain::services::ManagementService;
use oagw::infra::plugin::registry::{AuthPluginRegistry, PluginRegistries};
use oagw::infra::proxy::circuit::CircuitBreakers;
use oagw::infra::proxy::guards::SsrfPolicy;
use oagw::infra::proxy::metrics::DpMetrics;
use oagw::infra::proxy::pipeline::DataPlaneService;
use oagw::infra::proxy::rate_limit::RateLimiter;
use oagw::infra::proxy::runtime::PluginRuntime;
use oagw::infra::proxy::secrets::InMemorySecretSource;
use oagw::infra::proxy::transport::ProxyTransport;
use oagw::infra::storage::MemoryStorage;

/// Tenant every request in the harness acts as (`SecurityContext::anonymous`).
pub const TENANT: uuid::Uuid = uuid::Uuid::nil();

/// Built-in auth plugin identifier for the API-key plugin.
pub const APIKEY_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Built-in auth plugin for OAuth2 client credentials (`client_secret_post`).
pub const OAUTH2_PLUGIN: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Built-in guard plugin: required headers (ADR 0009).
pub const REQUIRED_HEADERS_PLUGIN: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Built-in transform plugin: `X-Request-Id`.
pub const REQUEST_ID_PLUGIN: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Configuration the tests run under: mirrors the graded
/// `config/e2e-local.yaml` posture (`allow_http_upstream: true`, SSRF off, a
/// short proxy budget so a wedged upstream cannot stall a test).
#[must_use]
pub fn test_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        body_limit_bytes: 64 * 1024,
        ..OagwConfig::default()
    }
}

/// Configuration with the SSRF screening disabled (the graded posture).
#[must_use]
pub fn ssrf_off() -> SsrfPolicy {
    SsrfPolicy {
        enabled: false,
        allowed_hosts: Vec::new(),
        denied_hosts: Vec::new(),
    }
}

/// Everything a test may need from the assembled gear.
pub struct Harness {
    /// The router with the management API and the proxy data plane.
    pub app: Router,
    /// The control plane (for direct seeding where the REST API is not the
    /// subject under test).
    pub management: Arc<ManagementService>,
    /// The in-memory secret source backing the credential plugins.
    pub secrets: Arc<InMemorySecretSource>,
}

impl Harness {
    /// Send a request through the router and return the raw response.
    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> axum::http::Response<axum::body::Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "oagw.test");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let bytes = encoded_body(&body);
        let request = builder
            .header("content-type", "application/json")
            .header("content-length", bytes.len().to_string())
            .body(Body::from(bytes))
            .expect("build request");
        self.app.clone().oneshot(request).await.expect("oneshot")
    }

    /// Send a JSON request and decode the response body.
    pub async fn json(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value, axum::http::HeaderMap) {
        let response = self.send(method, uri, headers, body).await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("read body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value, headers)
    }

    /// Send a JSON request and return the body as a byte buffer.
    pub async fn raw(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "oagw.test");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(Body::from(body)).expect("build request");
        let response = self.app.clone().oneshot(request).await.expect("oneshot");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .expect("read body");
        (status, bytes.to_vec(), headers)
    }
}

fn encoded_body(body: &Option<Value>) -> Vec<u8> {
    body.as_ref()
        .map(|value| value.to_string().into_bytes())
        .unwrap_or_default()
}

/// Build a harness with the default configuration.
#[must_use]
pub fn harness() -> Harness {
    harness_with(test_config())
}

/// Build a harness with a custom gear configuration.
#[must_use]
pub fn harness_with(cfg: OagwConfig) -> Harness {
    assemble(cfg, Vec::new(), Arc::new(CircuitBreakers::default()))
}

/// Build a harness with an extra guard plugin registered.
///
/// No built-in guard rejects with a status other than 400/502, so a test that
/// wants to assert another DESIGN status has to supply its own.
#[must_use]
pub fn harness_with_guard(guard: Arc<dyn oagw::domain::plugin::GuardPlugin>) -> Harness {
    assemble(
        test_config(),
        vec![guard],
        Arc::new(CircuitBreakers::default()),
    )
}

/// Build a harness whose circuit breakers trip after `threshold` failures.
#[must_use]
pub fn harness_with_breaker(threshold: u32, cooldown: Duration) -> Harness {
    assemble(
        test_config(),
        Vec::new(),
        Arc::new(CircuitBreakers::new(threshold, cooldown)),
    )
}

fn assemble(
    cfg: OagwConfig,
    extra_guards: Vec<Arc<dyn oagw::domain::plugin::GuardPlugin>>,
    circuits: Arc<CircuitBreakers>,
) -> Harness {
    let cfg = OagwConfig {
        proxy_timeout_secs: cfg.proxy_timeout_secs.max(1),
        ..cfg
    };
    let store = Arc::new(MemoryStorage::new());
    let management = Arc::new(ManagementService::new(store, cfg.clone()));

    let transport = Arc::new(
        ProxyTransport::with_defaults(
            Duration::from_secs(cfg.proxy_timeout_secs.max(1)),
            cfg.allow_http_upstream,
        )
        .expect("upstream transport"),
    );
    let secrets = Arc::new(InMemorySecretSource::new());
    let runtime = Arc::new(PluginRuntime::new(
        Arc::clone(&secrets) as Arc<dyn oagw::infra::proxy::secrets::SecretSource>,
        cfg.token_cache.ttl(),
        cfg.token_cache.capacity(),
        Some(Arc::clone(&transport)),
    ));
    let mut guard_registry = oagw::infra::plugin::registry::GuardPluginRegistry::with_builtins();
    for guard in extra_guards {
        guard_registry.register(guard);
    }
    let registries = Arc::new(PluginRegistries {
        auth: AuthPluginRegistry::with_builtins_for(Arc::clone(&runtime)),
        guard: guard_registry,
        ..PluginRegistries::with_builtins()
    });
    let data_plane = Arc::new(DataPlaneService::new(
        Arc::clone(&management),
        transport,
        registries,
        runtime,
        Arc::new(RateLimiter::new(4096)),
        circuits,
        DpMetrics::new(),
        ssrf_for(&cfg),
        None,
    ));

    let openapi = OpenApiRegistryImpl::new();
    let app =
        oagw::api::rest::register_routes(Router::new(), &openapi, management.clone(), data_plane);
    Harness {
        app,
        management,
        secrets,
    }
}

fn ssrf_for(cfg: &OagwConfig) -> SsrfPolicy {
    SsrfPolicy::from_config(&cfg.ssrf_policy)
}

/// A `SecurityContext` in the harness tenant.
#[must_use]
pub fn security() -> SecurityContext {
    SecurityContext::anonymous()
}

/// `POST /oagw/v1/upstreams` helper that returns the created resource.
pub async fn create_upstream(h: &Harness, body: Value) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(Method::POST, "/oagw/v1/upstreams", &[], Some(body))
        .await;
    (status, value)
}

/// `POST /oagw/v1/routes` helper that returns the created resource.
pub async fn create_route(h: &Harness, body: Value) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(Method::POST, "/oagw/v1/routes", &[], Some(body))
        .await;
    (status, value)
}

/// A minimal HTTP upstream body pointing at `host:port`.
#[must_use]
pub fn http_upstream(host: &str, port: u16, alias: Option<&str>) -> Value {
    json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "http", "host": host, "port": port } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
}

/// `PUT /oagw/v1/upstreams/{id}` helper.
pub async fn put_upstream(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            &[],
            Some(body),
        )
        .await;
    (status, value)
}

/// `GET /oagw/v1/upstreams/{id}` helper.
pub async fn get_upstream(h: &Harness, id: &str) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(Method::GET, &format!("/oagw/v1/upstreams/{id}"), &[], None)
        .await;
    (status, value)
}

/// `POST /oagw/v1/plugins` helper.
pub async fn create_plugin(h: &Harness, body: Value) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(Method::POST, "/oagw/v1/plugins", &[], Some(body))
        .await;
    (status, value)
}

/// A custom (UUID-backed) guard plugin whose source is inline.
#[must_use]
pub fn inline_guard_plugin(name: &str, source: &str) -> Value {
    json!({
        "enabled": true,
        "name": name,
        "type": "guard",
        "sharing": "private",
        "tags": ["test"],
        "config": {"required_request_headers": "x-correlation-id"},
        "phases": ["on_request"],
        "source": {"kind": "inline", "source_code": source, "language": "json"}
    })
}

/// A token-bucket rate limit block with a burst capacity.
///
/// `scope` is one of the schema's `global|tenant|user|ip|route`; the upstream id
/// is always part of the counter key, so every scope is per-upstream too.
#[must_use]
pub fn token_bucket(rate: u64, burst: u64, scope: &str) -> Value {
    json!({
        "sharing": "private",
        "algorithm": "token_bucket",
        "sustained": {"rate": rate, "window": "second"},
        "burst": {"capacity": burst},
        "scope": scope,
        "strategy": "reject",
        "cost": 1
    })
}

/// A plugin chain carrying one bound plugin reference.
#[must_use]
pub fn plugin_chain(sharing: &str, refs: &[&str]) -> Value {
    json!({
        "sharing": sharing,
        "items": refs.iter().map(|r| json!(r)).collect::<Vec<_>>()
    })
}

/// A route bound to `upstream_id` serving `GET|POST|PUT|DELETE|PATCH` on `path`.
#[must_use]
pub fn catch_all_route(upstream_id: &str, path: &str) -> Value {
    json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                "path": path,
                "path_suffix_mode": "append"
            }
        }
    })
}

/// A route that only serves `methods` on `path`.
#[must_use]
pub fn route_for(upstream_id: &str, methods: &[&str], path: &str) -> Value {
    json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": { "methods": methods, "path": path, "path_suffix_mode": "append" } }
    })
}

/// Create an upstream + catch-all route; returns `(upstream_id, route_id)`.
pub async fn setup_upstream_with_route(
    h: &Harness,
    upstream: Value,
    path: &str,
) -> (String, String) {
    let (status, upstream_body, _) = h
        .json(Method::POST, "/oagw/v1/upstreams", &[], Some(upstream))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "upstream creation failed: {upstream_body}"
    );
    let upstream_id = upstream_body["id"]
        .as_str()
        .expect("upstream id")
        .to_owned();
    let (status, route_body, _) = h
        .json(
            Method::POST,
            "/oagw/v1/routes",
            &[],
            Some(catch_all_route(&upstream_id, path)),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "route creation failed: {route_body}"
    );
    let route_id = route_body["id"].as_str().expect("route id").to_owned();
    (upstream_id, route_id)
}

/// Proxy a request through the data plane.
pub async fn proxy(
    h: &Harness,
    method: Method,
    alias: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> axum::http::response::Response<axum::body::Body> {
    let uri = if path.is_empty() {
        format!("/oagw/v1/proxy/{alias}")
    } else {
        format!("/oagw/v1/proxy/{alias}{path}")
    };
    h.send(method, &uri, headers, body).await
}

/// Read a response body as bytes.
pub async fn body_bytes(response: axum::body::Body) -> Vec<u8> {
    axum::body::to_bytes(response, 64 * 1024 * 1024)
        .await
        .expect("body")
        .to_vec()
}

/// Read a response body as a UTF-8 string.
pub async fn body_string(response: axum::body::Body) -> String {
    String::from_utf8(body_bytes(response).await).expect("utf-8 body")
}

/// `HeaderValue` from a `&str`, panicking on invalid input.
#[must_use]
pub fn header(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("header value")
}

// ------------------------------------------------------------------ upstreams
//
// A local echo upstream. Unlike a canned mock it reports back what it received,
// so a test can assert exactly what the gateway forwarded.

/// The request an [`echo_server`] received, serialised as JSON.
#[derive(Debug, Clone)]
pub struct Echo {
    /// HTTP method as received.
    pub method: String,
    /// Request path as received.
    pub path: String,
    /// Query string as received (without `?`).
    pub query: String,
    /// Headers as received, in arrival order.
    pub headers: Vec<(String, String)>,
}

#[derive(Clone)]
struct EchoRecorder(std::sync::Arc<std::sync::Mutex<Vec<Echo>>>);

/// A local HTTP upstream that answers every request with a JSON document
/// describing what it received. Bind host is always `127.0.0.1`.
///
/// # Panics
/// When the loopback listener cannot be bound.
pub async fn echo_server() -> (String, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo server");
    let port = listener.local_addr().expect("local addr").port();
    let recorder = EchoRecorder(Arc::new(std::sync::Mutex::new(Vec::new())));
    let app = Router::new().fallback(echo_handler).with_state(recorder);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("echo server");
    });
    ("127.0.0.1".to_owned(), port)
}

async fn echo_handler(
    State(recorder): State<EchoRecorder>,
    request: Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let json_body = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in &parts.headers {
        if let (Ok(name), Ok(value)) = (name.as_str().parse::<String>(), value.to_str()) {
            headers.push((name, value.to_owned()));
        }
    }
    recorder.0.lock().expect("echo recorder").push(Echo {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        headers: headers.clone(),
    });
    let seen = json!({
        "method": parts.method.as_str(),
        "path": parts.uri.path(),
        "query": parts.uri.query().unwrap_or_default(),
        "headers": headers.into_iter().collect::<BTreeMap<String, String>>(),
        "json": json_body,
    });
    (StatusCode::OK, axum::Json(seen)).into_response()
}

/// A harness whose request-body budget is deliberately tiny.
#[must_use]
pub fn harness_with_small_body() -> OagwConfig {
    OagwConfig {
        body_limit_bytes: 1024,
        ..test_config()
    }
}

/// `POST /oagw/v1/proxy/{alias}{path}` with `headers`, returning the decoded
/// JSON the upstream echoed back.
pub async fn post_json(
    h: &Harness,
    alias: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(
            Method::POST,
            &format!("/oagw/v1/proxy/{alias}{path}"),
            headers,
            Some(body),
        )
        .await;
    (status, value)
}

/// Serve the harness router on a real loopback listener.
///
/// `tower::ServiceExt::oneshot` hands the router a request that carries no
/// `hyper::upgrade::OnUpgrade` extension, so a protocol upgrade can only be
/// exercised over a real socket. Returns the address to dial.
///
/// # Panics
/// When the loopback listener cannot be bound.
pub async fn served(h: &Harness) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway listener");
    let addr = listener.local_addr().expect("gateway local addr");
    let app = h.app.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("gateway server");
    });
    addr
}
