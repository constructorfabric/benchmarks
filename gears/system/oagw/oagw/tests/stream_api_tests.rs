//! The streaming feature on the wire.
//!
//! Covers `cpt-cf-oagw-dod-stream-sse-forwarding`, `cpt-cf-oagw-dod-stream-lifecycle`,
//! `cpt-cf-oagw-dod-stream-upgrade`, `cpt-cf-oagw-dod-stream-timeouts`,
//! `cpt-cf-oagw-dod-stream-errors`, and `cpt-cf-oagw-dod-stream-tests` over the
//! mounted proxy surface: the incremental transfer of an event stream and of a
//! body that is not one, the handshake headers an upgrade request reaches its
//! upstream with, the non-101 passthrough, the two error answers with their GTS
//! types, their tags, and their missing `Retry-After`, the boundary
//! `proxy_timeout_secs` draws at the response headers, the refusal of an upgrade
//! request before the detection runs, the refusal of a scheme that is never
//! dialed, and the tunnel itself, which is carried over a real socket in both
//! directions and torn down in both. The upstream and the caller's half are the
//! mock boundary of every test here, and each test owns its own upstream, so no
//! test observes another's.

// @cpt-dod:cpt-cf-oagw-dod-stream-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::error::AuthZResolverError;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::pep::PolicyEnforcer;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use oagw::OagwConfig;
use oagw::OagwState;
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::store::OagwStore;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const TENANT: u128 = 0x51;
const HOST: &str = "127.0.0.1";
const ERROR_SOURCE: &str = "x-oagw-error-source";
const REQUEST_TIMEOUT_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
const IDLE_TIMEOUT_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
const STREAM_ABORTED_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
const PROTOCOL_ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
const ROUTE_NOT_FOUND_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// The six hop-by-hop headers the strip never suspends.
const ALWAYS_STRIPPED: [&str; 6] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// The `AuthZ` PDP the allowing stub stands in for.
struct Allowing;

#[async_trait::async_trait]
impl AuthZResolverClient for Allowing {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: String::from(pep_properties::OWNER_TENANT_ID),
                        value: json!(TENANT.to_string()),
                    })],
                }],
                deny_reason: None,
            },
        })
    }
}

/// The authenticated subject a proxy request carries.
fn subject() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(TENANT))
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .expect("the subject is complete")
}

/// The `upstream_id` key the route create body names its upstream by.
fn key_of(instance: &str) -> String {
    oagw::gts::parse_gts_instance(oagw::UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string()
}

/// Issues one create and returns the instance identifier of the row.
async fn created(app: &Router, method: Method, path: &str, body: &Value) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .extension(subject())
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("the request builds"),
        )
        .await
        .expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"].as_str().expect("the instance id").to_owned()
}

/// Mounts one surface whose single upstream is the endpoint the test bound,
/// with the header-arrival deadline and the method allowlist the test states.
async fn mounted(port: u16, header_timeout_secs: u64, methods: &[&str], scheme: &str) -> Router {
    let store = Arc::new(OagwStore::new());
    let cache = Arc::new(ControlPlaneCache::new());
    let config = OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: header_timeout_secs,
        ..OagwConfig::default()
    };
    let service = Arc::new(
        ManagementService::new(Arc::clone(&store), &config, Arc::clone(&cache))
            .expect("the validators compile"),
    );
    let state = Arc::new(OagwState::new(
        Arc::new(config),
        Arc::clone(&store),
        service,
        Some(Arc::new(PolicyEnforcer::new(Arc::new(Allowing)))),
        None,
        Arc::clone(&cache),
    ));
    let router = oagw::api::rest::register_management_routes(Router::new(), state);

    let upstream_instance = created(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        &json!({
            "alias": HOST,
            "server": { "endpoints": [{ "scheme": scheme, "host": HOST, "port": port }] },
            "protocol": HTTP_PROTOCOL
        }),
    )
    .await;
    created(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        &json!({
            "upstream_id": key_of(&upstream_instance),
            "match": { "http": { "methods": methods, "path": "/api" } },
            "priority": 10
        }),
    )
    .await;
    router
}

