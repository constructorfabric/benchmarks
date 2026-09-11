//! WebSocket pass-through tests (DESIGN §3.2 "Streaming", ADR 0001).
//!
//! Both hops run over real sockets: the upstream is an axum server echoing
//! WebSocket frames, and the gateway itself is served by `axum::serve` so the
//! upgrade future the handler captures is the one hyper fulfils. `tower::oneshot`
//! cannot drive an upgrade, because it never attaches one to a request.
//!
//! The client is a minimal RFC 6455 peer — a handshake, one masked text frame,
//! one read — rather than a crate the workspace does not carry.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::gts_helpers as gts;
use crate::domain::gts_helpers::PROTOCOL_HTTP;
use crate::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, MatchConfig, PathSuffixMode, Route, ServerConfig, Upstream,
};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::service::{DataPlaneService, is_websocket_upgrade};
use crate::infra::storage::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};

const TENANT: &str = "00000000-0000-0000-0000-000000000001";
/// The RFC 6455 example key, whose well-known accept value is asserted below.
const EXAMPLE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const EXAMPLE_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

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

/// An axum server that echoes every frame back over the upgraded socket.
async fn echo_upstream() -> u16 {
    let router = Router::new().route(
        "/api/chat",
        axum::routing::get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|socket: WebSocket| async move {
                let (mut sink, mut stream) = socket.split();
                while let Some(Ok(message)) = futures_util::StreamExt::next(&mut stream).await {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    if futures_util::SinkExt::send(&mut sink, message)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    port
}

async fn control_plane(port: u16) -> ControlPlaneService {
    struct FlatHierarchy;
    #[async_trait::async_trait]
    impl crate::domain::repo::TenantHierarchy for FlatHierarchy {
        async fn chain(&self, tenant_id: &str) -> Vec<String> {
            vec![tenant_id.to_owned()]
        }
    }
    let cp = ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepository::default()),
        Arc::new(MemoryRouteRepository::default()),
        Arc::new(MemoryPluginRepository::default()),
        Arc::new(FlatHierarchy),
        true,
    );
    let upstream = Upstream {
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(port),
            }],
        },
        protocol: PROTOCOL_HTTP.to_owned(),
        ..Upstream::default()
    };
    let created = cp
        .create_upstream(TENANT, upstream, Some("backend".to_owned()))
        .await
        .unwrap();
    let route = Route {
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![
                    crate::domain::model::HttpMethod::Get,
                    crate::domain::model::HttpMethod::Post,
                ],
                path: "/api".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..Route::default()
    };
    let mut stored = route;
    stored.upstream_id = created.id;
    stored.enabled = true;
    cp.create_route(TENANT, stored).await.unwrap();
    cp
}

/// Serves the gateway's own routes with a security context injected.
async fn serve_gateway(cp: ControlPlaneService) -> u16 {
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let cp = Arc::new(cp);
    let data_plane = DataPlaneService::new(cp.clone(), config.clone())
        .unwrap()
        .with_registries(
            crate::infra::plugin::registry::AuthPluginRegistry::empty(),
            crate::infra::plugin::registry::GuardPluginRegistry::empty(),
            crate::infra::plugin::registry::TransformPluginRegistry::empty(),
        );
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(Uuid::parse_str(TENANT).unwrap())
        .build()
        .unwrap();
    let router = crate::api::rest::routes::register_routes(
        Router::new(),
        &NoopOpenApiRegistry,
        cp,
        Arc::new(data_plane),
        config,
    )
    .layer(axum::Extension(ctx));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    port
}

/// A completed RFC 6455 handshake: the raw socket plus the server's head.
struct Handshake {
    socket: tokio::net::TcpStream,
    accept: Option<String>,
    /// Bytes the server sent before the first frame, if any.
    rest: Vec<u8>,
}

/// Opens a WebSocket handshake against `path` on the gateway.
async fn handshake(port: u16, path: &str) -> Handshake {
    use tokio::io::AsyncWriteExt;
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nupgrade: websocket\r\n\
         connection: Upgrade\r\nsec-websocket-key: {EXAMPLE_KEY}\r\nsec-websocket-version: 13\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let (status, headers, rest) = read_head(&mut socket).await;
    assert_eq!(status, 101, "the upgrade must succeed through the proxy");
    let accept = headers
        .iter()
        .find(|(name, _)| name == "sec-websocket-accept")
        .map(|(_, value)| value.clone());
    Handshake {
        socket,
        accept,
        rest,
    }
}

