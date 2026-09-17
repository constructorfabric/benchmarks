//! Data-plane transport tests.
//!
//! Both ends are real sockets: the upstream is an axum echo server and the
//! gateway is the router this crate registers, so the tests cover the wire
//! behaviour of plain HTTP, a streamed event stream and a WebSocket upgrade.

use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Router, extract::State};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::api::rest::routes::register_routes;
use crate::api::rest::state::OagwState;
use crate::config::OagwConfig;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Spawn the echo upstream and a gateway that forwards `alias` to it, and
/// return the gateway's port.
async fn stack(alias: &str, spec: Value, config: &OagwConfig) -> u16 {
    let upstream = spawn(axum_app()).await;
    gateway_on(alias, upstream_port(&spec, upstream), config).await
}

/// The port an upstream spec is pointed at, rewritten to `port`.
fn upstream_port(spec: &Value, port: u16) -> Value {
    let mut rewritten = spec.clone();
    rewritten["server"]["endpoints"][0]["host"] = Value::from("127.0.0.1");
    rewritten["server"]["endpoints"][0]["port"] = Value::from(port);
    rewritten
}

/// Spawn a gateway serving a single upstream, and return its port.
///
/// The platform `OoP` middleware supplies the security context in production;
/// the tests inject a fixed one instead.
async fn gateway_on(alias: &str, spec: Value, config: &OagwConfig) -> u16 {
    let state = OagwState::assemble(
        control_plane(),
        &resolver(),
        std::sync::Arc::new(config.clone()),
    );
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, std::sync::Arc::new(state))
        .layer(axum::Extension(security_context()));
    let created = register(router.clone(), alias, &spec).await;
    assert_eq!(created, StatusCode::CREATED, "upstream registration");
    spawn(router).await
}

/// A control plane over a fresh store.
fn control_plane() -> std::sync::Arc<crate::domain::services::management::ControlPlaneService> {
    let store = crate::infra::storage::memory::MemoryStore::new();
    std::sync::Arc::new(
        crate::domain::services::management::ControlPlaneService::new(
            store.clone(),
            store.clone() as std::sync::Arc<dyn crate::domain::repo::RouteRepository>,
            store as std::sync::Arc<dyn crate::domain::repo::PluginRepository>,
        ),
    )
}

/// A fixed caller identity for every proxied request.
fn security_context() -> toolkit_security::SecurityContext {
    use uuid::Uuid;
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(TENANT)
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"))
}

/// Deterministic tenant used by every request in this module.
static TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00ca);

/// A resolver backed by an empty credential store.
fn resolver() -> crate::infra::credentials::SecretResolver {
    crate::infra::credentials::SecretResolver::new(std::sync::Arc::new(
        credstore_sdk::test_util::MockCredStoreClient::empty(),
    ))
}

/// Bind `router` to an ephemeral loopback port and serve it in the background.
async fn spawn(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let port = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("local addr: {error}"))
        .port();
    tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    port
}

/// Register `spec` under `alias` through the management API.
async fn register(router: Router, alias: &str, spec: &Value) -> StatusCode {
    let mut payload = spec.clone();
    payload["alias"] = Value::from(alias);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap_or_else(|error| panic!("register request: {error}"));
    router
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("register: {error}"))
        .status()
}

/// Echo upstream: reports the request facts it received and offers a stream
/// and a WebSocket echo.
fn axum_app() -> Router {
    Router::new()
        .route("/echo", any(echo))
        .route("/sse", any(sse))
        .route("/ws", any(websocket))
}

/// Echo the request method, path, query and headers back as JSON.
async fn echo(request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let payload = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_else(|error| panic!("echo body: {error}"));
    let mut headers = serde_json::Map::new();
    for (name, value) in &parts.headers {
        let text = value
            .to_str()
            .unwrap_or_else(|error| panic!("header: {error}"));
        headers.insert(name.as_str().to_owned(), Value::from(text));
    }
    Json(json!({
        "method": parts.method.as_str(),
        "path": parts.uri.path(),
        "query": parts.uri.query(),
        "headers": headers,
        "body": String::from_utf8_lossy(&payload),
    }))
    .into_response()
}