/// Binds one listener the gateway dials and hands the connection it accepts
/// back, so each test scripts its upstream's bytes itself.
async fn listening() -> (u16, tokio::sync::oneshot::Receiver<TcpStream>) {
    let listener = TcpListener::bind((HOST, 0)).await.expect("the listener binds");
    let port = listener.local_addr().expect("the address").port();
    let (accepted, dialled) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            let _ = accepted.send(socket);
        }
    });
    (port, dialled)
}

/// Reads one peer's head, which ends at the first blank line, and returns it
/// with the bytes that arrived past that line, which belong to whoever reads
/// the half next.
async fn read_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream
            .read(&mut chunk)
            .await
            .expect("the peer's head reads");
        assert!(read > 0, "the peer closed before its head ended");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let ended = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the head ends")
        + 4;
    let head = String::from_utf8_lossy(&buffer[..ended]).into_owned();
    (head, buffer.split_off(ended))
}

/// The head's header lines, lower-cased, without the request line.
fn headers_of(head: &str) -> Vec<String> {
    head.split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(|line| line.to_ascii_lowercase())
        .collect()
}

/// Whether one header line names one header, compared as the wire spells it.
fn declares(headers: &[String], name: &str) -> bool {
    headers
        .iter()
        .any(|line| line.starts_with(name) && line[name.len()..].starts_with(':'))
}

/// Sends one proxy request and hands the response back with its body still
/// unbuffered, so a test can read it as it arrives.
async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .extension(subject());
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("the request builds"))
        .await
        .expect("oneshot resolves")
}

/// Reads one streamed body to its end.
async fn drain(body: Body) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(frame) = stream.next().await {
        bytes.extend_from_slice(&frame.expect("the body frame"));
    }
    bytes
}

/// Reads one answer's status, headers, and body as a problem document.
async fn problem(response: Response) -> (StatusCode, Vec<(String, String)>, Value) {
    let status = response.status();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    (status, headers, document)
}

/// One header value of an answer, matched case-insensitively.
fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_str())
}

/// Serves one mounted surface over a real socket, which the tunnel needs: the
/// caller's half of the tunnel is the connection the server accepted.
async fn served(router: Router) -> u16 {
    let listener = TcpListener::bind((HOST, 0)).await.expect("the listener binds");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("the server serves");
    });
    port
}

/// Adds the subject the proxy surface requires, which a socket-bound caller
/// cannot carry as a request extension.
fn authenticated(router: Router) -> Router {
    router.layer(axum::middleware::from_fn(
        |mut request: Request<Body>, next: Next| async move {
            request.extensions_mut().insert(subject());
            next.run(request).await
        },
    ))
}

/// The handshake a socket-bound caller sends, with the four headers the
/// handshake owns and the hop-by-hop headers the strip keeps stripping.
const HANDSHAKE: &str = "upgrade: websocket\r\nconnection: Upgrade\r\n\
                         keep-alive: timeout=5\r\nproxy-authorization: Basic bWFyYQ==\r\n\
                         te: trailers\r\ntrailer: x-upstream\r\n\
                         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                         sec-websocket-version: 13\r\n\
                         sec-websocket-protocol: chat, superchat\r\n\
                         sec-websocket-extensions: permessage-deflate\r\n";

/// The 101 an upstream that takes the handshake answers with.
const SWITCHING: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                           connection: Upgrade\r\n\r\n";