/// Reads bytes until a full HTTP head has arrived, then splits it.
async fn read_head(socket: &mut tokio::net::TcpStream) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "the connection closed before a response arrived");
        buffered.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffered.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffered[..end + 2]).to_string();
            let mut lines = head.lines();
            let status = lines
                .next()
                .and_then(|line| line.split(' ').nth(1))
                .and_then(|code| code.parse::<u16>().ok())
                .unwrap_or_default();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            return (status, headers, buffered[end + 4..].to_vec());
        }
    }
}

/// Sends one masked text frame.
async fn send_text(socket: &mut tokio::net::TcpStream, payload: &str) {
    use tokio::io::AsyncWriteExt;
    let bytes = payload.as_bytes();
    assert!(
        bytes.len() < 126,
        "the test client only writes short frames"
    );
    let mut frame = vec![
        0x81u8,
        0x80 | u8::try_from(bytes.len()).unwrap(),
        0x00,
        0x00,
        0x00,
        0x00,
    ];
    frame.extend_from_slice(bytes);
    socket.write_all(&frame).await.unwrap();
}

/// Reads the first complete, unmasked server frame.
async fn read_frame(socket: &mut tokio::net::TcpStream, seed: &mut Vec<u8>) -> (u8, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    loop {
        if let Some((opcode, payload)) = decode(seed) {
            let consumed = frame_len(seed).unwrap_or(seed.len());
            seed.drain(..consumed);
            return (opcode, payload);
        }
        let mut chunk = [0u8; 1024];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "the connection closed mid-frame");
        seed.extend_from_slice(&chunk[..read]);
    }
}

/// Decodes the first complete frame, if one has arrived.
fn decode(buffered: &[u8]) -> Option<(u8, Vec<u8>)> {
    let end = frame_len(buffered)?;
    Some((buffered[0] & 0x0f, buffered[2..end].to_vec()))
}

/// The byte length of the first frame, when it is complete.
fn frame_len(buffered: &[u8]) -> Option<usize> {
    if buffered.len() < 2 {
        return None;
    }
    let (offset, length) = match buffered[1] & 0x7f {
        126 => {
            if buffered.len() < 4 {
                return None;
            }
            (4, u16::from_be_bytes([buffered[2], buffered[3]]) as usize)
        }
        // The echo never sends anything large enough to need a 64-bit length.
        127 => return None,
        size => (2, size as usize),
    };
    (buffered.len() >= offset + length).then(|| offset + length)
}

/// The proxy path only ever sees a GET upgrade (RFC 7230 §5.4).
#[test]
fn upgrade_detection_follows_rfc_7230() {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    assert!(is_websocket_upgrade(&Method::GET, &headers));
    assert!(!is_websocket_upgrade(&Method::POST, &headers));

    let mut wrong_token = HeaderMap::new();
    wrong_token.insert(http::header::UPGRADE, HeaderValue::from_static("h2c"));
    wrong_token.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("Upgrade"),
    );
    assert!(!is_websocket_upgrade(&Method::GET, &wrong_token));
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_echo_through_the_proxy() {
    let port = echo_upstream().await;
    let cp = control_plane(port).await;
    let gateway = serve_gateway(cp).await;

    let mut upgrade = handshake(gateway, "/oagw/v1/proxy/backend/api/chat").await;
    assert_eq!(
        upgrade.accept.as_deref(),
        Some(EXAMPLE_ACCEPT),
        "the upstream's accept value must reach the client unchanged"
    );

    send_text(&mut upgrade.socket, "hello through the proxy").await;
    let mut seed = std::mem::take(&mut upgrade.rest);
    let (opcode, payload) = read_frame(&mut upgrade.socket, &mut seed).await;
    assert_eq!(opcode, 0x1, "the echo is a text frame");
    assert_eq!(String::from_utf8_lossy(&payload), "hello through the proxy");

    // A second round trip proves the spliced connection stays a conversation
    // rather than a one-shot relay.
    send_text(&mut upgrade.socket, "and again").await;
    let (_, payload) = read_frame(&mut upgrade.socket, &mut seed).await;
    assert_eq!(String::from_utf8_lossy(&payload), "and again");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_handshake_is_not_spoofed_by_a_plain_get() {
    use tokio::io::AsyncWriteExt;
    let port = echo_upstream().await;
    let cp = control_plane(port).await;
    let gateway = serve_gateway(cp).await;

    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", gateway))
        .await
        .unwrap();
    // An `Upgrade: websocket` header without a `Connection: Upgrade` token is
    // not an upgrade, so it is proxied as an ordinary request.
    let request = format!(
        "GET /oagw/v1/proxy/backend/api/chat HTTP/1.1\r\nhost: 127.0.0.1:{gateway}\r\n\
         upgrade: websocket\r\nconnection: keep-alive\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let (status, headers, _rest) = read_head(&mut socket).await;
    // The upstream answers the plain GET itself (axum's route refuses a
    // non-upgrading handshake with 400); the gateway must have relayed that
    // answer rather than upgrading on its own.
    assert_eq!(status, 400, "a non-upgrading request is proxied as HTTP");
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == gts::HEADER_ERROR_SOURCE)
            .map(|(_, value)| value.as_str()),
        Some(gts::ERROR_SOURCE_UPSTREAM),
        "the rejection comes from the upstream, not from the gateway"
    );
}

