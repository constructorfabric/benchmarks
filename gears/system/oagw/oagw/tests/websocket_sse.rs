//! Streaming tests (PRD §5.4 `cpt-cf-oagw-fr-streaming`, DESIGN §3.3 "Proxy
//! API", DESIGN §5.1, ADR-0007): the WebSocket handshake the proxy splices
//! through, and the SSE responses it passes through without buffering.
//!
//! DESIGN.md has no streaming chapter of its own to cite — see the note in the
//! [`oagw::streaming`] module documentation — so every citation here names an
//! anchor that does exist.
//!
//! Every end here is a real socket, because both of these are *connection
//! lifecycle* behaviours and a `oneshot` call cannot show them:
//!
//! * the **gateway** is the composed proxy router served by `axum::serve`, which
//!   is what the host binary runs (`serve_connection_with_upgrades`), so
//!   client-side upgrades are actually delivered to the handler;
//! * the **caller** is either `tokio-tungstenite`'s client — the only way to
//!   observe that a handshake was completed and that frames flow afterwards — or
//!   a hand-rolled HTTP/1.1 client for the answers that are *not* an upgrade;
//! * the **upstream** is a `TcpListener` serving either a WebSocket handshake
//!   (`tokio-tungstenite::accept_hdr_async`) or a chunked `text/event-stream`
//!   written by hand, so a test can see the exact bytes the gateway forwarded and
//!   flush in the middle of a body.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Extension;
use axum::Router;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite as ts;
use uuid::Uuid;

use oagw::DataPlaneService;
use oagw::GuardPlugin;
use oagw::OagwConfig;
use oagw::OagwError;
use oagw::PluginEngine;
use oagw::PluginEngineService;
use oagw::PluginRef;
use oagw::PluginRegistries;
use oagw::PluginStore;
use oagw::ProxyHooks;
use oagw::RateLimitLimiter;
use oagw::RateLimitService;
use oagw::TenantHierarchy;
use oagw::api::rest::proxy_routes::register_proxy_routes;
use oagw::domain::services::control_plane::ControlPlaneService;
use oagw::domain::storage::{RouteStore, UpstreamStore};
use oagw::domain::types::{
    Endpoint, GUARD_PLUGIN_TYPE_ID, HttpMatch, PathSuffixMode, PluginsConfig, Protocol,
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitSustained,
    RateLimitWindow, Route, RouteMatch, RouteMethod, RouteSpec, Scheme, ServerConfig, SharingMode,
    Upstream, UpstreamSpec,
};
use oagw::infra::plugin::PluginContext;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// The proxy path (gear-relative, without `/api`).
const PROXY: &str = "/oagw/v1/proxy";

/// The calling tenant of this file's requests.
const TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0002);

/// The `Sec-WebSocket-Key` of RFC 6455 §1.3's own example handshake.
///
/// A fixed key makes the gateway's answer checkable: the `Sec-WebSocket-Accept`
/// the caller sees must be the one derived from *this* key, which is only
/// possible if the key travelled to the upstream unchanged and the gateway signed
/// its own handshake with it.
const CLIENT_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// `base64(SHA1(CLIENT_KEY || "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`.
///
/// The value RFC 6455 §1.3 prints, checked against the derivation the gateway
/// itself performs (see the `expected_accept` unit test in `oagw::streaming`): a
/// hand-rolled upstream that answers with anything else is a non-conformant
/// upstream, and the gateway must refuse to countersign it.
const EXPECTED_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// An accept value that is *not* the one `CLIENT_KEY` derives.
///
/// A plausible base64 blob, so the test cannot pass because of a parsing
/// accident: the value is well-formed and still wrong.
const WRONG_ACCEPT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// The subprotocols the caller offers.
const OFFERED_PROTOCOLS: &str = "chat, superchat";

/// The extension the caller offers, which the gateway's splice cannot honour.
const OFFERED_EXTENSION: &str = "permessage-deflate; client_max_window_bits";

/// The subprotocol the upstream picks out of the caller's offer.
const CHOSEN_PROTOCOL: &str = "chat";

/// How long a test waits for a message that must arrive.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Minimal OpenAPI registry: records nothing, returns the schema name.
struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Ancestor-chain stub: the chain is what the proxy is allowed to see.
struct StubHierarchy {
    chain: Vec<Uuid>,
}

#[async_trait]
impl TenantHierarchy for StubHierarchy {
    async fn chain(&self, _security: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant];
        chain.extend(self.chain.iter().copied());
        chain
    }
}

/// A guard plugin that rejects every request it is handed.
///
/// Not a stub of the engine: it is a real [`GuardPlugin`], registered in a real
/// [`PluginRegistries`] and resolved by the real [`PluginEngineService`], so the
/// test proves that a *plugin* rejection stops a handshake — not that a hand-set
/// error does.
struct RejectingGuard {
    /// The full GTS identifier the registry holds it under.
    reference: String,
}

#[async_trait]
impl GuardPlugin for RejectingGuard {
    fn plugin_ref(&self) -> &str {
        &self.reference
    }

    async fn guard_request(
        &self,
        _context: &PluginContext<'_>,
        _headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        Err(OagwError::validation(
            "the guard rejected this request: no correlation header was sent",
        ))
    }

    async fn guard_response(
        &self,
        _context: &PluginContext<'_>,
        _headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        Ok(())
    }
}

/// A served gateway, plus the stores a test seeds configuration into.
struct Gateway {
    upstreams: Arc<UpstreamStore>,
    routes: Arc<RouteStore>,
    address: SocketAddr,
}

/// A proxy stack the way the gear wires it, served over a loopback port.
///
/// `security` is the caller the host's authentication would have resolved; it is
/// attached as an `Extension`, which is the same thing the other test files do
/// with `Request::extension` before a `oneshot`. `None` means the host resolved
/// nothing, and the proxy must answer 401.
///
/// The loopback mock is plaintext, so `allow_http_upstream` is open here, as the
/// e2e configuration opens it. The plugin engine is left out: the splice is under
/// test, and the auth plugins are covered by their own slice.
async fn gateway(security: Option<SecurityContext>) -> Gateway {
    gateway_with(security, None).await
}

/// The same stack with a plugin engine of the test's choosing.
///
/// `plugins` is the hook the gear installs for the whole plugin chain (ADR-0002);
/// `None` forwards without one, exactly as [`gateway`] does. A handshake that a
/// guard rejects must never reach the upstream, which is what the rejected-
/// handshake test proves — and it cannot be proven without an engine to run the
/// guard.
async fn gateway_with(
    security: Option<SecurityContext>,
    plugins: Option<Arc<dyn PluginEngine>>,
) -> Gateway {
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let control_plane = Arc::new(ControlPlaneService::new(config));
    let upstreams = Arc::clone(control_plane.upstream_store());
    let routes = Arc::clone(control_plane.route_store());

    let hierarchy: Arc<dyn TenantHierarchy> = Arc::new(StubHierarchy { chain: Vec::new() });

    let data_plane = Arc::new(
        DataPlaneService::new(
            config,
            Arc::clone(&control_plane),
            Arc::clone(&upstreams),
            Arc::clone(&routes),
        )
        .with_tenant_hierarchy(hierarchy)
        .with_hooks(ProxyHooks::new(
            Some(Arc::new(RateLimitService::new(Arc::new(
                RateLimitLimiter::new(),
            )))),
            None,
            plugins,
        )),
    );

    let router = register_proxy_routes(Router::new(), &NoopOpenApiRegistry, data_plane);
    let router = match security {
        Some(security) => router.layer(Extension(security)),
        None => router,
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let address = listener.local_addr().expect("the address is known");

    // `axum::serve` is what the host binary runs, and it is
    // `serve_connection_with_upgrades`: without it hyper never hands an upgraded
    // socket to the handler, and every handshake below would fail.
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the gateway serves");
    });

    Gateway {
        upstreams,
        routes,
        address,
    }
}