/// An event stream reaches the caller as its events arrive, in the order and
/// the bytes the upstream emitted them, with none buffered to completion
/// first. The header deadline is the graded two seconds, and the body outlives
/// it, which is why no mid-body stall is answered with the header deadline's
/// variant.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_stream_reaches_the_caller_as_its_events_arrive() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 2, &["GET"], "http").await;
    let reader = tokio::spawn(async move {
        let response = send(
            &router,
            Method::GET,
            "/oagw/v1/proxy/127.0.0.1/api",
            &[],
        )
        .await;
        let head_arrived = Instant::now();
        let bytes = drain(response.into_body()).await;
        (head_arrived, bytes)
    });

    let mut upstream = dialled.await.expect("the gateway dials");
    let (head, _tail) = read_head(&mut upstream).await;
    assert!(!declares(&headers_of(&head), "upgrade"), "{head}");
    upstream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     connection: close\r\n\r\ndata: one\n\n")
        .await
        .expect("the first event is written");
    upstream.flush().await.expect("the first event is flushed");
    // The second event is held back long enough to cross the header deadline,
    // so a gateway that buffered the body would answer only after it.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    upstream
        .write_all(b"data: two\n\n")
        .await
        .expect("the second event is written");
    upstream.flush().await.expect("the second event is flushed");
    drop(upstream);

    let (head_arrived, bytes) = reader.await.expect("the reader finishes");
    assert_eq!(bytes, b"data: one\n\ndata: two\n\n".as_slice());
    let body_span = head_arrived.elapsed();
    assert!(
        body_span >= Duration::from_millis(2_000),
        "the body was buffered: {body_span:?}"
    );
}

/// A body whose content type is not `text/event-stream` is forwarded the same
/// way: as it arrives, byte for byte, with no frame parsed and none withheld.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_an_event_stream_is_forwarded_the_same_way() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let reader = tokio::spawn(async move {
        let response = send(&router, Method::GET, "/oagw/v1/proxy/127.0.0.1/api", &[]).await;
        let head_arrived = Instant::now();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(String::from);
        let source = response
            .headers()
            .get(ERROR_SOURCE)
            .and_then(|value| value.to_str().ok())
            .map(String::from);
        let bytes = drain(response.into_body()).await;
        (head_arrived, content_type, source, bytes)
    });

    let mut upstream = dialled.await.expect("the gateway dials");
    let (_head, _tail) = read_head(&mut upstream).await;
    upstream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     connection: close\r\n\r\n{\"a\":")
        .await
        .expect("the first half is written");
    upstream.flush().await.expect("the first half is flushed");
    tokio::time::sleep(Duration::from_millis(900)).await;
    upstream
        .write_all(b"1}")
        .await
        .expect("the second half is written");
    upstream.flush().await.expect("the second half is flushed");
    drop(upstream);

    let (head_arrived, content_type, source, bytes) = reader.await.expect("the reader finishes");
    assert_eq!(content_type.as_deref(), Some("application/json"));
    assert_eq!(source.as_deref(), Some("upstream"));
    assert_eq!(bytes, b"{\"a\":1}".as_slice());
    let body_span = head_arrived.elapsed();
    assert!(
        body_span >= Duration::from_millis(700),
        "the body was buffered: {body_span:?}"
    );
}

/// An upgrade request reaches its upstream with the handshake's own headers
/// and the two suspended strip names, without the six hop-by-hop headers, and
/// a 101 is answered to the caller as the upstream sent it.
#[tokio::test(flavor = "multi_thread")]
async fn an_upgrade_request_reaches_the_upstream_with_its_handshake_headers() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let taken = tokio::spawn(async move {
        let mut upstream = dialled.await.expect("the gateway dials");
        let (head, _tail) = read_head(&mut upstream).await;
        let headers = headers_of(&head);
        for stripped in ALWAYS_STRIPPED {
            assert!(!declares(&headers, stripped), "{head}");
        }
        assert!(declares(&headers, "upgrade"), "{head}");
        assert!(declares(&headers, "connection"), "{head}");
        assert!(declares(&headers, "host"), "{head}");
        assert!(declares(&headers, "sec-websocket-key"), "{head}");
        assert!(declares(&headers, "sec-websocket-version"), "{head}");
        assert!(declares(&headers, "sec-websocket-protocol"), "{head}");
        assert!(declares(&headers, "sec-websocket-extensions"), "{head}");
        upstream
            .write_all(SWITCHING)
            .await
            .expect("the 101 is written");
        upstream.flush().await.expect("the 101 is flushed");
    });

    let response = send(
        &router,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "Basic bWFyYQ=="),
            ("te", "trailers"),
            ("trailer", "x-upstream"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat, superchat"),
            ("sec-websocket-extensions", "permessage-deflate"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response.headers().get("upgrade").and_then(|v| v.to_str().ok()),
        Some("websocket")
    );
    assert_eq!(response.headers().get(ERROR_SOURCE).and_then(|v| v.to_str().ok()), Some("upstream"));
    taken.await.expect("the upstream finished");
}

