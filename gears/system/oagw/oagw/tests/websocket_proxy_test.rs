#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Upgrade tunneling of the proxy data plane, over live sockets.
//!
//! Upgrade needs a real connection on both ends, so the tests cannot run
//! through `Router::oneshot`: the caller side is a raw TCP client speaking
//! RFC 6455 by hand (the production path never parses frames, and no WebSocket
//! client dependency exists), the upstream is an axum `ws` echo server — the
//! `test-utils` feature is what makes `axum/ws` available — and the gear itself
//! is served by a real `axum::serve` listener. Both listeners bind to port 0.
//!
//! What is under test ([phase 7](../../docs/DESIGN.md) "Proxy Request Flow"):
//! the `101` relayed with its handshake, frames round-tripping in both
//! directions, the session ending when the caller ends it, the gateway problem
//! when the upstream refuses the upgrade, and the plain HTTP path left alone.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::body::to_bytes;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::Request;
use axum::response::Response;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use toolkit::ClientHub;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::{ProxyState, register_proxy_routes};
use oagw::config::{OagwConfig, SsrfPolicyConfig};
use oagw::domain::model::{
    Alias, EndpointScheme, HttpMethod, Protocol, Route, Upstream, UpstreamEndpoint,
};
use oagw::domain::service::Service;
use oagw::gear::OagwState;
use oagw::infra::http_client::ProxyClient;
use oagw::infra::plugins::{PluginCatalog, PluginRegistry, SecretResolver, TokenCacheConfig};
use oagw::infra::ratelimit::RateLimiter;
use oagw::infra::secrets::CredStoreSecretResolver;
use oagw::infra::store::Store;

/// The gear-relative prefix of every proxied path.
const PROXY: &str = "/oagw/v1/proxy/";
/// The alias of the test upstream, which owns the echo endpoint.
const ALIAS: &str = "ws.echo.test";
/// The tenant the test caller is authenticated as.
const TENANT: &str = "00000000-0000-0000-0000-000000000001";

/// The RFC 6455 §1.3 example key, whose accept value is a constant of the
/// protocol: whatever the proxy relays has to be byte-for-byte the upstream's.
const RFC_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// The accept value the RFC 6455 §1.3 example computes for [`RFC_KEY`].
const RFC_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// A `text` frame, per RFC 6455 §5.2.
const OP_TEXT: u8 = 0x1;
/// A `close` frame, per RFC 6455 §5.5.1.
const OP_CLOSE: u8 = 0x8;
/// How long a test waits for the tunnel to end before calling it stuck.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

// ── Harness ───────────────────────────────────────────────────────────────

/// The tenant the test gear serves.
fn tenant() -> Uuid {
    Uuid::parse_str(TENANT).unwrap()
}

/// The graded configuration: plaintext `http` endpoints allowed, and the SSRF
/// policy off, because the mocked upstreams dial `127.0.0.1`.
fn config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicyConfig { enabled: false },
        ..OagwConfig::default()
    }
}

/// The gear state behind the proxy, with the same shape `Gear::init` builds.
fn state() -> Arc<ArcSwap<OagwState>> {
    let resolver: Arc<dyn SecretResolver> =
        Arc::new(CredStoreSecretResolver::new(Arc::new(ClientHub::new())));
    Arc::new(ArcSwap::from_pointee(OagwState {
        config: config(),
        store: Arc::new(Store::new()),
        plugins: Arc::new(PluginRegistry::with_builtins(
            resolver,
            TokenCacheConfig::new(Duration::from_secs(60), 128),
        )),
        plugin_catalog: Arc::new(PluginCatalog::new()),
        rate_limiter: Arc::new(RateLimiter::with_system_clock()),
    }))
}

/// Injects the caller the tests stand in for, the way the platform middleware
/// does in front of the gear's router.
async fn authenticate(
    mut request: Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    request.extensions_mut().insert(
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(tenant())
            .build()
            .unwrap(),
    );
    next.run(request).await
}