/// A plaintext endpoint pointing at `port`.
fn http_endpoint(port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port,
    }
}

/// Seed an upstream record for `alias` pointing at `endpoint`.
///
/// The record is *not* passed through the control plane's create path: the
/// management API is not under test here, and the loopback port is whatever the
/// operating system handed out. Normalizing the spec is kept, because resolution
/// relies on the stored alias being lowercase.
fn seed_upstream(gateway: &Gateway, tenant: Uuid, alias: &str, endpoint: Endpoint) -> Uuid {
    let mut spec = UpstreamSpec {
        alias: Some(alias.to_owned()),
        server: ServerConfig {
            endpoints: vec![endpoint],
        },
        protocol: Protocol::Http,
        ..UpstreamSpec::default()
    };
    spec = spec.validate().expect("the upstream spec normalizes");

    gateway
        .upstreams
        .insert(Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        })
        .expect("the upstream inserts")
        .id
}

/// Seed an upstream record for `alias` whose plugin chain binds `guard`.
///
/// The engine resolves a binding against the in-process registries, so a guard is
/// only ever run when the upstream's `plugins.items` names it; this is the same
/// shape the management API would store.
fn seed_guarded_upstream(
    gateway: &Gateway,
    tenant: Uuid,
    alias: &str,
    endpoint: Endpoint,
    guard: String,
) -> Uuid {
    let mut spec = UpstreamSpec {
        alias: Some(alias.to_owned()),
        server: ServerConfig {
            endpoints: vec![endpoint],
        },
        protocol: Protocol::Http,
        plugins: Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new(guard)],
        }),
        ..UpstreamSpec::default()
    };
    spec = spec.validate().expect("the upstream spec normalizes");

    gateway
        .upstreams
        .insert(Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        })
        .expect("the upstream inserts")
        .id
}

/// An engine whose guard registry holds one plugin: [`RejectingGuard`].
///
/// Built exactly as the gear builds it — the built-ins first, then whatever a
/// later gear registered — so the resolution under test is the real one and not a
/// hand-written stub of the chain.
fn rejecting_guard_engine(guard: String) -> Arc<PluginEngineService> {
    let mut registries = PluginRegistries::with_builtins();
    registries
        .guard
        .register(Arc::new(RejectingGuard { reference: guard }));
    Arc::new(PluginEngineService::new(
        registries,
        Arc::new(PluginStore::new()),
    ))
}

/// Seed an enabled `GET` route for `path`, with an optional rate limit.
fn seed_route(
    gateway: &Gateway,
    tenant: Uuid,
    upstream: Uuid,
    path: &str,
    rate_limit: Option<RateLimitConfig>,
) {
    gateway
        .routes
        .insert(Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: upstream,
            created_at: 0,
            updated_at: 0,
            spec: RouteSpec {
                upstream_id: upstream,
                match_rules: RouteMatch {
                    http: Some(HttpMatch {
                        methods: vec![RouteMethod::Get],
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit,
            },
        })
        .expect("the route inserts");
}

/// A reject-strategy tenant-scoped limit of `rate` per second, no burst.
fn limit_per_second(rate: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: RateLimitSustained {
            rate,
            window: RateLimitWindow::Second,
        },
        burst: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
    }
}

/// The caller the host's authentication would have resolved.
fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(TENANT)
        .build()
        .expect("the security context builds")
}

/// Seed the upstream-and-route pair every WebSocket test needs, and return what
/// the upstream observed.
///
/// The upstream always picks a subprotocol: `tokio-tungstenite`'s client refuses
/// an answer that does not repeat one out of the offer it made, and what is under
/// test here is the gateway's relay, not the client library's conformance check.
async fn websocket_gateway() -> (Gateway, Seen) {
    let upstream = websocket_upstream(Some(CHOSEN_PROTOCOL)).await;
    let gateway = gateway(Some(security_context())).await;
    let id = seed_upstream(&gateway, TENANT, "ws.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/", None);
    (gateway, upstream.seen)
}

// ---------------------------------------------------------------------------
// A raw HTTP/1.1 client
// ---------------------------------------------------------------------------

/// A minimal HTTP/1.1 client over a real socket.
///
/// `connection` is the *single* `Connection` header value the caller sends: it is
/// the one header axum and hyper both read as a token list from the first value
/// only, and a caller that wants an upgrade must send exactly what a browser
/// would send.
///
/// The response is read until it is *complete* — by `content-length`, by the
/// terminating chunk, or by EOF — rather than to EOF alone, because a gateway
/// that keeps the caller's connection open after an ordinary answer (as it does
/// for a handshake the upstream refused) would otherwise never end the read.
async fn send_and_read(
    address: SocketAddr,
    path: &str,
    connection: &str,
    headers: &[(&str, &str)],
) -> (u16, HeaderMap, Vec<u8>) {
    let mut socket = TcpStream::connect(address)
        .await
        .expect("the gateway accepts");
    let mut request =
        format!("GET {PROXY}{path} HTTP/1.1\r\nhost: {address}\r\nconnection: {connection}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the request is written");

    let raw = read_response(&mut socket).await;

    let (head, body) = split_head(&raw);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("'{status_line}' carries a status code"));

    let mut response_headers = HeaderMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = HeaderName::from_lowercase(name.trim().to_ascii_lowercase().as_bytes())
            .expect("a valid header name");
        response_headers.append(&name, HeaderValue::from_str(value.trim()).expect("a value"));
    }

    // The framing hyper chose for the response is the gateway's business, not the
    // test's: a caller asserts on the payload, never on which of the two legal
    // encodings delivered it.
    let body = if response_headers.contains_key("transfer-encoding") {
        decode_chunked(body)
    } else {
        body.to_vec()
    };

    (status, response_headers, body)
}

/// Decode a chunked body, terminator included.
fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    let mut rest = body;
    while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
        let size_text = std::str::from_utf8(&rest[..line_end]).unwrap_or_default();
        let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
            return decoded;
        };
        if size == 0 {
            return decoded;
        }
        let start = line_end + 2;
        decoded.extend_from_slice(rest.get(start..start + size).unwrap_or_default());
        rest = rest.get(start + size + 2..).unwrap_or_default();
    }
    decoded
}

/// Read one HTTP/1.1 response off `socket`, until its body is complete.
///
/// Each read is bounded, so a test that never gets what it wants fails inside
/// [`READ_TIMEOUT`] instead of hanging.
async fn read_response(socket: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if response_is_complete(&raw) {
            return raw;
        }
        let read = timeout(READ_TIMEOUT, socket.read(&mut buffer))
            .await
            .expect("the response arrives in time")
            .expect("the response is read");
        if read == 0 {
            return raw;
        }
        raw.extend_from_slice(&buffer[..read]);
    }
}

