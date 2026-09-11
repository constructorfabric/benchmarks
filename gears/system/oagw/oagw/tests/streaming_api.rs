//! Integration tests for server-sent-event pass-through streaming and
//! WebSocket upgrade proxying (`cpt-cf-oagw-feature-streaming-proxy`).
//!
//! SSE detection/passthrough is exercised with `httpmock` where a fixed
//! response body suffices, and with a small local `axum` mock upstream
//! (bound to a real `TcpListener`) where genuinely incremental, timed
//! delivery must be observed. WebSocket upgrade proxying always needs a
//! real local server on both ends — the gateway itself (so the client-facing
//! upgrade goes through a real hyper connection carrying the `OnUpgrade`
//! extension) and the mock upstream — so those tests spawn both via
//! `axum::serve` and drive the gateway with `tokio_tungstenite::connect_async`
//! as a real WebSocket client.
//!
//! Every test is wrapped in a short `tokio::time::timeout` so a broken relay
//! fails fast instead of hanging the suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bytes::Bytes;
use futures_util::stream::unfold;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt as _;
use httpmock::MockServer;
use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::model::{
    Algorithm, Burst, Endpoint, HttpMatch, MatchConfig, PathSuffixMode, Protocol, RateLimitConfig,
    RateLimitScope, Route, RouteMethod, Scheme, ServerConfig, Sharing, Strategy, Sustained,
    Upstream, Window,
};
use oagw::state::ControlPlaneState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Shared gateway/router test scaffolding, mirroring `tests/proxy_api.rs`.
// ---------------------------------------------------------------------------

fn router_for(state: Arc<ControlPlaneState>, config: OagwConfig, tenant_id: Uuid) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.proxy_timeout_secs.max(1)))
        .build()
        .expect("client must build");
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("security context must build");

    register_routes(Router::new(), &openapi)
        .layer(Extension(state))
        .layer(Extension(Arc::new(config)))
        .layer(Extension(Arc::new(client)))
        .layer(Extension(ctx))
}

fn default_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

fn tcp_endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port: Some(port),
    }
}

fn upstream_with(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.to_owned(),
        tags: Vec::new(),
        server: ServerConfig { endpoints },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn simple_route(upstream_id: Uuid, path: &str, methods: &[RouteMethod]) -> Route {
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        tags: Vec::new(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        enabled: true,
        priority: 0,
    }
}

fn install(state: &ControlPlaneState, tenant_id: Uuid, upstream: Upstream, route: Route) {
    state
        .tenant(tenant_id)
        .upstreams
        .insert(upstream.id, upstream);
    state.tenant(tenant_id).routes.insert(route.id, route);
}

async fn send(router: Router, request: Request<Body>) -> Response {
    router
        .oneshot(request)
        .await
        .expect("router call must succeed")
}

/// Binds `app` to a real loopback port and serves it in the background,
/// returning the bound address. Needed wherever a real hyper connection is
/// required: WebSocket upgrades on either end, and a mid-flight raw-TCP
/// abort.
async fn spawn_server(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind must succeed");
    let addr = listener.local_addr().expect("local_addr must succeed");
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });
    addr
}

// ---------------------------------------------------------------------------
// SSE mock upstreams.
// ---------------------------------------------------------------------------

/// Emits `events` one after another, sleeping `delay` between successive
/// events (never before the first), so a real client can observe
/// incremental, timed delivery rather than a single buffered write.
fn delayed_event_stream(
    events: Vec<&'static str>,
    delay: Duration,
    produced: Arc<AtomicUsize>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    unfold(
        (events.into_iter(), true, produced),
        move |(mut iter, first, produced)| async move {
            if !first {
                tokio::time::sleep(delay).await;
            }
            let next = iter.next()?;
            produced.fetch_add(1, Ordering::SeqCst);
            Some((Ok(Bytes::from(next)), (iter, false, produced)))
        },
    )
}

async fn sse_incremental_handler(Extension(produced): Extension<Arc<AtomicUsize>>) -> Response {
    let events = vec![
        "event: tick\ndata: 0\n\n",
        "event: tick\ndata: 1\n\n",
        "event: tick\ndata: 2\n\n",
    ];
    let body = Body::from_stream(delayed_event_stream(
        events,
        Duration::from_millis(300),
        produced,
    ));
    ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn sse_incremental_app(produced: Arc<AtomicUsize>) -> Router {
    Router::new()
        .route("/events", any(sse_incremental_handler))
        .layer(Extension(produced))
}

/// Writes a valid `text/event-stream` response head plus one complete
/// chunk, then a truncated second chunk header before dropping the
/// connection outright — a genuine mid-flight abort at the transport level,
/// which a well-formed upstream (e.g. `httpmock` or a normal `axum` handler)
/// cannot easily reproduce since both always terminate the body cleanly.
async fn spawn_truncating_sse_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind must succeed");
    let addr = listener.local_addr().expect("local_addr must succeed");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0_u8; 1024];
            drop(socket.read(&mut buf).await);
            let payload = "event: first\ndata: hi\n\n";
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{payload}\r\n",
                payload.len()
            );
            // Declares a 16-byte chunk but sends only 3 bytes, then closes:
            // an unrecoverable truncation for the chunked decoder.
            response.push_str("10\r\nabc");
            drop(socket.write_all(response.as_bytes()).await);
            drop(socket.flush().await);
        }
        // Dropping `socket` here (end of scope) resets the connection.
    });
    addr
}