/// The handshake headers an upstream recorded, as `(name, value)` pairs.
type CapturedHeaders = Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// An upstream that records the handshake headers it was sent, then echoes.
async fn capturing_upstream(seen: CapturedHeaders) -> u16 {
    let router = Router::new().route(
        "/api/chat",
        axum::routing::get(move |ws: WebSocketUpgrade, headers: HeaderMap| {
            let seen = Arc::clone(&seen);
            async move {
                *seen.lock().unwrap() = headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_owned(),
                            value.to_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect();
                ws.on_upgrade(|socket: WebSocket| async move {
                    echo_socket(socket).await;
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    port
}

/// Relays every incoming frame back until the peer closes.
async fn echo_socket(socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    while let Some(Ok(message)) = StreamExt::next(&mut stream).await {
        if matches!(message, Message::Close(_)) {
            break;
        }
        if futures_util::SinkExt::send(&mut sink, message)
            .await
            .is_err()
        {
            break;
        }
    }
}

/// The gateway's own routing headers are consumed on the upgrade path too: they
/// direct the proxy and must not leak to the upstream with the handshake.
#[tokio::test(flavor = "multi_thread")]
async fn the_upgrade_handshake_leaves_the_gateway_headers_behind() {
    use tokio::io::AsyncWriteExt;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = capturing_upstream(Arc::clone(&seen)).await;
    let cp = control_plane(port).await;
    let gateway = serve_gateway(cp).await;

    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", gateway))
        .await
        .unwrap();
    let request = format!(
        "GET /oagw/v1/proxy/backend/api/chat HTTP/1.1\r\nhost: 127.0.0.1:{gateway}\r\n\
         upgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: {EXAMPLE_KEY}\r\n\
         sec-websocket-version: 13\r\nx-oagw-target-host: 127.0.0.1\r\n\
         x-oagw-error-source: gateway\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let (status, _headers, mut rest) = read_head(&mut socket).await;
    assert_eq!(status, 101, "the upgrade must still complete");

    // The handshake reached the upstream without the two gateway headers.
    let headers = seen.lock().unwrap().clone();
    assert!(
        !headers
            .iter()
            .any(|(name, _)| name == gts::HEADER_TARGET_HOST),
        "the routing header must not reach the upstream: {headers:?}"
    );
    assert!(
        !headers
            .iter()
            .any(|(name, _)| name == gts::HEADER_ERROR_SOURCE),
        "the error-source header must not reach the upstream: {headers:?}"
    );

    // The spliced connection still carries frames.
    send_text(&mut socket, "still talking").await;
    let mut buffered = std::mem::take(&mut rest);
    let (opcode, payload) = read_frame(&mut socket, &mut buffered).await;
    assert_eq!(opcode, 0x1);
    assert_eq!(String::from_utf8_lossy(&payload), "still talking");
}