/// Whether `raw` holds a whole response: a head, and the body it describes.
fn response_is_complete(raw: &[u8]) -> bool {
    let Some(separator) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = std::str::from_utf8(&raw[..separator]).unwrap_or_default();
    let body = &raw[separator + 4..];

    if let Some(length) = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
    {
        return body.len() >= length;
    }
    if head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    {
        // The terminating chunk, framed as `0\r\n\r\n`.
        return body.ends_with(b"0\r\n\r\n");
    }
    false
}

/// Split a raw HTTP/1.1 response into its head and its body.
fn split_head(raw: &[u8]) -> (&str, &[u8]) {
    let separator = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has a head");
    (
        std::str::from_utf8(&raw[..separator]).expect("the head is text"),
        &raw[separator + 4..],
    )
}

/// The value of `name` in a response, as text.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The problem document a response carries.
fn problem(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("a problem+json document")
}

// ---------------------------------------------------------------------------
// WebSocket upstream
// ---------------------------------------------------------------------------

/// What the upstream saw of the handshake the gateway forwarded.
#[derive(Debug, Clone, Default)]
struct Handshake {
    path: String,
    connection: Vec<String>,
    upgrade: Vec<String>,
    key: Vec<String>,
    version: Vec<String>,
    protocol: Vec<String>,
    extensions: Vec<String>,
}

impl Handshake {
    /// The first value of a header, or `None` when the gateway sent none.
    fn one(&self, values: &[String]) -> Option<String> {
        values.first().cloned()
    }
}

/// A shared slot for the handshake the upstream observed.
type Seen = Arc<StdMutex<Option<Handshake>>>;

/// The handshake the upstream recorded.
fn seen_handshake(seen: &Seen) -> Handshake {
    seen.lock()
        .expect("the handshake slot is not poisoned")
        .clone()
        .unwrap_or_else(|| panic!("the upstream received a handshake"))
}

/// The handshake the upstream recorded, if it got one.
fn seen_handshake_opt(seen: &Seen) -> Option<Handshake> {
    seen.lock()
        .expect("the handshake slot is not poisoned")
        .clone()
}

/// A WebSocket upstream that answers the handshake and then runs a session.
struct WsUpstream {
    endpoint: Endpoint,
    seen: Seen,
}

/// What an upstream's session does once the handshake is answered.
///
/// The two alternative flows of `cpt-cf-oagw-usecase-sse-streaming` are *ends*:
/// "upstream closes connection" and "client disconnects". They can only be proven
/// by an upstream whose session is scripted to end, which is what the variants
/// below are for.
#[derive(Clone)]
enum Session {
    /// Echo everything, until the peer ends the session.
    Echo,
    /// Echo everything, and set the flag when this task's read loop returns.
    ///
    /// The flag is the only way a test can see the *upstream* leg end: a socket
    /// closing is not observable from the outside, but the loop that serviced it
    /// returning is.
    EchoUntilEnd { ended: Arc<AtomicBool> },
    /// Say what it was told to say, then close the session, and read no more.
    ///
    /// This is the upstream that initiates the close, which is the flow the PRD
    /// names first: the gateway must close the client's connection with it.
    AnnounceThenClose(Vec<ts::Message>),
}

/// The handshake callback: record what the gateway forwarded, and offer a
/// subprotocol of the upstream's choosing.
struct RecordHandshake {
    seen: Seen,
    protocol: Option<&'static str>,
}

impl ts::handshake::server::Callback for RecordHandshake {
    // tungstenite's `ErrorResponse` is a whole `http::Response<Option<String>>`,
    // which it itself notes is a size it means to box; the shape of the trait is
    // the upstream library's, not this test's.
    #[allow(clippy::result_large_err)]
    fn on_request(
        self,
        request: &ts::handshake::server::Request,
        response: ts::handshake::server::Response,
    ) -> Result<ts::handshake::server::Response, ts::handshake::server::ErrorResponse> {
        // `accept_hdr_async` calls this once, with the request it read off the
        // socket: the forwarded handshake, exactly as the gateway wrote it.
        let mut handshake = Handshake {
            path: request.uri().path().to_owned(),
            ..Handshake::default()
        };
        for (name, slot) in [
            ("connection", 0_u8),
            ("upgrade", 1),
            ("sec-websocket-key", 2),
            ("sec-websocket-version", 3),
            ("sec-websocket-protocol", 4),
            ("sec-websocket-extensions", 5),
        ] {
            let values: Vec<String> = request
                .headers()
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .map(ToOwned::to_owned)
                .collect();
            match slot {
                0 => handshake.connection = values,
                1 => handshake.upgrade = values,
                2 => handshake.key = values,
                3 => handshake.version = values,
                4 => handshake.protocol = values,
                _ => handshake.extensions = values,
            }
        }
        *self
            .seen
            .lock()
            .expect("the handshake slot is not poisoned") = Some(handshake);

        let mut response = response;
        if let Some(protocol) = self.protocol {
            response
                .headers_mut()
                .insert("sec-websocket-protocol", HeaderValue::from_static(protocol));
        }
        Ok(response)
    }
}

/// Start a WebSocket upstream that answers the handshake with `protocol` and then
/// echoes, until the peer ends the session.
///
/// The recorded handshake is what the tests assert on: it is the only place the
/// *forwarded* request is observable, and the gateway's answer to the caller says
/// nothing about what it sent.
async fn websocket_upstream(protocol: Option<&'static str>) -> WsUpstream {
    websocket_upstream_with(protocol, Session::Echo).await
}

/// Start a WebSocket upstream whose session is `session`.
///
/// A session that ends is how the lifecycle tests drive the two alternative flows
/// of the streaming use case, so the upstream — not the caller — decides when the
/// conversation is over, or reports when its own leg was taken away from it.
async fn websocket_upstream_with(protocol: Option<&'static str>, session: Session) -> WsUpstream {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let port = listener.local_addr().expect("the address is known").port();
    let seen: Seen = Arc::new(StdMutex::new(None));

    let seen_for_task = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                continue;
            };
            let (seen, session) = (Arc::clone(&seen_for_task), session.clone());
            tokio::spawn(async move {
                let callback = RecordHandshake { seen, protocol };

                let Ok(stream) = tokio_tungstenite::accept_hdr_async(socket, callback).await else {
                    return;
                };
                run_session(stream, session).await;
            });
        }
    });

    WsUpstream {
        endpoint: http_endpoint(port),
        seen,
    }
}

/// Run one upstream session to its end.
async fn run_session<S>(stream: WebSocketStream<S>, session: Session)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match session {
        Session::Echo => echo(stream, None).await,
        Session::EchoUntilEnd { ended } => echo(stream, Some(ended)).await,
        Session::AnnounceThenClose(messages) => announce_then_close(stream, &messages).await,
    }
}