/// Writes a valid `text/event-stream` response head, then drops the
/// connection before a single body byte (not even a chunk-size line) is
/// written: a genuine abort between the response head arriving and any byte
/// reaching the caller, distinct from `spawn_truncating_sse_upstream`'s
/// mid-body truncation.
async fn spawn_headers_only_then_reset_sse_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind must succeed");
    let addr = listener.local_addr().expect("local_addr must succeed");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0_u8; 1024];
            drop(socket.read(&mut buf).await);
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            drop(socket.write_all(response.as_bytes()).await);
            drop(socket.flush().await);
        }
        // Dropping `socket` here (end of scope) resets the connection before
        // any chunk of the (declared-but-never-sent) body arrives.
    });
    addr
}

// ---------------------------------------------------------------------------
// SSE: detection, framing preservation, byte-identical incremental forwarding.
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-it-headers-first-01
// @cpt-begin:cpt-cf-oagw-dod-sse-framing-preserved:p1:inst-sse-it-headers-first-01
#[tokio::test]
async fn sse_response_headers_are_forwarded_with_content_type_preserved_and_no_content_length() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/stream");
            then.status(200)
                .header("content-type", "text/event-stream; charset=utf-8")
                .body("event: only\ndata: hi\n\n");
        });

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse.example.com",
            vec![tcp_endpoint(Scheme::Http, &server.host(), server.port())],
        );
        let route = simple_route(up.id, "/v1/stream", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let router = router_for(state, default_config(), tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse.example.com/v1/stream")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream; charset=utf-8")
        );
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::CONTENT_LENGTH)
        );
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream")
        );

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        assert_eq!(bytes.as_ref(), b"event: only\ndata: hi\n\n");
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-sse-framing-preserved:p1:inst-sse-it-headers-first-01
// @cpt-end:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-it-headers-first-01

// CODE2-F-002 regression: the streaming path must strip hop-by-hop response
// headers too, symmetric with the buffered path and the request direction.
#[tokio::test]
async fn hop_by_hop_response_headers_never_reach_the_client_on_the_streaming_path() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/stream");
            then.status(200)
                .header("content-type", "text/event-stream")
                .header("transfer-encoding", "chunked")
                .header("connection", "keep-alive")
                .body("event: only\ndata: hi\n\n");
        });

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse-hopresp.example.com",
            vec![tcp_endpoint(Scheme::Http, &server.host(), server.port())],
        );
        let route = simple_route(up.id, "/v1/stream", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let router = router_for(state, default_config(), tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-hopresp.example.com/v1/stream")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("transfer-encoding"));
        assert!(!response.headers().contains_key("connection"));
    })
    .await
    .expect("test must not hang");
}