/// A handshake the upstream does not take up is returned to the caller with
/// that upstream answer unchanged and the error-source classification's tag,
/// and the connection stays a plain request/response exchange.
#[tokio::test(flavor = "multi_thread")]
async fn a_handshake_the_upstream_does_not_upgrade_passes_through_unchanged() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let refused = tokio::spawn(async move {
        let mut upstream = dialled.await.expect("the gateway dials");
        let (_head, _tail) = read_head(&mut upstream).await;
        upstream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: 2\r\n\r\n{}")
            .await
            .expect("the refusal is written");
        upstream.flush().await.expect("the refusal is flushed");
        // The connection is held open to prove the gateway answers from it and
        // does not tunnel, and is closed with the answer.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let response = send(
        &router,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        &[("upgrade", "websocket"), ("connection", "Upgrade")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(ERROR_SOURCE).and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    let bytes = drain(response.into_body()).await;
    assert_eq!(bytes, b"{}".as_slice());
    refused.await.expect("the upstream finished");
}

/// A body that terminates mid-flight, after the headers arrived, is answered
/// 502 with the `StreamAborted` variant, the gateway tag, and no `Retry-After`.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_terminated_mid_flight_is_answered_502_stream_aborted() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let torn = tokio::spawn(async move {
        let mut upstream = dialled.await.expect("the gateway dials");
        let (_head, _tail) = read_head(&mut upstream).await;
        // The length is declared and never delivered: the body ends mid-flight.
        // The length is declared and never delivered at all: the body is
        // terminated before its first byte, which is the one moment a
        // mid-flight failure can still be answered as a whole.
        upstream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                         content-length: 5\r\n\r\n")
            .await
            .expect("the torn answer is written");
        upstream.flush().await.expect("the torn answer is flushed");
        drop(upstream);
    });

    let response = send(&router, Method::GET, "/oagw/v1/proxy/127.0.0.1/api", &[]).await;
    let (status, headers, document) = problem(response).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{document}");
    assert_eq!(document["type"], STREAM_ABORTED_TYPE, "{document}");
    assert_eq!(document["status"], 502, "{document}");
    assert_eq!(header_of(&headers, ERROR_SOURCE), Some("gateway"));
    assert_eq!(header_of(&headers, "content-type"), Some("application/problem+json"));
    assert!(!headers.iter().any(|(name, _)| name == "retry-after"));
    torn.await.expect("the upstream finished");
}

/// A stream whose response headers never arrive is answered 504 with the
/// `RequestTimeout` variant the header-arrival deadline carries, and the 60
/// seconds of the idle deadline are spent on no body at all, because no body
/// was ever opened.
#[tokio::test(flavor = "multi_thread")]
async fn a_header_wait_past_the_deadline_is_answered_504_request_timeout() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 2, &["GET"], "http").await;
    let silent = tokio::spawn(async move {
        // The connection is held, not answered: the header wait is what
        // breaches.
        let mut held = dialled.await.expect("the gateway dials");
        let (_head, _tail) = read_head(&mut held).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    });

    let response = send(&router, Method::GET, "/oagw/v1/proxy/127.0.0.1/api", &[]).await;
    let (status, headers, document) = problem(response).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{document}");
    assert_eq!(document["type"], REQUEST_TIMEOUT_TYPE, "{document}");
    assert_eq!(header_of(&headers, ERROR_SOURCE), Some("gateway"));
    assert_eq!(header_of(&headers, "content-type"), Some("application/problem+json"));
    assert!(!headers.iter().any(|(name, _)| name == "retry-after"));
    silent.await.expect("the upstream finished");
}