/// Echo what the gateway relays.
///
/// Text comes back prefixed (so an echo cannot be mistaken for the original
/// arriving on the other leg), binary comes back verbatim, and a control frame is
/// answered with both the answer tungstenite would send on its own and a report
/// the caller can see: tungstenite answers a Ping and a Pong silently, so the only
/// way to prove the gateway *relayed* one is for the upstream to say so.
///
/// `ended` is set when this loop returns, whatever it returned for: a relayed
/// `Close`, a socket error, or a socket that simply went away. It is what lets a
/// test observe the upstream leg being taken away from the session.
async fn echo<S>(stream: WebSocketStream<S>, ended: Option<Arc<AtomicBool>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut source) = stream.split();
    while let Some(item) = source.next().await {
        let Ok(message) = item else { break };
        let replies: Vec<ts::Message> = match message {
            ts::Message::Text(text) => vec![ts::Message::text(format!("echo:{text}"))],
            ts::Message::Binary(data) => vec![ts::Message::Binary(data)],
            ts::Message::Ping(data) => {
                vec![
                    ts::Message::text("saw-a-ping"),
                    // Sent back so the caller can see the *payload* the gateway
                    // relayed, not merely that a control frame moved.
                    ts::Message::Ping(data),
                ]
            }
            ts::Message::Pong(_) => vec![ts::Message::text("got-a-pong")],
            ts::Message::Close(_) => break,
            ts::Message::Frame(_) => Vec::new(),
        };
        for reply in replies {
            if sink.send(reply).await.is_err() {
                break;
            }
        }
    }

    // Every way out of the loop is the upstream leg ending: report it.
    if let Some(ended) = ended.as_ref() {
        ended.store(true, Ordering::SeqCst);
    }
}

/// Send `messages` and close the session, without reading anything back.
///
/// The close is the point: this is the upstream that ends the conversation, and
/// what is under test is what the caller's leg does about it.
async fn announce_then_close<S>(stream: WebSocketStream<S>, messages: &[ts::Message])
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, _source) = stream.split();
    for message in messages {
        if sink.send(message.clone()).await.is_err() {
            return;
        }
    }
    let _ = sink.send(ts::Message::Close(None)).await;
    // The socket is dropped here, which is the upstream closing the connection for
    // good: nothing waits for the caller's answer.
}

// ---------------------------------------------------------------------------
// SSE upstream
// ---------------------------------------------------------------------------

/// A `text/event-stream` upstream that writes two events and will not write the
/// second one until it is told to.
///
/// The release flag is what makes the streaming test non-vacuous. A gateway that
/// buffers the body can only finish its response once the upstream has finished
/// *its* body, and the upstream finishes only after the caller has seen the first
/// event: buffered, the test times out; streamed, the caller sees the first event
/// while the second does not exist yet.
struct SseUpstream {
    endpoint: Endpoint,
    /// Set once the first event is on the wire.
    first_flushed: Arc<AtomicBool>,
    /// Set once the second event is on the wire.
    second_flushed: Arc<AtomicBool>,
    /// Set by the caller to let the upstream write the second event.
    release: Arc<AtomicBool>,
}

/// Start an SSE upstream that waits for `release` between its two events.
async fn sse_upstream() -> SseUpstream {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let port = listener.local_addr().expect("the address is known").port();

    let first_flushed = Arc::new(AtomicBool::new(false));
    let second_flushed = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));

    let (first, second, gate) = (
        Arc::clone(&first_flushed),
        Arc::clone(&second_flushed),
        Arc::clone(&release),
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let _ = sse_once(&mut socket, &first, &second, &gate).await;
        }
    });

    SseUpstream {
        endpoint: http_endpoint(port),
        first_flushed,
        second_flushed,
        release,
    }
}

/// Read one request and answer it with a chunked event stream.
///
/// The chunked framing is written by hand because the test needs to flush in the
/// middle of a body, which is precisely what the gateway must not be able to
/// hide behind a length it computed itself.
async fn sse_once(
    socket: &mut TcpStream,
    first_flushed: &AtomicBool,
    second_flushed: &AtomicBool,
    release: &AtomicBool,
) -> std::io::Result<()> {
    read_request_head(socket).await?;

    let head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(&chunk(b"data: first\n\n")).await?;
    socket.flush().await?;
    first_flushed.store(true, Ordering::SeqCst);

    // The second event does not exist until the caller asks for it.
    while !release.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    socket.write_all(&chunk(b"data: second\n\n")).await?;
    socket.write_all(b"0\r\n\r\n").await?;
    socket.flush().await?;
    second_flushed.store(true, Ordering::SeqCst);
    Ok(())
}

/// One HTTP/1.1 chunk, framed.
fn chunk(payload: &[u8]) -> Vec<u8> {
    let mut framed = format!("{:x}\r\n", payload.len()).into_bytes();
    framed.extend_from_slice(payload);
    framed.extend_from_slice(b"\r\n");
    framed
}

/// Read until the end of an HTTP/1.1 request head.
async fn read_request_head(socket: &mut TcpStream) -> std::io::Result<()> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// A raw `101` upstream: the handshake that is never completed
// ---------------------------------------------------------------------------

/// An upstream that answers one upgrade request with a hand-written `101` and then
/// closes the socket, without ever completing the upgrade.
///
/// `tokio-tungstenite`'s server cannot produce this: a conformant server either
/// completes the handshake or refuses it. It is exactly the upstream the design
/// cares about, though — one that says `101` and then goes away — and writing the
/// head by hand is the only way to reach the gateway's failure paths behind it.
///
/// The `accept` value is what the upstream answers with: the correct one, a wrong
/// one, or none at all. The gateway must verify it (RFC 6455 §4.2.2) before it
/// signs anything of its own.
async fn raw_upgrade_upstream(accept: Option<String>) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let port = listener.local_addr().expect("the address is known").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let _ = upgrade_once(&mut socket, accept.as_deref()).await;
        }
    });

    http_endpoint(port)
}