// @cpt-begin:cpt-cf-oagw-dod-sse-incremental-forwarding:p1:inst-sse-it-incremental-order-01
// @cpt-begin:cpt-cf-oagw-dod-sse-upstream-close:p1:inst-sse-it-incremental-order-01
#[tokio::test]
async fn three_events_delivered_incrementally_and_in_order_before_upstream_finishes() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let produced = Arc::new(AtomicUsize::new(0));
        let upstream_addr = spawn_server(sse_incremental_app(Arc::clone(&produced))).await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse-incremental.example.com",
            vec![tcp_endpoint(
                Scheme::Http,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        let route = simple_route(up.id, "/events", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let mut config = default_config();
        config.proxy_timeout_secs = 5;
        let router = router_for(state, config, tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-incremental.example.com/events")
            .body(Body::empty())
            .unwrap();

        let start = std::time::Instant::now();
        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut body = response.into_body();
        let mut chunks: Vec<(Duration, Bytes)> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.expect("frame must read");
            if let Some(data) = frame.data_ref() {
                chunks.push((start.elapsed(), data.clone()));
            }
        }

        assert_eq!(chunks.len(), 3, "all three events must be delivered");
        assert_eq!(chunks[0].1.as_ref(), b"event: tick\ndata: 0\n\n");
        assert_eq!(chunks[1].1.as_ref(), b"event: tick\ndata: 1\n\n");
        assert_eq!(chunks[2].1.as_ref(), b"event: tick\ndata: 2\n\n");

        // The buffered path would deliver every event at once, only after
        // the upstream had produced (and delayed) all three; incremental
        // forwarding instead delivers the first well before that point.
        assert!(
            chunks[0].0 < Duration::from_millis(200),
            "first event must arrive before the upstream produced the last one, got {:?}",
            chunks[0].0
        );
        assert!(
            chunks[2].0 >= Duration::from_millis(550),
            "last event must only arrive after both delays elapsed, got {:?}",
            chunks[2].0
        );
        assert_eq!(produced.load(Ordering::SeqCst), 3);
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-sse-upstream-close:p1:inst-sse-it-incremental-order-01
// @cpt-end:cpt-cf-oagw-dod-sse-incremental-forwarding:p1:inst-sse-it-incremental-order-01

// @cpt-begin:cpt-cf-oagw-dod-sse-client-disconnect:p1:inst-sse-it-client-disconnect-01
#[tokio::test]
async fn a_client_disconnect_stops_the_upstream_from_producing_further_events() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let produced = Arc::new(AtomicUsize::new(0));
        let upstream_addr = spawn_server(sse_incremental_app(Arc::clone(&produced))).await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse-disconnect.example.com",
            vec![tcp_endpoint(
                Scheme::Http,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        let route = simple_route(up.id, "/events", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let mut config = default_config();
        config.proxy_timeout_secs = 5;
        let router = router_for(state, config, tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-disconnect.example.com/events")
            .body(Body::empty())
            .unwrap();

        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Read only the first event, then drop the response body entirely —
        // the client-disconnect case — instead of draining the stream.
        let mut body = response.into_body();
        let first = body
            .frame()
            .await
            .expect("first frame must read")
            .expect("must read");
        assert!(first.data_ref().is_some());
        drop(body);

        // Give the dropped connection time to propagate to the upstream;
        // it must never reach the third event.
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            produced.load(Ordering::SeqCst) < 3,
            "the upstream must stop producing events once the client disconnects"
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-sse-client-disconnect:p1:inst-sse-it-client-disconnect-01

// @cpt-begin:cpt-cf-oagw-dod-sse-abort-handling:p1:inst-sse-it-preheader-failure-01
#[tokio::test]
async fn an_upstream_failure_before_headers_yields_a_gateway_sourced_502() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        // Port 1 is privileged and virtually never bound, so the connection
        // is refused before any response head is ever produced.
        let up = upstream_with(
            "sse-closed.example.com",
            vec![tcp_endpoint(Scheme::Http, "127.0.0.1", 1)],
        );
        let route = simple_route(up.id, "/v1/stream", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let router = router_for(state, default_config(), tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-closed.example.com/v1/stream")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-sse-abort-handling:p1:inst-sse-it-preheader-failure-01

// @cpt-begin:cpt-cf-oagw-dod-sse-abort-handling:p1:inst-sse-it-midflight-abort-01
#[tokio::test]
async fn an_upstream_abort_after_headers_truncates_the_stream_with_no_problem_body() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let upstream_addr = spawn_truncating_sse_upstream().await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse-abort.example.com",
            vec![tcp_endpoint(
                Scheme::Http,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        let route = simple_route(up.id, "/v1/stream", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let router = router_for(state, default_config(), tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-abort.example.com/v1/stream")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        // The status was already committed to `200` before the abort could
        // be known, so it must stay `200` — the caller only ever observes a
        // truncated body, never a problem-details document.
        assert_eq!(response.status(), StatusCode::OK);

        let mut body = response.into_body();
        let mut collected = Vec::new();
        while let Some(Ok(frame)) = body.frame().await {
            if let Some(data) = frame.data_ref() {
                collected.extend_from_slice(data);
            }
        }
        assert!(collected.starts_with(b"event: first"));
        assert!(
            !collected.ends_with(b"\n\n"),
            "the body must be visibly truncated, not a clean event boundary"
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-sse-abort-handling:p1:inst-sse-it-midflight-abort-01

// CONS-F-001 regression: a stream abort that happens after the response
// head arrived (and was recognised as an event stream) but before any body
// byte reached the caller must surface as the documented `StreamAborted`
// (502), distinct from `an_upstream_failure_before_headers_yields_a_gateway_sourced_502`'s
// pre-header `downstream.error.v1` classification above.
#[tokio::test]
async fn an_abort_between_headers_and_the_first_byte_yields_the_documented_stream_aborted_type() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let upstream_addr = spawn_headers_only_then_reset_sse_upstream().await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse-stream-aborted.example.com",
            vec![tcp_endpoint(
                Scheme::Http,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        let route = simple_route(up.id, "/v1/stream", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let router = router_for(state, default_config(), tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-stream-aborted.example.com/v1/stream")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
        );
    })
    .await
    .expect("test must not hang");
}

// @cpt-begin:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-sse-it-timeout-window-01
#[tokio::test]
async fn an_sse_stream_stays_open_past_the_configured_proxy_timeout() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let produced = Arc::new(AtomicUsize::new(0));
        let upstream_addr = spawn_server(sse_incremental_app(Arc::clone(&produced))).await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "sse-timeout.example.com",
            vec![tcp_endpoint(
                Scheme::Http,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        let route = simple_route(up.id, "/events", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        // A proxy timeout far shorter than the ~600ms it takes the mock
        // upstream to finish producing all three events: only the pre-stream
        // phase may be bound by it.
        let mut config = default_config();
        config.proxy_timeout_secs = 1;
        let router = router_for(state, config, tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-timeout.example.com/events")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        assert_eq!(
            bytes.as_ref(),
            b"event: tick\ndata: 0\n\nevent: tick\ndata: 1\n\nevent: tick\ndata: 2\n\n" as &[u8]
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-sse-it-timeout-window-01

// ---------------------------------------------------------------------------
// WebSocket mock upstreams.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EchoState {
    handshake_headers: Mutex<Option<HeaderMap>>,
    received_close_code: AtomicU16,
    upstream_saw_client_close: Mutex<bool>,
}

async fn ws_echo_handler(
    Extension(state): Extension<Arc<EchoState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    *state
        .handshake_headers
        .lock()
        .expect("lock must not be poisoned") = Some(headers);
    ws.on_upgrade(move |socket| handle_echo(socket, state))
}

async fn handle_echo(mut socket: WebSocket, state: Arc<EchoState>) {
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            Message::Close(frame) => {
                if let Some(frame) = &frame {
                    state
                        .received_close_code
                        .store(frame.code, Ordering::SeqCst);
                }
                *state
                    .upstream_saw_client_close
                    .lock()
                    .expect("lock must not be poisoned") = true;
                drop(socket.send(Message::Close(frame)).await);
                break;
            }
            Message::Text(text) if text.as_str() == "please-close-1001" => {
                drop(
                    socket
                        .send(Message::Close(Some(CloseFrame {
                            code: 1001,
                            reason: "server done".into(),
                        })))
                        .await,
                );
                break;
            }
            Message::Text(text) if text.as_str() == "please-disconnect" => {
                // Drop the socket without any close frame: an abrupt
                // upstream-initiated drop.
                return;
            }
            other => {
                if socket.send(other).await.is_err() {
                    break;
                }
            }
        }
    }
    *state
        .upstream_saw_client_close
        .lock()
        .expect("lock must not be poisoned") = true;
}

fn ws_echo_app(state: Arc<EchoState>) -> Router {
    Router::new()
        .route("/ws", any(ws_echo_handler))
        .layer(Extension(state))
}

async fn ws_reject_handler() -> StatusCode {
    StatusCode::NOT_FOUND
}

fn ws_reject_app() -> Router {
    Router::new().route("/ws", any(ws_reject_handler))
}

/// Installs a `ws.example.com` upstream/route pair pointing at `upstream_addr`
/// and serves the gateway on a real socket, returning its address.
async fn spawn_gateway_with_ws_upstream(
    alias: &str,
    scheme: Scheme,
    upstream_addr: SocketAddr,
    allow_http_upstream: bool,
) -> SocketAddr {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        alias,
        vec![tcp_endpoint(
            scheme,
            &upstream_addr.ip().to_string(),
            upstream_addr.port(),
        )],
    );
    let route = simple_route(up.id, "/ws", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let mut config = default_config();
    config.allow_http_upstream = allow_http_upstream;
    let router = router_for(state, config, tenant_id);
    spawn_server(router).await
}

fn ws_url(gateway_addr: SocketAddr, alias: &str) -> String {
    format!("ws://{gateway_addr}/oagw/v1/proxy/{alias}/ws")
}

/// A response-mutating `axum::middleware::from_fn` layer shaped exactly like
/// `crate::api::rest::routes::ensure_error_source_header` — the layer the
/// composed-server bug report first suspected of breaking the upgrade
/// handoff (it awaits `next.run`, then mutates only response headers, never
/// the body). Wrapping the router in this layer for a real-TCP-listener
/// WebSocket test exercises exactly the interference the router-level tests
/// otherwise miss by never applying any router `.layer(...)`.
async fn response_header_stamping_middleware(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("x-oagw-test-middleware", HeaderValue::from_static("seen"));
    response
}

/// Installs a `ws.example.com` upstream/route pair pointing at `upstream_addr`
/// and serves the gateway on a real socket with an extra response-mutating
/// middleware layered over the whole router, returning its address.
async fn spawn_gateway_with_ws_upstream_and_middleware(
    alias: &str,
    scheme: Scheme,
    upstream_addr: SocketAddr,
) -> SocketAddr {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        alias,
        vec![tcp_endpoint(
            scheme,
            &upstream_addr.ip().to_string(),
            upstream_addr.port(),
        )],
    );
    let route = simple_route(up.id, "/ws", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id)
        .layer(middleware::from_fn(response_header_stamping_middleware));
    spawn_server(router).await
}

/// A raw-TCP WebSocket upstream that always answers `101` and accepts
/// whatever `Sec-WebSocket-Extensions` the client offered, echoing it back
/// unchanged — mimicking a real upstream (e.g. `aiohttp`'s default
/// `compress=True` `WebSocketResponse`) that negotiates `permessage-deflate`
/// the moment it is offered, regardless of whether the party relaying
/// frames on its behalf can actually decode that extension.
async fn spawn_extension_accepting_ws_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind must succeed");
    let addr = listener.local_addr().expect("local_addr must succeed");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0_u8; 4096];
            drop(socket.read(&mut buf).await);
            // The well-known RFC 6455 `Sec-WebSocket-Key`/`-Accept` pair,
            // matching `ws_request_with_extension` below.
            let response = "HTTP/1.1 101 Switching Protocols\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
                 Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n";
            drop(socket.write_all(response.as_bytes()).await);
            drop(socket.flush().await);
        }
    });
    addr
}

/// A hand-built client handshake request offering `permessage-deflate` —
/// `tokio_tungstenite::connect_async`'s own request-building path never
/// offers any extension, so reproducing a client that does (as real
/// WebSocket client libraries commonly do by default) requires building the
/// request by hand and relying on `http::Request<()>`'s `IntoClientRequest`
/// impl, which is documented to pass the request through unaltered.
fn ws_request_with_extension(gateway_addr: SocketAddr, alias: &str) -> Request<()> {
    Request::builder()
        .method("GET")
        .uri(ws_url(gateway_addr, alias))
        .header("host", gateway_addr.to_string())
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-extensions", "permessage-deflate")
        .body(())
        .expect("request must build")
}

// ---------------------------------------------------------------------------
// WebSocket: recognition, upstream-first negotiation, frame relay, close
// propagation, refusal handling.
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-ws-it-handshake-headers-01
// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-it-handshake-headers-01
// @cpt-begin:cpt-cf-oagw-dod-ws-frame-relay:p1:inst-ws-it-text-binary-01
#[tokio::test]
async fn text_and_binary_frames_are_echoed_with_identical_bytes_and_the_handshake_survives() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr =
            spawn_gateway_with_ws_upstream("ws-echo.example.com", Scheme::Ws, upstream_addr, true)
                .await;

        let (mut client, response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-echo.example.com"))
                .await
                .expect("client upgrade must succeed only after the upstream accepted");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        client
            .send(TungsteniteMessage::Text("hello".into()))
            .await
            .expect("send must succeed");
        let echoed = client
            .next()
            .await
            .expect("must receive")
            .expect("must be Ok");
        assert_eq!(echoed, TungsteniteMessage::Text("hello".into()));

        let binary_payload = vec![1_u8, 2, 3, 4, 250];
        client
            .send(TungsteniteMessage::Binary(binary_payload.clone().into()))
            .await
            .expect("send must succeed");
        let echoed_binary = client
            .next()
            .await
            .expect("must receive")
            .expect("must be Ok");
        assert_eq!(
            echoed_binary,
            TungsteniteMessage::Binary(binary_payload.into())
        );

        client.close(None).await.ok();

        let recorded = echo_state
            .handshake_headers
            .lock()
            .expect("lock must not be poisoned")
            .clone()
            .expect("handshake headers must have been recorded");
        assert_eq!(
            recorded
                .get("connection")
                .and_then(|v| v.to_str().ok())
                .map(str::to_ascii_lowercase),
            Some("upgrade".to_owned())
        );
        assert_eq!(
            recorded
                .get("upgrade")
                .and_then(|v| v.to_str().ok())
                .map(str::to_ascii_lowercase),
            Some("websocket".to_owned())
        );
        assert!(recorded.contains_key("sec-websocket-key"));
        assert_eq!(
            recorded
                .get("sec-websocket-version")
                .and_then(|v| v.to_str().ok()),
            Some("13")
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-frame-relay:p1:inst-ws-it-text-binary-01
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-it-handshake-headers-01
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-ws-it-handshake-headers-01

/// Regression test for the composed-server relay failure reported against
/// the real server: the client-facing router-level tests above never apply
/// any `.layer(...)` to the router at all, so they could never have caught
/// interference from a response-mutating middleware such as
/// `ensure_error_source_header`. This drives a real upgrade, over a real
/// `TcpListener`, through a router wrapped in exactly that shape of
/// middleware, and still expects a text AND a binary frame relayed cleanly
/// in both directions — proving (rather than assuming) that such a layer
/// does not interfere with the `OnUpgrade` handoff.
#[tokio::test]
async fn a_response_mutating_middleware_layered_over_the_router_does_not_break_the_upgrade() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr = spawn_gateway_with_ws_upstream_and_middleware(
            "ws-mw.example.com",
            Scheme::Ws,
            upstream_addr,
        )
        .await;

        let (mut client, response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-mw.example.com"))
                .await
                .expect(
                    "client upgrade must succeed with a response-mutating middleware layered \
                     over the router",
                );
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        client
            .send(TungsteniteMessage::Text("hello".into()))
            .await
            .expect("send must succeed");
        let echoed = client
            .next()
            .await
            .expect("must receive")
            .expect("must be Ok");
        assert_eq!(echoed, TungsteniteMessage::Text("hello".into()));

        let binary_payload = vec![9_u8, 8, 7, 6, 5];
        client
            .send(TungsteniteMessage::Binary(binary_payload.clone().into()))
            .await
            .expect("send must succeed");
        let echoed_binary = client
            .next()
            .await
            .expect("must receive")
            .expect("must be Ok");
        assert_eq!(
            echoed_binary,
            TungsteniteMessage::Binary(binary_payload.into())
        );

        client.close(None).await.ok();
    })
    .await
    .expect("test must not hang");
}

/// Regression test for the actual root cause of the composed-server relay
/// failure: an upstream that accepts the caller's forwarded
/// `Sec-WebSocket-Extensions` offer (`cpt-cf-oagw-dod-ws-upgrade-headers-survive`
/// requires forwarding it unchanged) is free to start using that extension,
/// which `relay` never implements a codec for — this drove a real upstream,
/// over a real `TcpListener`, that accepts `permessage-deflate` on the
/// handshake exactly as a default-configured `aiohttp` upstream does, and
/// confirms the gateway now refuses the upgrade up front instead of
/// completing a `101` that then silently breaks the first time the
/// extension is actually used.
#[tokio::test]
async fn an_upstream_accepting_the_forwarded_extension_offer_is_refused_end_to_end() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let upstream_addr = spawn_extension_accepting_ws_upstream().await;
        let gateway_addr =
            spawn_gateway_with_ws_upstream("ws-ext.example.com", Scheme::Ws, upstream_addr, true)
                .await;

        let request = ws_request_with_extension(gateway_addr, "ws-ext.example.com");
        let error = tokio_tungstenite::connect_async(request).await.expect_err(
            "an upstream accepting an extension this relay cannot honor must be refused, not \
             silently upgraded and then broken mid-session",
        );

        match error {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                assert_eq!(
                    response
                        .headers()
                        .get("x-oagw-error-source")
                        .and_then(|v| v.to_str().ok()),
                    Some("gateway")
                );
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }
    })
    .await
    .expect("test must not hang");
}

// @cpt-begin:cpt-cf-oagw-dod-ws-frame-relay:p1:inst-ws-it-ping-pong-01
#[tokio::test]
async fn a_ping_frame_is_relayed_and_a_pong_frame_is_relayed_back() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr =
            spawn_gateway_with_ws_upstream("ws-ping.example.com", Scheme::Ws, upstream_addr, true)
                .await;

        let (mut client, _response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-ping.example.com"))
                .await
                .expect("upgrade must succeed");

        client
            .send(TungsteniteMessage::Ping(Bytes::from_static(
                b"ping-payload",
            )))
            .await
            .expect("send must succeed");

        let mut saw_matching_pong = false;
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_secs(2), client.next()).await {
                Ok(Some(Ok(TungsteniteMessage::Pong(payload)))) => {
                    if payload.as_ref() == b"ping-payload" {
                        saw_matching_pong = true;
                        break;
                    }
                }
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        assert!(saw_matching_pong, "a matching pong must be relayed back");
        client.close(None).await.ok();
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-frame-relay:p1:inst-ws-it-ping-pong-01

// @cpt-begin:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-it-client-close-code-01
#[tokio::test]
async fn a_client_close_with_code_1000_is_delivered_to_the_upstream_and_both_halves_release() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr =
            spawn_gateway_with_ws_upstream("ws-close.example.com", Scheme::Ws, upstream_addr, true)
                .await;

        let (mut client, _response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-close.example.com"))
                .await
                .expect("upgrade must succeed");

        client
            .send(TungsteniteMessage::Close(Some(
                tokio_tungstenite::tungstenite::protocol::frame::CloseFrame {
                    code: 1000.into(),
                    reason: "bye".into(),
                },
            )))
            .await
            .expect("send close must succeed");

        // Drain until the connection ends; the peer's close reply and/or
        // stream end are both acceptable terminal outcomes here.
        while (tokio::time::timeout(Duration::from_secs(2), client.next()).await)
            .is_ok_and(|item| item.is_some())
        {}

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(echo_state.received_close_code.load(Ordering::SeqCst), 1000);
        assert!(
            *echo_state
                .upstream_saw_client_close
                .lock()
                .expect("lock must not be poisoned")
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-it-client-close-code-01

// @cpt-begin:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-it-upstream-close-code-01
#[tokio::test]
async fn an_upstream_close_with_code_1001_is_delivered_to_the_caller() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr = spawn_gateway_with_ws_upstream(
            "ws-upclose.example.com",
            Scheme::Ws,
            upstream_addr,
            true,
        )
        .await;

        let (mut client, _response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-upclose.example.com"))
                .await
                .expect("upgrade must succeed");

        client
            .send(TungsteniteMessage::Text("please-close-1001".into()))
            .await
            .expect("send must succeed");

        let mut observed_code = None;
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_secs(2), client.next()).await {
                Ok(Some(Ok(TungsteniteMessage::Close(Some(frame))))) => {
                    observed_code = Some(u16::from(frame.code));
                    break;
                }
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        assert_eq!(observed_code, Some(1001));
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-it-upstream-close-code-01

// @cpt-begin:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-it-abrupt-drops-01
#[tokio::test]
async fn an_abrupt_upstream_drop_without_a_close_frame_closes_the_caller_half() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr = spawn_gateway_with_ws_upstream(
            "ws-updrop.example.com",
            Scheme::Ws,
            upstream_addr,
            true,
        )
        .await;

        let (mut client, _response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-updrop.example.com"))
                .await
                .expect("upgrade must succeed");

        client
            .send(TungsteniteMessage::Text("please-disconnect".into()))
            .await
            .expect("send must succeed");

        let outcome = tokio::time::timeout(Duration::from_secs(3), client.next()).await;
        // The caller half must be torn down: either the stream ends
        // (`None`), the underlying connection errors, or the gateway's own
        // teardown sends a close frame while releasing the half. Any of
        // these means the caller is no longer left waiting on a half-closed
        // link; a further data frame would not.
        match outcome {
            Ok(None | Some(Err(_) | Ok(TungsteniteMessage::Close(_)))) | Err(_) => {}
            Ok(Some(Ok(message))) => panic!("expected the caller half to close, got {message:?}"),
        }
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-it-abrupt-drops-01

/// The other half of `cpt-cf-oagw-dod-ws-close-propagation`'s abrupt-drop
/// criterion: dropping the *caller's* TCP connection without a close frame
/// must close the *upstream* half. Observed via `EchoState`'s
/// `upstream_saw_client_close` flag, which `handle_echo` sets once its
/// `socket.recv()` loop ends for any reason (including an abrupt peer
/// disconnect).
#[tokio::test]
async fn an_abrupt_caller_drop_without_a_close_frame_closes_the_upstream_half() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;
        let gateway_addr = spawn_gateway_with_ws_upstream(
            "ws-callerdrop.example.com",
            Scheme::Ws,
            upstream_addr,
            true,
        )
        .await;

        let (mut client, _response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-callerdrop.example.com"))
                .await
                .expect("upgrade must succeed");

        // Round-trip one frame first, so the upstream half is definitely
        // established before the abrupt drop below.
        client
            .send(TungsteniteMessage::Text("hello".into()))
            .await
            .expect("send must succeed");

        // An abrupt drop: the underlying TCP connection is closed with no
        // WebSocket close frame sent at all.
        drop(client);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if *echo_state
                .upstream_saw_client_close
                .lock()
                .expect("lock must not be poisoned")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the upstream half must close after an abrupt caller drop"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("test must not hang");
}

// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-it-non-101-rejection-01
#[tokio::test]
async fn an_upstream_refusing_the_handshake_yields_a_gateway_sourced_502() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let upstream_addr = spawn_server(ws_reject_app()).await;
        let gateway_addr = spawn_gateway_with_ws_upstream(
            "ws-reject.example.com",
            Scheme::Ws,
            upstream_addr,
            true,
        )
        .await;

        let error = tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-reject.example.com"))
            .await
            .expect_err("the upstream's 404 must surface as a refused upgrade");

        match error {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                assert_eq!(
                    response
                        .headers()
                        .get("x-oagw-error-source")
                        .and_then(|v| v.to_str().ok()),
                    Some("gateway")
                );
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-it-non-101-rejection-01

// @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-ws-it-plaintext-disabled-01
#[tokio::test]
async fn a_ws_endpoint_is_refused_with_503_link_unavailable_when_plaintext_is_disabled() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        // Plaintext is refused before any connection is attempted, so this
        // address need not even be reachable.
        let unreachable = "127.0.0.1:1".parse().expect("valid socket addr");
        let gateway_addr = spawn_gateway_with_ws_upstream(
            "ws-plaintext.example.com",
            Scheme::Ws,
            unreachable,
            false,
        )
        .await;

        let error =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-plaintext.example.com"))
                .await
                .expect_err("plaintext ws must be refused when disabled");

        match error {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(
                    response
                        .headers()
                        .get("x-oagw-error-source")
                        .and_then(|v| v.to_str().ok()),
                    Some("gateway")
                );
                let body = response.body().clone().unwrap_or_default();
                let json: serde_json::Value = serde_json::from_slice(&body).expect("must be JSON");
                assert_eq!(
                    json["type"],
                    "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
                );
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-ws-it-plaintext-disabled-01

// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-it-wt-scheme-01
#[tokio::test]
async fn a_wt_scheme_endpoint_yields_a_502_protocol_error_confirming_webtransport_is_unimplemented()
{
    tokio::time::timeout(TEST_TIMEOUT, async {
        let placeholder = "127.0.0.1:1".parse().expect("valid socket addr");
        let gateway_addr =
            spawn_gateway_with_ws_upstream("ws-wt.example.com", Scheme::Wt, placeholder, true)
                .await;

        let error = tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-wt.example.com"))
            .await
            .expect_err("a wt endpoint must never be upgraded");

        match error {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                assert_eq!(
                    response
                        .headers()
                        .get("x-oagw-error-source")
                        .and_then(|v| v.to_str().ok()),
                    Some("gateway")
                );
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-it-wt-scheme-01

// @cpt-begin:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-ws-it-guard-before-upgrade-01
#[tokio::test]
async fn a_guard_violation_is_rejected_before_any_upgrade_is_negotiated() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "ws-guard.example.com",
            vec![tcp_endpoint(
                Scheme::Ws,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        // Only POST is allowed, so a GET upgrade request never matches.
        let route = simple_route(up.id, "/ws", &[RouteMethod::Post]);
        install(&state, tenant_id, up, route);

        let router = router_for(state, default_config(), tenant_id);
        let gateway_addr = spawn_server(router).await;

        let error = tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-guard.example.com"))
            .await
            .expect_err("a method the route does not allow must be rejected before any upgrade");

        match error {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }
        assert!(
            !*echo_state
                .upstream_saw_client_close
                .lock()
                .expect("lock must not be poisoned"),
            "the upstream must never have been contacted"
        );
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-ws-it-guard-before-upgrade-01

// CODE2-F-003 regression: the WebSocket pipeline must run the request-phase
// plugin/rate-limit hook before endpoint selection, so an exhausted rate
// limit is reported instead of a target-host rejection — the same
// precedence `run_proxy_pipeline` gives the plain-HTTP path.
#[tokio::test]
async fn a_rate_limit_rejection_takes_precedence_over_a_target_host_rejection_on_the_ws_path() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let mut up = upstream_with(
            "ws-precedence.example.com",
            vec![
                tcp_endpoint(Scheme::Ws, "us.ws-precedence.example.com", 80),
                tcp_endpoint(Scheme::Ws, "eu.ws-precedence.example.com", 80),
            ],
        );
        up.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: Algorithm::TokenBucket,
            sustained: Sustained {
                rate: 1,
                window: Window::Minute,
            },
            burst: Some(Burst { capacity: 1 }),
            scope: RateLimitScope::Global,
            strategy: Strategy::Reject,
            cost: 1,
        });
        let route = simple_route(up.id, "/ws", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        let router = router_for(Arc::clone(&state), default_config(), tenant_id);
        let gateway_addr = spawn_server(router).await;

        // First attempt: admitted by the (single-token) rate limiter, then
        // rejected for the missing `X-OAGW-Target-Host` header against this
        // common-suffix, multi-endpoint pool — both the old and new
        // ordering agree here.
        let first =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-precedence.example.com"))
                .await
                .expect_err("a common-suffix pool with no target host must be rejected");
        match first {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }

        // Second attempt: the single-token bucket is now exhausted. The
        // pre-fix ordering ran endpoint selection before the request hook,
        // so it would still report the target-host rejection every time,
        // never seeing the exhausted bucket; the fixed ordering must report
        // the rate-limit rejection first.
        let second =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-precedence.example.com"))
                .await
                .expect_err("the exhausted bucket must reject the second attempt");
        match second {
            TungsteniteError::Http(response) => {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            }
            other => panic!("expected an HTTP handshake rejection, got {other:?}"),
        }
    })
    .await
    .expect("test must not hang");
}

// @cpt-begin:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-ws-it-timeout-window-01
/// The WebSocket counterpart of `an_sse_stream_stays_open_past_the_configured_proxy_timeout`:
/// a proxy timeout far shorter than the session's actual lifetime must not
/// tear an established, actively-used WebSocket session down.
#[tokio::test]
async fn a_ws_session_stays_open_past_the_configured_proxy_timeout() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let echo_state = Arc::new(EchoState::default());
        let upstream_addr = spawn_server(ws_echo_app(Arc::clone(&echo_state))).await;

        let state = Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let up = upstream_with(
            "ws-timeout.example.com",
            vec![tcp_endpoint(
                Scheme::Ws,
                &upstream_addr.ip().to_string(),
                upstream_addr.port(),
            )],
        );
        let route = simple_route(up.id, "/ws", &[RouteMethod::Get]);
        install(&state, tenant_id, up, route);

        // A proxy timeout far shorter than the time this test spends
        // actively using the session below.
        let mut config = default_config();
        config.proxy_timeout_secs = 1;
        let router = router_for(state, config, tenant_id);
        let gateway_addr = spawn_server(router).await;

        let (mut client, response) =
            tokio_tungstenite::connect_async(ws_url(gateway_addr, "ws-timeout.example.com"))
                .await
                .expect("upgrade must succeed");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        // Outlive the 1-second proxy timeout window, then prove data still
        // flows in both directions over the same, still-open session.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        client
            .send(TungsteniteMessage::Text("still-alive".into()))
            .await
            .expect("send must succeed after the timeout window has elapsed");
        let echoed = client
            .next()
            .await
            .expect("must receive")
            .expect("must be Ok");
        assert_eq!(echoed, TungsteniteMessage::Text("still-alive".into()));
    })
    .await
    .expect("test must not hang");
}
// @cpt-end:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-ws-it-timeout-window-01