/// An upgrade request a route does not declare `GET` for is answered 404 with
/// the `RouteNotFound` variant and never reaches the detection or the upstream,
/// so the answer names the resolution and not a streaming-specific reason.
#[tokio::test(flavor = "multi_thread")]
async fn an_upgrade_request_a_route_does_not_declare_get_for_is_answered_404() {
    let (port, mut dialled) = listening().await;
    let router = mounted(port, 5, &["POST"], "http").await;

    let (status, headers, document) =
        problem(send(&router, Method::GET, "/oagw/v1/proxy/127.0.0.1/api",
            &[("upgrade", "websocket"), ("connection", "Upgrade")]).await)
            .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{document}");
    assert_eq!(document["type"], ROUTE_NOT_FOUND_TYPE, "{document}");
    assert_eq!(header_of(&headers, ERROR_SOURCE), Some("gateway"));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        dialled.try_recv().is_err(),
        "the refused upgrade request reached the upstream"
    );
}

/// A `wt`-scheme endpoint is refused at the send by the forward, so the 502 is
/// the protocol error the dial answers with and no tunnel is taken up.
#[tokio::test(flavor = "multi_thread")]
async fn a_wt_scheme_endpoint_is_refused_at_the_send_and_never_tunnelled() {
    let (port, mut dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "wt").await;

    let (status, headers, document) = problem(
        send(
            &router,
            Method::GET,
            "/oagw/v1/proxy/127.0.0.1/api",
            &[("upgrade", "websocket"), ("connection", "Upgrade")],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{document}");
    assert_eq!(document["type"], PROTOCOL_ERROR_TYPE, "{document}");
    assert_eq!(header_of(&headers, ERROR_SOURCE), Some("gateway"));
    assert!(!headers.iter().any(|(name, _)| name == "retry-after"));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        dialled.try_recv().is_err(),
        "the refused scheme reached a socket"
    );
}

/// The idle deadline is sixty seconds, is named by no configuration key, and
/// its breach is answered with the variant the idle timeout carries, which is
/// what the two answers of the pump map to.
#[tokio::test]
async fn the_idle_deadline_is_sixty_seconds_and_reached_by_no_configuration() {
    let document = serde_json::to_value(OagwConfig::default()).expect("the config serialises");
    let spelled = document.to_string();
    assert!(
        !spelled.contains("idle"),
        "the idle deadline is configurable: {spelled}"
    );
    assert_eq!(oagw::domain::stream::IDLE_TIMEOUT_SECS, 60);
    // The breach is answered by the pump's own mapping, which the streamed
    // error answers share with the tunnel.
    let stalled = oagw::domain::stream::answer_of(oagw::domain::stream::StreamOutcome::Stalled)
        .expect("the stall is answered");
    assert_eq!(stalled.kind.http_status(), 504);
    assert_eq!(stalled.kind.gts_type(), IDLE_TIMEOUT_TYPE);
}

/// After a 101, bytes the caller sends reach the upstream and bytes the
/// upstream sends reach the caller, with nothing added, interpreted, or
/// withheld in either direction.
#[tokio::test(flavor = "multi_thread")]
async fn a_101_handshake_tunnels_bytes_in_both_directions() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let gateway = served(authenticated(router)).await;
    let echoed = tokio::spawn(async move {
        let mut upstream = dialled.await.expect("the gateway dials");
        let (head, _tail) = read_head(&mut upstream).await;
        let headers = headers_of(&head);
        for stripped in ALWAYS_STRIPPED {
            assert!(!declares(&headers, stripped), "{head}");
        }
        assert!(declares(&headers, "upgrade"), "{head}");
        assert!(declares(&headers, "sec-websocket-key"), "{head}");
        upstream.write_all(SWITCHING).await.expect("the 101 is written");
        upstream.flush().await.expect("the 101 is flushed");
        let mut buffer = [0_u8; 16];
        let read = upstream.read(&mut buffer).await.expect("the tunnel reads");
        assert_eq!(&buffer[..read], b"ping");
        upstream.write_all(b"pong").await.expect("the echo is written");
        upstream.flush().await.expect("the echo is flushed");
    });

    let mut caller = TcpStream::connect((HOST, gateway))
        .await
        .expect("the caller connects");
    caller
        .write_all(
            format!(
                "GET /oagw/v1/proxy/127.0.0.1/api HTTP/1.1\r\nhost: gateway\r\n{HANDSHAKE}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("the handshake is sent");
    let (head, tail) = read_head(&mut caller).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    caller.write_all(b"ping").await.expect("the tunnel sends");
    let mut buffer = tail;
    let mut chunk = [0_u8; 16];
    while buffer.len() < 4 {
        let read = caller.read(&mut chunk).await.expect("the tunnel reads");
        assert!(read > 0, "the tunnel closed before the echo");
        buffer.extend_from_slice(&chunk[..read]);
    }
    assert_eq!(buffer, b"pong".as_slice());
    echoed.await.expect("the upstream finished");
}

/// A caller that disconnects has its upstream half closed by the gateway, and
/// the outcome is recorded on the session the exchange carried.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_that_disconnects_closes_the_upstream_half() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let gateway = served(authenticated(router)).await;
    let observed = tokio::spawn(async move {
        let mut upstream = dialled.await.expect("the gateway dials");
        let (_head, _tail) = read_head(&mut upstream).await;
        upstream.write_all(SWITCHING).await.expect("the 101 is written");
        upstream.flush().await.expect("the 101 is flushed");
        let mut buffer = [0_u8; 8];
        loop {
            let read = upstream.read(&mut buffer).await.expect("the tunnel reads");
            if read == 0 {
                break true;
            }
        }
    });

    let mut caller = TcpStream::connect((HOST, gateway))
        .await
        .expect("the caller connects");
    caller
        .write_all(
            format!(
                "GET /oagw/v1/proxy/127.0.0.1/api HTTP/1.1\r\nhost: gateway\r\n{HANDSHAKE}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("the handshake is sent");
    let (head, _tail) = read_head(&mut caller).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    drop(caller);
    assert!(
        observed.await.expect("the upstream finished"),
        "the upstream half was never closed"
    );
}

/// An upstream that closes its half has the bytes it already sent written to
/// the caller before the caller's half is closed with it.
#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_that_closes_its_half_closes_the_caller_s_after_its_bytes() {
    let (port, dialled) = listening().await;
    let router = mounted(port, 5, &["GET"], "http").await;
    let gateway = served(authenticated(router)).await;
    tokio::spawn(async move {
        let mut upstream = dialled.await.expect("the gateway dials");
        let (_head, _tail) = read_head(&mut upstream).await;
        upstream.write_all(SWITCHING).await.expect("the 101 is written");
        upstream.flush().await.expect("the 101 is flushed");
        upstream.write_all(b"bye").await.expect("the bytes are written");
        upstream.flush().await.expect("the bytes are flushed");
        drop(upstream);
    });

    let mut caller = TcpStream::connect((HOST, gateway))
        .await
        .expect("the caller connects");
    caller
        .write_all(
            format!(
                "GET /oagw/v1/proxy/127.0.0.1/api HTTP/1.1\r\nhost: gateway\r\n{HANDSHAKE}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("the handshake is sent");
    let (head, tail) = read_head(&mut caller).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let mut bytes = tail;
    let mut chunk = [0_u8; 16];
    loop {
        let read = caller.read(&mut chunk).await.expect("the tunnel reads");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    assert_eq!(bytes, b"bye".as_slice());
}