/// Serves the proxy gear with one `http` upstream pointing at `upstream`, and
/// returns the address it listens on together with its router.
async fn serve_proxy(upstream: SocketAddr) -> (SocketAddr, Router) {
    let swap = state();
    let store = Arc::clone(&swap.load().store);
    let openapi = OpenApiRegistryImpl::new();
    let proxy = Arc::new(ProxyState {
        gear: Arc::clone(&swap),
        service: Arc::new(Service::new(Arc::clone(&store), Arc::new(ClientHub::new()))),
        client_hub: Arc::new(ClientHub::new()),
        client: ProxyClient::new(),
    });
    let router = register_proxy_routes(Router::new(), &openapi, proxy)
        .layer(axum::middleware::from_fn(authenticate));

    let record = Upstream {
        id: None,
        enabled: true,
        alias: Some(Alias::try_new(ALIAS).unwrap()),
        tags: Vec::new(),
        server: oagw::domain::model::UpstreamServer {
            endpoints: vec![UpstreamEndpoint {
                scheme: EndpointScheme::Http,
                host: "127.0.0.1".to_owned(),
                port: upstream.port(),
            }],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    };
    let record = put_upstream(&store, record);
    put_route(&store, record.id.unwrap());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let serving = router.clone();
    tokio::spawn(async move { axum::serve(listener, serving).await.unwrap() });
    (address, router)
}

/// Puts an upstream in the store, with the id the service would have generated.
fn put_upstream(store: &Store, mut upstream: Upstream) -> Upstream {
    let id = upstream.id.unwrap_or_else(Uuid::new_v4);
    upstream.id = Some(id);
    store.put_upstream(tenant(), upstream.clone());
    upstream
}

/// Puts the route that maps every path of the test upstream.
fn put_route(store: &Store, upstream_id: Uuid) {
    store.put_route(
        tenant(),
        Route {
            id: Some(Uuid::new_v4()),
            tags: Vec::new(),
            upstream_id,
            match_rule: oagw::domain::model::RouteMatch {
                http: Some(oagw::domain::model::HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: "/".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: oagw::domain::model::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        },
    );
}

/// The upstream the tests proxy to: a `ws` echo endpoint and a plain one, on
/// one ephemeral port.
async fn serve_upstream() -> SocketAddr {
    let app = Router::new()
        .route("/ws", axum::routing::get(echo_upgrade))
        .route("/plain", axum::routing::get(|| async { "pong" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

/// Upgrades to a WebSocket that echoes every text and binary frame back, until
/// the caller closes.
async fn echo_upgrade(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|mut socket: WebSocket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                axum::extract::ws::Message::Text(text) => {
                    if socket
                        .send(axum::extract::ws::Message::Text(text))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                axum::extract::ws::Message::Binary(bytes) => {
                    if socket
                        .send(axum::extract::ws::Message::Binary(bytes))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                axum::extract::ws::Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

// ── RFC 6455 client ───────────────────────────────────────────────────────

/// Opens a WebSocket session through the proxy at `path`, returning the socket,
/// the status it was answered with and the headers of that answer.
async fn open_session(
    proxy: SocketAddr,
    path: &str,
) -> io::Result<(TcpStream, u16, Vec<(String, String)>)> {
    let mut stream = TcpStream::connect(proxy).await?;
    let request = format!(
        "GET {PROXY}{ALIAS}{path} HTTP/1.1\r\n\
         host: caller.example.com\r\n\
         connection: Upgrade\r\n\
         upgrade: websocket\r\n\
         sec-websocket-key: {RFC_KEY}\r\n\
         sec-websocket-version: 13\r\n\
         \r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let head = read_head(&mut stream).await?;
    let (status, headers) = parse_head(&head);
    Ok((stream, status, headers))
}

/// The value of `name` in `headers`, or `None` when it is not there.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_str())
}

/// Sends one masked client frame (RFC 6455 §5.3), the only framing the test
/// client does.
async fn send_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mask = [0x37_u8, 0x5a, 0x71, 0x0c];
    let mut frame = vec![0x80_u8 | opcode];
    match payload.len() {
        length @ 0..=125 => frame.push(0x80_u8 | length as u8),
        length => {
            frame.push(0x80_u8 | 126);
            frame.extend_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&frame).await
}

/// Reads one unmasked server frame, returning its opcode and its payload.
async fn read_frame(stream: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).await?;
    let opcode = head[0] & 0x0f;
    let length = match head[1] & 0x7f {
        126 => {
            let mut extended = [0_u8; 2];
            stream.read_exact(&mut extended).await?;
            u64::from(u16::from_be_bytes(extended))
        }
        127 => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the tests do not send 64-bit frames",
            ));
        }
        length => u64::from(length),
    };
    let mut payload = vec![0_u8; usize::try_from(length).unwrap_or(0)];
    stream.read_exact(&mut payload).await?;
    Ok((opcode, payload))
}

/// Reads frames until the connection ends, reporting whether it did end: the
/// caller's close must end the tunnel, not hang it.
async fn ends(stream: &mut TcpStream) -> bool {
    loop {
        match tokio::time::timeout(CLOSE_TIMEOUT, read_frame(stream)).await {
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => return true,
            Err(_) => return false,
        }
    }
}

/// Reads the response head of a raw HTTP conversation, up to the blank line.
async fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
    }
}

/// The status line and the headers of one response head.
fn parse_head(head: &str) -> (u16, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_default();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (status, headers)
}

/// Reads the body of a response whose length `headers` declares.
async fn read_body(stream: &mut TcpStream, headers: &[(String, String)]) -> io::Result<Vec<u8>> {
    let length: usize = header(headers, "content-length")
        .and_then(|length| length.parse().ok())
        .unwrap_or_default();
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

// ── Tunnel ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_is_relayed_and_both_directions_round_trip() {
    let upstream = serve_upstream().await;
    let (proxy, _router) = serve_proxy(upstream).await;

    let (mut socket, status, headers) = open_session(proxy, "/ws").await.unwrap();

    assert_eq!(status, 101, "the caller expects the protocol to switch");
    assert_eq!(
        header(&headers, "sec-websocket-accept"),
        Some(RFC_ACCEPT),
        "the upstream's accept must be relayed verbatim"
    );
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("upstream"),
        "the 101 came from the upstream"
    );

    // The handshake is over: what flows is frames, which the proxy never reads.
    for message in ["first message", "second message"] {
        send_frame(&mut socket, OP_TEXT, message.as_bytes())
            .await
            .unwrap();
        let (opcode, payload) = read_frame(&mut socket).await.unwrap();
        assert_eq!(opcode, OP_TEXT, "{message}");
        assert_eq!(payload, message.as_bytes(), "{message}");
    }

    // And the session ends when the caller ends it.
    send_frame(&mut socket, OP_CLOSE, &[]).await.unwrap();
    assert!(ends(&mut socket).await, "the tunnel must close cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_upgrade_is_a_gateway_problem() {
    let upstream = serve_upstream().await;
    let (proxy, _router) = serve_proxy(upstream).await;

    // `/plain` answers the handshake with a plain 200: no protocol switch.
    let (mut socket, status, headers) = open_session(proxy, "/plain").await.unwrap();

    assert_eq!(status, 502, "a session that cannot happen is a 502");
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
    let problem: Value =
        serde_json::from_slice(&read_body(&mut socket, &headers).await.unwrap()).unwrap();
    assert_eq!(problem["status"], 502, "{problem}");
}

// ── Non-upgrade traffic ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_request_on_the_same_route_is_proxied_as_before() {
    let upstream = serve_upstream().await;
    let (_proxy, router) = serve_proxy(upstream).await;

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("{PROXY}{ALIAS}/plain"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-oagw-error-source"],
        axum::http::header::HeaderValue::from_static("upstream")
    );
    let body = to_bytes(response.into_body(), 64).await.unwrap();
    assert_eq!(body.as_ref(), b"pong");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_header_alone_is_not_an_upgrade() {
    let upstream = serve_upstream().await;
    let (_proxy, router) = serve_proxy(upstream).await;

    // `Connection: upgrade` is the other half of the detection rule: without it
    // the request is a plain HTTP one, upgrade header or not.
    let mut request = Request::builder()
        .method("GET")
        .uri(format!("{PROXY}{ALIAS}/plain"))
        .body(axum::body::Body::empty())
        .unwrap();
    request.headers_mut().insert(
        axum::http::header::UPGRADE,
        axum::http::header::HeaderValue::from_static("websocket"),
    );

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200, "the plain path served it");
    let body = to_bytes(response.into_body(), 64).await.unwrap();
    assert_eq!(body.as_ref(), b"pong");
}