/// Read one request head, answer `101` with `accept`, and drop the socket.
async fn upgrade_once(socket: &mut TcpStream, accept: Option<&str>) -> std::io::Result<()> {
    read_request_head(socket).await?;

    let accept_line = accept
        .map(|value| format!("sec-websocket-accept: {value}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: \
         websocket\r\n{accept_line}\r\n"
    );
    socket.write_all(head.as_bytes()).await?;
    socket.flush().await?;
    // Dropped here: no upgrade is ever completed, and no frame is ever sent.
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket client
// ---------------------------------------------------------------------------

/// The handshake request the caller sends, with the offered protocol and
/// extension.
fn handshake_request(address: SocketAddr, path: &str) -> ts::handshake::client::Request {
    ts::handshake::client::Request::builder()
        .method(axum::http::Method::GET)
        .uri(format!("ws://{address}{PROXY}{path}"))
        .header("host", format!("{address}"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", CLIENT_KEY)
        .header("sec-websocket-protocol", OFFERED_PROTOCOLS)
        .header("sec-websocket-extensions", OFFERED_EXTENSION)
        .body(())
        .expect("the handshake request builds")
}

/// Connect to the gateway and complete the handshake.
async fn connect(
    address: SocketAddr,
    path: &str,
) -> (
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    HeaderMap,
) {
    let (stream, response) = tokio_tungstenite::connect_async(handshake_request(address, path))
        .await
        .expect("the handshake completes");
    (stream, response.headers().clone())
}

/// Read the next message that satisfies `want`, discarding anything else.
///
/// Control frames are answered by tungstenite on its own, so a caller cannot
/// assume it sees exactly what it expects in exactly the order it sent it; what
/// it can assume is that the message it is waiting for arrives.
async fn expect_message<S, F, T>(socket: &mut WebSocketStream<S>, want: F, what: &str) -> T
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: Fn(&ts::Message) -> Option<T>,
{
    timeout(READ_TIMEOUT, async {
        loop {
            let message = socket
                .next()
                .await
                .unwrap_or_else(|| panic!("the session ended before {what}"));
            let message = message.unwrap_or_else(|error| panic!("{what}: {error}"));
            if let Some(matched) = want(&message) {
                return matched;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} did not arrive within {READ_TIMEOUT:?}"))
}

/// Read until this leg's session ends, and say how it ended.
///
/// A leg that is closed by its peer must end, and the form the end takes is
/// tungstenite's to choose: the relayed `Close` frame, the connection error, or
/// simply the end of the stream. All three are the same fact — the session is
/// over — and what would be a regression is any *message* arriving after the peer
/// said it was done, or the read never returning at all.
async fn expect_session_end<S>(socket: &mut WebSocketStream<S>, what: &str) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (ended, strays) = timeout(READ_TIMEOUT, async {
        let mut strays: Vec<String> = Vec::new();
        loop {
            match socket.next().await {
                None => break ("the stream ended".to_owned(), strays),
                Some(Ok(ts::Message::Close(frame))) => {
                    break (format!("closed with {frame:?}"), strays);
                }
                Some(Err(error)) => break (format!("errored with {error}"), strays),
                // A message the peer sent after it said the session was over. It
                // does not end the loop, because the timeout below is what turns
                // a session that never ends into a failure — but it is reported,
                // so a stray frame is never mistaken for a clean end.
                Some(Ok(message)) => strays.push(format!("{message:?}")),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never ended within {READ_TIMEOUT:?}"));

    assert!(
        strays.is_empty(),
        "{what}: messages arrived after the session ended ({ended}): {strays:?}"
    );
    format!("{what}: {ended}")
}

/// Send a raw upgrade request and read only the head the gateway answers with.
///
/// Unlike [`send_and_read`] this does not wait for a body, because a `101` has
/// none and the test needs the head *before* it learns what happened to the
/// connection behind it. The returned socket is still open, so the test can read
/// the end for itself.
async fn send_upgrade_and_read_head(
    address: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, HeaderMap, TcpStream) {
    let mut socket = TcpStream::connect(address)
        .await
        .expect("the gateway accepts");
    let mut request =
        format!("GET {PROXY}{path} HTTP/1.1\r\nhost: {address}\r\nconnection: Upgrade\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the request is written");

    // Bounded, like every other read: a gateway that neither answers nor hangs up
    // makes this test fail, not hang.
    let raw = timeout(READ_TIMEOUT, async {
        let mut raw = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("the response head is read");
            assert!(read > 0, "the gateway hung up before answering");
            raw.extend_from_slice(&buffer[..read]);
        }
        raw
    })
    .await
    .expect("the response head arrives in time");

    let (head, _) = split_head(&raw);
    let status_line = head.split("\r\n").next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("'{status_line}' carries a status code"));

    let mut response_headers = HeaderMap::new();
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = HeaderName::from_lowercase(name.trim().to_ascii_lowercase().as_bytes())
            .expect("a valid header name");
        response_headers.append(&name, HeaderValue::from_str(value.trim()).expect("a value"));
    }

    (status, response_headers, socket)
}

/// Read this socket to its end, and report whether it ended.
///
/// Bounded, like every read here: a connection that stays open is a failure, not
/// a hang.
async fn read_to_eof(socket: &mut TcpStream, what: &str) {
    timeout(READ_TIMEOUT, async {
        let mut buffer = [0_u8; 64];
        loop {
            let read = socket.read(&mut buffer).await.expect("the socket is read");
            if read == 0 {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never ended within {READ_TIMEOUT:?}"));
}

/// The handshake headers of a raw upgrade request, keyed off [`CLIENT_KEY`].
const RAW_HANDSHAKE: &[(&str, &str)] = &[
    ("upgrade", "websocket"),
    ("sec-websocket-version", "13"),
    ("sec-websocket-key", CLIENT_KEY),
];

/// The text payload of a message.
fn text_of(message: &ts::Message) -> Option<String> {
    match message {
        ts::Message::Text(text) => Some(text.to_string()),
        _ => None,
    }
}

/// The binary payload of a message.
fn binary_of(message: &ts::Message) -> Option<Bytes> {
    match message {
        ts::Message::Binary(data) => Some(data.clone()),
        _ => None,
    }
}

/// The payload of a Ping.
fn ping_of(message: &ts::Message) -> Option<Bytes> {
    match message {
        ts::Message::Ping(data) => Some(data.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests: the splice
// ---------------------------------------------------------------------------

/// A WebSocket session is spliced in both directions: what the caller sends
/// reaches the upstream, and what the upstream sends reaches the caller.
#[tokio::test]
async fn a_websocket_session_is_spliced_in_both_directions() {
    let (gateway, _) = websocket_gateway().await;

    let (mut socket, _) = connect(gateway.address, "/ws.example.com/echo").await;

    // Caller -> upstream -> caller, as text.
    socket
        .send(ts::Message::text("from the caller"))
        .await
        .expect("the message is sent");
    let echoed = expect_message(&mut socket, text_of, "the echoed text").await;
    assert_eq!(echoed, "echo:from the caller");

    // Caller -> upstream -> caller, as binary, and back again unharmed.
    let payload = Bytes::from_static(&[0x00, 0x7f, 0xff, 0xfe]);
    socket
        .send(ts::Message::Binary(payload.clone()))
        .await
        .expect("the frame is sent");
    let back = expect_message(&mut socket, binary_of, "the echoed binary frame").await;
    assert_eq!(back, payload, "a binary frame survives the splice");
}

/// A Ping the caller sends reaches the upstream, a Ping the upstream sends
/// reaches the caller, and the Pong the client answers with comes back too.
#[tokio::test]
async fn control_frames_travel_in_both_directions() {
    let (gateway, _) = websocket_gateway().await;

    let (mut socket, _) = connect(gateway.address, "/ws.example.com/echo").await;

    // Caller -> upstream: the upstream reports every Ping it received as a text
    // message, so this message can only exist if the gateway relayed it.
    socket
        .send(ts::Message::Ping(Bytes::from_static(b"caller-ping")))
        .await
        .expect("the ping is sent");
    let reported = expect_message(&mut socket, text_of, "the upstream's ping report").await;
    assert_eq!(reported, "saw-a-ping");

    // Upstream -> caller: the upstream answers the relayed Ping by sending one
    // with the same payload, so the caller sees the very bytes it sent — proof
    // that a control frame survives the splice unharmed in both directions.
    let relayed = expect_message(&mut socket, ping_of, "the relayed ping").await;
    assert_eq!(relayed, Bytes::from_static(b"caller-ping"));

    // Caller -> upstream again: tungstenite answered that Ping with a Pong, the
    // gateway relayed it, and the upstream reports it.
    let ponged = expect_message(&mut socket, text_of, "the upstream's pong report").await;
    assert_eq!(ponged, "got-a-pong");
}

/// The handshake the gateway forwards carries the caller's key verbatim, the
/// caller's protocol offer, and no extension.
#[tokio::test]
async fn the_forwarded_handshake_is_the_callers_without_an_extension() {
    let (gateway, seen) = websocket_gateway().await;

    let (mut socket, _) = connect(gateway.address, "/ws.example.com/echo").await;
    socket.close(None).await.expect("the socket closes");

    let handshake = seen_handshake(&seen);
    assert_eq!(handshake.path, "/echo");
    assert_eq!(
        handshake.one(&handshake.key).as_deref(),
        Some(CLIENT_KEY),
        "the caller's key is not rewritten: the gateway signs its own handshake with it"
    );
    assert_eq!(handshake.one(&handshake.version).as_deref(), Some("13"));
    assert_eq!(
        handshake.one(&handshake.protocol).as_deref(),
        Some(OFFERED_PROTOCOLS),
        "the caller's offer is forwarded, because only the upstream may pick"
    );
    assert_eq!(
        handshake
            .one(&handshake.upgrade)
            .map(|value| value.to_ascii_lowercase()),
        Some("websocket".to_owned())
    );
    assert!(
        handshake
            .one(&handshake.connection)
            .is_some_and(|value| value
                .split([',', ' '])
                .any(|token| token.eq_ignore_ascii_case("upgrade"))),
        "hyper only takes the upgrade path with the connection token"
    );
    assert!(
        handshake.extensions.is_empty(),
        "the splice negotiates no extension, so none is offered: {:?}",
        handshake.extensions
    );
}

/// The handshake the caller sees is the gateway's, signed from the caller's own
/// key, and it reports the upstream as the source (ADR-0007).
#[tokio::test]
async fn the_gateway_signs_the_handshake_and_marks_it_as_upstream() {
    let (gateway, _) = websocket_gateway().await;

    let (mut socket, headers) = connect(gateway.address, "/ws.example.com/echo").await;
    socket.close(None).await.expect("the socket closes");

    assert_eq!(
        header(&headers, "sec-websocket-accept"),
        Some(EXPECTED_ACCEPT),
        "the accept is derived from the caller's key, which is why it is consistent"
    );
    assert_eq!(
        header(&headers, "upgrade"),
        Some("websocket"),
        "the gateway's own handshake is what the caller sees"
    );
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("upstream"),
        "ADR-0007: an upgraded 101 is an upstream answer like any other"
    );
}

/// The subprotocol the upstream chose out of the caller's offer reaches the
/// caller.
#[tokio::test]
async fn the_subprotocol_the_upstream_chose_reaches_the_caller() {
    let (gateway, _) = websocket_gateway().await;

    let (mut socket, headers) = connect(gateway.address, "/ws.example.com/echo").await;
    socket.close(None).await.expect("the socket closes");

    assert_eq!(
        header(&headers, "sec-websocket-protocol"),
        Some(CHOSEN_PROTOCOL),
        "the caller offered {OFFERED_PROTOCOLS} and the upstream chose {CHOSEN_PROTOCOL}"
    );
}

// ---------------------------------------------------------------------------
// Tests: the lifecycle of a spliced session
// ---------------------------------------------------------------------------

/// An upstream that closes the connection ends the caller's session.
///
/// `cpt-cf-oagw-usecase-sse-streaming`, first alternative flow: "Upstream closes
/// connection: System closes client connection and logs event". The gateway has
/// no session of its own to keep alive, so the only correct thing it can do when
/// one leg says goodbye is end the other.
#[tokio::test]
async fn an_upstream_close_ends_the_callers_session() {
    let upstream = websocket_upstream_with(
        Some(CHOSEN_PROTOCOL),
        Session::AnnounceThenClose(vec![ts::Message::text("closing now")]),
    )
    .await;
    let gateway = gateway(Some(security_context())).await;
    let id = seed_upstream(&gateway, TENANT, "ws.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/", None);

    let (mut socket, _) = connect(gateway.address, "/ws.example.com/echo").await;

    // The upstream's last message crossed the splice, so the session was live.
    let announced = expect_message(&mut socket, text_of, "the upstream's last message").await;
    assert_eq!(announced, "closing now");

    // ... and the close that followed it ends this leg too. A session that stayed
    // open would leave the caller waiting for a message that will never come, and
    // a message after the close would mean the gateway kept a dead session alive.
    let ended = expect_session_end(&mut socket, "the caller's leg").await;
    assert!(
        ended.contains("closed"),
        "the peer's close is what ended it: {ended}"
    );
}

/// A caller that goes away without a close ends the upstream's leg.
///
/// `cpt-cf-oagw-usecase-sse-streaming`, second alternative flow: "Client
/// disconnects: System closes upstream connection". Dropping the socket sends no
/// `Close` frame, so this is the abrupt end — the one that has to be noticed by
/// the leg that is still open, or the upstream would wait forever.
#[tokio::test]
async fn a_caller_that_disappears_ends_the_upstreams_leg() {
    let ended = Arc::new(AtomicBool::new(false));
    let upstream = websocket_upstream_with(
        Some(CHOSEN_PROTOCOL),
        Session::EchoUntilEnd {
            ended: Arc::clone(&ended),
        },
    )
    .await;
    let gateway = gateway(Some(security_context())).await;
    let id = seed_upstream(&gateway, TENANT, "ws.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/", None);

    let (mut socket, _) = connect(gateway.address, "/ws.example.com/echo").await;

    // The session is live in both directions before it is taken away.
    socket
        .send(ts::Message::text("still here"))
        .await
        .expect("the message is sent");
    let echoed = expect_message(&mut socket, text_of, "the echoed text").await;
    assert_eq!(echoed, "echo:still here");

    // No Close frame: the caller just goes away.
    drop(socket);

    timeout(READ_TIMEOUT, async {
        while !ended.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the upstream's leg ends within the timeout");
}

/// A handshake a guard rejects is a gateway problem, and never reaches the
/// upstream.
///
/// The plugin chain runs on a handshake like on any other request, *before* the
/// upstream is dialed — which is the whole reason a rejected handshake can be
/// answered with a problem document at all, and the reason the ADR-0007 marker is
/// `gateway` here.
#[tokio::test]
async fn a_guard_rejected_handshake_is_a_problem_and_is_never_dialed() {
    // The guard's reference is what the upstream binding must name, and what the
    // registry must hold: one string, both places, as a real binding would be.
    let guard = format!("{GUARD_PLUGIN_TYPE_ID}rejecting.vendor.v1");
    let plugins = rejecting_guard_engine(guard.clone());
    let gateway = gateway_with(Some(security_context()), Some(plugins)).await;
    let upstream = websocket_upstream(Some(CHOSEN_PROTOCOL)).await;
    let id = seed_guarded_upstream(&gateway, TENANT, "ws.example.com", upstream.endpoint, guard);
    seed_route(&gateway, TENANT, id, "/", None);

    let (status, headers, body) = send_and_read(
        gateway.address,
        "/ws.example.com/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(
        status, 400,
        "a guard's rejection is the rejection it documented"
    );
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("gateway"),
        "no upstream was involved in the answer"
    );
    assert_eq!(problem(&body)["status"], 400);
    assert!(
        header(&headers, "sec-websocket-accept").is_none(),
        "no handshake was signed for a request the policy rejected"
    );
    assert!(
        seen_handshake_opt(&upstream.seen).is_none(),
        "the guard runs before the dial: the upstream saw nothing"
    );
}

// ---------------------------------------------------------------------------
// Tests: an upstream that agrees and then does not deliver
// ---------------------------------------------------------------------------

/// An upstream whose `101` cannot be verified gets no handshake signed.
///
/// RFC 6455 §4.2.2 makes the `Sec-WebSocket-Accept` check mandatory, and the
/// gateway builds the upstream leg with `from_raw_socket`, which tells tungstenite
/// the handshake is already complete — so the gateway is the only party left that
/// can do it. An upstream that answers with the accept a *different* key derives,
/// or with none, has not agreed to the protocol the caller offered, and the `101`
/// must not be countersigned.
///
/// Both answers are 502 problem documents, not 101s: the caller has received
/// nothing yet, so ADR-0007's "no problem+json once a stream has begun" does not
/// apply.
#[tokio::test]
async fn an_upstream_that_cannot_prove_the_handshake_is_not_signed() {
    for (accept, what) in [
        (Some(WRONG_ACCEPT), "an accept from another key"),
        (None, "no accept at all"),
    ] {
        let gateway = gateway(Some(security_context())).await;
        let endpoint = raw_upgrade_upstream(accept.map(ToOwned::to_owned)).await;
        let id = seed_upstream(&gateway, TENANT, "ws.example.com", endpoint);
        seed_route(&gateway, TENANT, id, "/", None);

        let (status, headers, body) = send_and_read(
            gateway.address,
            "/ws.example.com/echo",
            "Upgrade",
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-version", "13"),
                ("sec-websocket-key", CLIENT_KEY),
            ],
        )
        .await;

        assert_eq!(status, 502, "{what}: the gateway refuses to sign");
        assert_eq!(
            header(&headers, "content-type"),
            Some("application/problem+json"),
            "{what}"
        );
        assert_eq!(
            header(&headers, "x-oagw-error-source"),
            Some("gateway"),
            "{what}: the failure is the gateway's own check"
        );
        assert_eq!(problem(&body)["status"], 502, "{what}");
        assert!(
            header(&headers, "sec-websocket-accept").is_none(),
            "{what}: no handshake reached the caller"
        );
        assert!(
            header(&headers, "sec-websocket-protocol").is_none(),
            "{what}: the upstream's choice is not echoed either"
        );
        let detail = problem(&body)["detail"]
            .as_str()
            .expect("a detail")
            .to_owned();
        let detail = detail.as_str();
        assert!(
            detail.contains("Sec-WebSocket-Accept"),
            "{what}: the reason names the check that failed: {detail}"
        );
    }
}

/// An upstream that answers `101` and then vanishes leaves the caller with a
/// signed handshake and no session behind it.
///
/// This drives the `pending.await` error arm of [`DataPlaneService::proxy_upgrade`]
/// — the arm a real upstream produces. The other arm of that failure path (no
/// `OnUpgrade` extension on the upstream's `101` at all) is **not reachable**
/// through this transport: hyper's client puts the extension on every `101` it
/// reads, whatever the request asked for, so that branch stays as defensive code
/// with its own comment. Either way the caller has *not* been told a stream has
/// begun, because the `101` the gateway signed is the last thing it can be told:
/// the failure is the socket ending, never a document.
#[tokio::test]
async fn an_upstream_that_never_hands_over_the_socket_ends_the_callers_session() {
    let gateway = gateway(Some(security_context())).await;
    // The accept is correct, so the handshake *is* verifiable: what fails is the
    // handover, not the check.
    let endpoint = raw_upgrade_upstream(Some(EXPECTED_ACCEPT.to_owned())).await;
    let id = seed_upstream(&gateway, TENANT, "ws.example.com", endpoint);
    seed_route(&gateway, TENANT, id, "/", None);

    let (status, headers, mut socket) =
        send_upgrade_and_read_head(gateway.address, "/ws.example.com/echo", RAW_HANDSHAKE).await;

    assert_eq!(
        status, 101,
        "the gateway had already signed the handshake when the upstream vanished"
    );
    assert_eq!(
        header(&headers, "sec-websocket-accept"),
        Some(EXPECTED_ACCEPT),
        "the handshake the caller sees is still the gateway's, signed from its own key"
    );
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("upstream"),
        "ADR-0007: an upgraded 101 is an upstream answer like any other"
    );

    // What must not happen is a connection that stays open with nothing behind it.
    // The gateway has no upstream leg to splice, so the only honest answer to the
    // handshake it already signed is to end the connection.
    read_to_eof(&mut socket, "the connection behind the 101").await;
}

// ---------------------------------------------------------------------------
// Tests: answers that are not an upgrade
// ---------------------------------------------------------------------------

/// An upstream that refuses the handshake is passed through as an ordinary
/// response.
#[tokio::test]
async fn an_upstream_that_refuses_the_handshake_is_passed_through() {
    let gateway = gateway(Some(security_context())).await;
    let upstream = plain_upstream(404, "no such socket").await;
    let id = seed_upstream(&gateway, TENANT, "ws.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/", None);

    let (status, headers, body) = send_and_read(
        gateway.address,
        "/ws.example.com/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(status, 404, "the upstream's own status is not rewritten");
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("upstream"),
        "it is still marked as an upstream answer"
    );
    assert_eq!(body, b"no such socket");
    assert!(
        header(&headers, "sec-websocket-accept").is_none(),
        "a refused handshake is not signed as if it had been accepted"
    );
}

/// A handshake offered behind another `Upgrade` header is detected, and therefore
/// answered rather than forwarded.
///
/// RFC 9110 §7.9 lets a caller repeat the `Upgrade` field, so the gateway's
/// detection rule reads all of them — and once it has decided this *is* an
/// upgrade, a request the extractor cannot complete is a 400 problem, never a
/// bodyless `GET` forwarded with its `Upgrade` stripped.
#[tokio::test]
async fn a_handshake_behind_another_upgrade_header_is_rejected_not_forwarded() {
    let (gateway, seen) = websocket_gateway().await;

    let (status, headers, body) = send_and_read(
        gateway.address,
        "/ws.example.com/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "h2c"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(status, 400, "the second value is a websocket offer");
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("gateway"),
        "the request was never forwarded, so no upstream answered it"
    );
    assert_eq!(problem(&body)["status"], 400);
    assert!(
        header(&headers, "sec-websocket-accept").is_none(),
        "a handshake the gateway could not complete is not signed"
    );
    assert!(
        seen_handshake_opt(&seen).is_none(),
        "a detected upgrade is never turned into an ordinary proxied request"
    );
}

/// A handshake for an unknown alias is a problem+json document, not an axum
/// rejection.
#[tokio::test]
async fn a_handshake_for_an_unknown_alias_is_a_problem_document() {
    let gateway = gateway(Some(security_context())).await;

    let (status, headers, body) = send_and_read(
        gateway.address,
        "/no-such-alias/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(status, 404);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
    assert_eq!(problem(&body)["status"], 404);
}

/// A handshake with no authenticated caller fails closed with a 401 problem.
#[tokio::test]
async fn an_unauthenticated_handshake_is_a_problem_document() {
    let gateway = gateway(None).await;
    let upstream = websocket_upstream(Some(CHOSEN_PROTOCOL)).await;
    let id = seed_upstream(&gateway, TENANT, "ws.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/", None);

    let (status, headers, body) = send_and_read(
        gateway.address,
        "/ws.example.com/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(
        status, 401,
        "an upgrade is not a way around the tenant check"
    );
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(problem(&body)["status"], 401);
    assert!(
        seen_handshake_opt(&upstream.seen).is_none(),
        "nothing was dialed for a request that was never authenticated"
    );
}

/// A handshake that exhausts the caller's budget is a 429 problem, like any other
/// request (ADR-0003).
#[tokio::test]
async fn a_throttled_handshake_is_a_problem_document() {
    let (gateway, _) = websocket_gateway().await;

    // One request per second: the handshake below is the second one.
    let route = gateway
        .routes
        .list(TENANT)
        .first()
        .expect("the seeded route")
        .clone();
    let mut spec = route.spec.clone();
    spec.rate_limit = Some(limit_per_second(1));
    gateway
        .routes
        .replace(TENANT, route.id, spec, 0)
        .expect("the route updates");

    let (mut socket, _) = connect(gateway.address, "/ws.example.com/echo").await;
    socket.close(None).await.expect("the socket closes");

    let (status, headers, body) = send_and_read(
        gateway.address,
        "/ws.example.com/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(status, 429, "a handshake spends a token like any request");
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(problem(&body)["status"], 429);
    assert!(
        header(&headers, "retry-after").is_some(),
        "the caller is told when to come back"
    );
}

/// A request that looks like an upgrade but is not a well-formed handshake is a
/// 400 problem, never the plain-text rejection the extractor produces.
#[tokio::test]
async fn a_malformed_handshake_is_a_problem_document_naming_the_reason() {
    let (gateway, seen) = websocket_gateway().await;

    // No `Sec-WebSocket-Version`: RFC 6455 §4.1 requires it, so the gateway has
    // detected an upgrade it cannot complete.
    let (status, headers, body) = send_and_read(
        gateway.address,
        "/ws.example.com/echo",
        "Upgrade",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", CLIENT_KEY),
        ],
    )
    .await;

    assert_eq!(status, 400);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
    let document = problem(&body);
    assert_eq!(document["status"], 400);
    let detail = document["detail"].as_str().expect("a detail");
    assert!(
        detail.to_ascii_lowercase().contains("websocket"),
        "the reason is named in the problem document, not in axum's plain text: {detail}"
    );
    assert!(
        seen_handshake_opt(&seen).is_none(),
        "a handshake the gateway rejected was never dialed"
    );
}

// ---------------------------------------------------------------------------
// Tests: SSE
// ---------------------------------------------------------------------------

/// An SSE response streams through the gateway: the caller sees the first event
/// before the upstream has even written the second.
#[tokio::test]
async fn sse_events_stream_through_the_gateway_without_buffering() {
    let gateway = gateway(Some(security_context())).await;
    let upstream = sse_upstream().await;
    let id = seed_upstream(&gateway, TENANT, "sse.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/events", None);

    let mut socket = TcpStream::connect(gateway.address)
        .await
        .expect("the gateway accepts");
    let request = format!(
        "GET {PROXY}/sse.example.com/events HTTP/1.1\r\nhost: {}\r\naccept: text/event-stream\r\n\r\n",
        gateway.address
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the request is written");

    // The first event must arrive while the upstream is still holding the second
    // one back. A gateway that buffered the body could not produce it at all
    // before the upstream finished, and the upstream finishes only when this test
    // lets it — so this read is the whole proof.
    let first = timeout(READ_TIMEOUT, read_until(&mut socket, b"data: first"))
        .await
        .expect("the first event arrived before the upstream wrote the second");
    assert!(
        !upstream.second_flushed.load(Ordering::SeqCst),
        "the second event did not exist yet when the first was delivered"
    );
    assert!(
        !first.windows(4).any(|window| window == b"data: second"),
        "only the first event was on the wire"
    );

    // Now the upstream may finish, and the caller sees the rest.
    upstream.release.store(true, Ordering::SeqCst);
    timeout(READ_TIMEOUT, read_until(&mut socket, b"data: second"))
        .await
        .expect("the second event arrived");

    assert!(
        upstream.first_flushed.load(Ordering::SeqCst),
        "the upstream really did write the first event"
    );
    assert!(
        upstream.second_flushed.load(Ordering::SeqCst),
        "the upstream really did write the second event"
    );
}

/// An SSE response keeps the media type the upstream declared and invents no
/// length for a body that has none.
#[tokio::test]
async fn an_sse_response_keeps_its_content_type_and_invents_no_length() {
    let gateway = gateway(Some(security_context())).await;
    let upstream = sse_upstream().await;
    let id = seed_upstream(&gateway, TENANT, "sse.example.com", upstream.endpoint);
    seed_route(&gateway, TENANT, id, "/events", None);

    upstream.release.store(true, Ordering::SeqCst);
    let (status, headers, body) =
        send_and_read(gateway.address, "/sse.example.com/events", "close", &[]).await;

    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/event-stream"),
        "the upstream's media type survives the header transformation"
    );
    assert!(
        header(&headers, "content-length").is_none(),
        "a stream has no length, and the gateway must not invent one"
    );
    let body = String::from_utf8(body).expect("the body is text");
    assert!(body.contains("data: first"), "{body}");
    assert!(body.contains("data: second"), "{body}");
}

// ---------------------------------------------------------------------------
// A plain upstream
// ---------------------------------------------------------------------------

/// An upstream that answers every request with `status` and `body`, then closes.
struct PlainUpstream {
    endpoint: Endpoint,
}

/// Start an upstream that answers `status` with `body` and closes the socket.
///
/// The body is chunk-framed so the response has one the gateway can pass through;
/// the test client reads to EOF, which works because the gateway closes the
/// connection once the upstream does.
async fn plain_upstream(status: u16, body: &'static str) -> PlainUpstream {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let port = listener.local_addr().expect("the address is known").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let _ = plain_once(&mut socket, status, body).await;
        }
    });

    PlainUpstream {
        endpoint: http_endpoint(port),
    }
}

/// Read one request head and answer it once.
async fn plain_once(
    socket: &mut TcpStream,
    status: u16,
    body: &'static str,
) -> std::io::Result<()> {
    read_request_head(socket).await?;
    let response = format!(
        "HTTP/1.1 {status} OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n"
    );
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(&chunk(body.as_bytes())).await?;
    socket.write_all(b"0\r\n\r\n").await?;
    socket.flush().await
}

/// Read from `socket` until `marker` appears, returning everything read.
///
/// Bounded per read, like [`read_response`].
async fn read_until(socket: &mut TcpStream, marker: &[u8]) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        if raw.windows(marker.len()).any(|window| window == marker) {
            return raw;
        }
        let read = timeout(READ_TIMEOUT, socket.read(&mut buffer))
            .await
            .expect("the response arrives in time")
            .expect("the response is read");
        if read == 0 {
            return raw;
        }
        raw.extend_from_slice(&buffer[..read]);
    }
}