/// Stream three server-sent events and close.
async fn sse(State(()): State<()>) -> Response {
    let stream = async_stream::stream! {
        for event in ["alpha", "beta", "gamma"] {
            yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(format!(
                "data: {event}\n\n"
            )));
        }
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Echo every WebSocket text frame back until the client hangs up.
async fn websocket(State(()): State<()>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|socket: WebSocket| async move {
        let mut socket = socket;
        while let Some(Ok(message)) = socket.recv().await {
            let Some(text) = message.into_text().ok() else {
                break;
            };
            if socket.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    })
}

/// An upstream spec for an `http` endpoint.
fn spec(rate_limit: Option<Value>) -> Value {
    let mut payload = json!({
        "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 0 }] },
        "protocol": PROTOCOL
    });
    if let Some(rate_limit) = rate_limit {
        payload["rate_limit"] = rate_limit;
    }
    payload
}

/// Send a request to the gateway and return the raw response.
async fn gateway(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> axum::http::Response<Incoming> {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://127.0.0.1:{port}/oagw/v1/proxy{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let payload = body.unwrap_or_default().as_bytes().to_vec();
    dial(
        builder
            .body(Body::from(payload))
            .unwrap_or_else(|error| panic!("gateway request: {error}")),
    )
    .await
}

/// Dial the gateway with the pooled client.
async fn dial(request: Request<Body>) -> axum::http::Response<Incoming> {
    let client: Client<HttpConnector, Body> = Client::builder(TokioExecutor::new()).build_http();
    client
        .request(request)
        .await
        .unwrap_or_else(|error| panic!("gateway call: {error}"))
}

/// Drain a response body and decode it as JSON.
async fn json_of(response: axum::http::Response<Incoming>) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap_or_else(|error| panic!("body: {error}"))
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json: {error}"))
}

/// Drain a response body and decode it as a problem document.
async fn problem_of(response: axum::http::Response<Incoming>) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap_or_else(|error| panic!("body: {error}"))
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("problem: {error}"))
}

fn header<'r>(response: &'r axum::http::Response<Incoming>, name: &str) -> &'r str {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// The config the transport tests run under: plaintext upstreams on a
/// loopback fabric, SSRF egress blocking off.
fn config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: crate::config::SsrfPolicy { enabled: false },
        ..OagwConfig::default()
    }
}

/// A config with the shortest timeout the contract allows.
fn quick_timeout() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 1,
        ..config()
    }
}

/// The shipped config: plaintext upstreams are refused.
fn shipped_config() -> OagwConfig {
    OagwConfig::default()
}

/// The transport config with SSRF egress blocking switched back on.
fn ssrf_config() -> OagwConfig {
    OagwConfig {
        ssrf_policy: crate::config::SsrfPolicy { enabled: true },
        ..config()
    }
}

#[tokio::test]
async fn a_get_is_forwarded_and_the_response_is_stamped_as_upstream() {
    let port = stack("echo", spec(None), &config()).await;
    let response = gateway(port, "GET", "/echo/echo?alpha=1", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-oagw-error-source"), "upstream");
    let body = json_of(response).await;
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/echo");
    assert_eq!(body["query"], Value::from("alpha=1"));
}

#[tokio::test]
async fn a_post_body_reaches_the_upstream() {
    let port = stack("echo", spec(None), &config()).await;
    let response = gateway(
        port,
        "POST",
        "/echo/echo",
        &[("content-type", "text/plain")],
        Some("payload"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_of(response).await;
    assert_eq!(body["method"], "POST");
    assert_eq!(body["body"], "payload");
}

#[tokio::test]
async fn header_rules_decide_what_reaches_the_upstream() {
    let port = stack("echo", spec(None), &config()).await;
    let response = gateway(
        port,
        "GET",
        "/echo/echo",
        &[("x-forwarded-for", "203.0.113.7"), ("x-secret", "nope")],
        None,
    )
    .await;
    let body = json_of(response).await;
    let headers = &body["headers"];
    // The default `passthrough: none` drops everything the configuration does
    // not name; the gateway re-authors `host` itself.
    assert_eq!(headers["x-forwarded-for"], Value::Null);
    assert!(
        headers["host"]
            .as_str()
            .is_some_and(|host| host.contains(':'))
    );
    assert_eq!(headers["transfer-encoding"], Value::Null);
}

#[tokio::test]
async fn an_event_stream_is_relayed_in_full() {
    let port = stack("echo", spec(None), &config()).await;
    let response = gateway(port, "GET", "/echo/sse", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8_lossy(
        &response
            .into_body()
            .collect()
            .await
            .unwrap_or_else(|error| panic!("sse: {error}"))
            .to_bytes(),
    )
    .into_owned();
    assert_eq!(text.matches("data: ").count(), 3);
    assert!(text.contains("data: alpha"));
    assert!(text.contains("data: gamma"));
}

#[tokio::test]
async fn a_websocket_upgrade_is_bridged_to_the_upstream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port = stack("echo", spec(None), &config()).await;
    let request = Request::builder()
        .method("GET")
        .uri(format!("http://127.0.0.1:{port}/oagw/v1/proxy/echo/ws"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("upgrade request: {error}"));
    let mut response = dial(request).await;
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .unwrap_or_else(|error| panic!("client upgrade: {error}"));
    let mut socket = hyper_util::rt::TokioIo::new(upgraded);
    socket
        .write_all(&text_frame(b"ping"))
        .await
        .unwrap_or_else(|error| panic!("frame write: {error}"));
    let mut echoed = [0_u8; 6];
    socket
        .read_exact(&mut echoed)
        .await
        .unwrap_or_else(|error| panic!("frame read: {error}"));
    assert_eq!(&echoed[..2], &[0x81, 0x04]);
    assert_eq!(&echoed[2..], b"ping");
}

/// One masked WebSocket text frame carrying `payload`.
fn text_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![
        0x81_u8,
        0x80_u8 | u8::try_from(payload.len()).unwrap_or_default(),
        0,
        0,
        0,
        0,
    ];
    frame.extend(payload.iter().copied());
    frame
}

#[tokio::test]
async fn a_declared_body_over_the_cap_is_rejected_with_413() {
    let port = stack("echo", spec(None), &config()).await;
    let response = gateway(
        port,
        "POST",
        "/echo/echo",
        &[("content-length", "104857601")],
        Some("small"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header(&response, "x-oagw-error-source"), "gateway");
    let problem = problem_of(response).await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[tokio::test]
async fn a_slow_upstream_times_out_with_504() {
    let config = quick_timeout();
    let slow = spawn(Router::new().route(
        "/slow",
        any(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "late"
        }),
    ))
    .await;
    let port = gateway_on("slow", upstream_port(&spec(None), slow), &config).await;
    let response = gateway(port, "GET", "/slow/slow", &[], None).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn a_refused_connection_is_a_503() {
    // Take a port, then release it: nothing is listening there any more.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let dead = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("addr: {error}"))
        .port();
    drop(listener);
    let port = gateway_on("dead", upstream_port(&spec(None), dead), &config()).await;
    let response = gateway(port, "GET", "/dead/echo", &[], None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, "x-oagw-error-source"), "gateway");
    let problem = problem_of(response).await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_from_the_gateway() {
    let port = stack("echo", spec(None), &config()).await;
    let response = gateway(port, "GET", "/nope/echo", &[], None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, "x-oagw-error-source"), "gateway");
    let problem = problem_of(response).await;
    assert_eq!(problem["context"]["alias"], "nope");
}

#[tokio::test]
async fn an_exhausted_bucket_is_a_429_with_retry_after() {
    let config = config();
    let port = stack(
        "limited",
        spec(Some(json!({
            "sustained": { "rate": 1, "window": "minute" },
            "response_headers": true
        }))),
        &config,
    )
    .await;
    let first = gateway(port, "GET", "/limited/echo", &[], None).await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = gateway(port, "GET", "/limited/echo", &[], None).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(second.headers().get("retry-after").is_some());
    assert_eq!(
        second
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_the_gate_is_closed() {
    let port = stack("plain", spec(None), &shipped_config()).await;
    let response = gateway(port, "GET", "/plain/echo", &[], None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, "x-oagw-error-source"), "gateway");
    let problem = problem_of(response).await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("allow_http_upstream"))
    );
}

#[tokio::test]
async fn an_ssrf_guarded_gateway_refuses_a_loopback_upstream() {
    let port = stack("loop", spec(None), &ssrf_config()).await;
    let response = gateway(port, "GET", "/loop/echo", &[], None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, "x-oagw-error-source"), "gateway");
    let problem = problem_of(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("forbidden network range"))
    );
}
