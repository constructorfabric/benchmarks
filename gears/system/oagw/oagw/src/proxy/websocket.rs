//! WebSocket upgrade proxying: the tunnel the data plane opens (R4, R7, R10).
//!
//! An upgrade request is never answered with a proxied HTTP response (R4):
//! [`relay`] performs the handshake against the upstream and, once both ends
//! have switched protocols, relays the bytes of the tunnel in both directions
//! until either side closes.
//!
//! ```text
//! upgrade: websocket in the request
//!   -> the handshake headers are re-added to the outbound request (R7)
//!   -> the upstream answers
//!        -> not 101 -> its refusal is passed through as the client's answer
//!        -> 101     -> 101 is answered to the client, then
//!                      copy_bidirectional(client io, upstream io) (R4)
//! ```
//!
//! The tunnel is a byte tunnel: the frames of the upgraded protocol are opaque
//! to the gateway, which is what makes the relay protocol-agnostic and keeps
//! the handshake honest — the client's `Sec-WebSocket-Key` travels upstream
//! unchanged and the upstream's `Sec-WebSocket-Accept` travels back down, so
//! the caller verifies the very handshake the upstream performed.
//!
//! The dial is made over the endpoint's HTTP scheme (`http` for `ws`, `https`
//! for `wss`): a WebSocket handshake is an HTTP request that asks to switch
//! protocols, so the transport-level scheme stays HTTP. Choosing between them
//! is Phase 4's policy — a plaintext endpoint is refused before this module is
//! reached unless `oagw.config.allow_http_upstream` allows it (R10).

use std::time::Duration;

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use tokio::io::copy_bidirectional;

use tracing::{debug, info, warn};

use crate::domain::model::{Endpoint, HttpMethod, Upstream};
use crate::error::ErrorSource;
use crate::proxy::forward::{self, ProxyService, UpstreamCall};
use crate::proxy::headers as gateway_headers;
use crate::proxy::matcher::RouteMatch;

/// How long a tunnel is given to come up on either side.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The headers a WebSocket handshake is established with (RFC 6455 §4.1).
const HANDSHAKE_HEADERS: [header::HeaderName; 4] = [
    header::SEC_WEBSOCKET_KEY,
    header::SEC_WEBSOCKET_VERSION,
    header::SEC_WEBSOCKET_PROTOCOL,
    header::SEC_WEBSOCKET_EXTENSIONS,
];

/// The raw byte stream of an upgraded connection.
type TunnelIo = TokioIo<hyper::upgrade::Upgraded>;

/// Detects a WebSocket upgrade request (R4).
///
/// A request upgrades when it asks for the `websocket` protocol on an HTTP/1.1
/// connection, which is the only shape hyper's server can upgrade.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("websocket"))
        })
}

/// Proxies a WebSocket upgrade (R4, R7, R10).
///
/// # Errors
///
/// Every failure is returned as a [`GatewayError`], which renders as an RFC
/// 9457 problem document, with the host of the endpoint the handshake was
/// dialled at.
pub async fn relay(
    service: &ProxyService,
    upstream: &Upstream,
    endpoint: &Endpoint,
    route: &RouteMatch,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let url = target_url(endpoint, &route.upstream_path, route.query.as_deref());
    let call = UpstreamCall {
        // A WebSocket handshake is a GET with no body (RFC 6455 §4.1).
        method: HttpMethod::Get,
        scheme: endpoint.scheme,
        authority: gateway_headers::authority(endpoint),
        path: route.upstream_path.clone(),
        query: route.query.clone(),
        headers: handshake_headers(&headers, upstream, endpoint),
        body: Bytes::new(),
    };

    let response = match forward::dial(&service.client, &call).await {
        Ok(response) => response,
        Err(mut error) => {
            error = error.with_host(endpoint.host.as_str());
            return error.into_response();
        }
    };

    let mut inner = response.into_inner();

    if inner.status() != StatusCode::SWITCHING_PROTOCOLS {
        // The upstream refused the handshake: its answer is the client's
        // answer, and no tunnel is opened.
        return refusal(upstream, inner);
    }

    let upstream_io = hyper::upgrade::on(&mut inner);
    let (mut head, _) = inner.into_parts();
    gateway_headers::build_response_headers(upstream, &mut head.headers);

    // The values the tunnel is established with are re-added after the
    // hop-by-hop stripping of Phase 4 (R7).
    head.headers
        .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    head.headers
        .insert(header::UPGRADE, HeaderValue::from_static("websocket"));

    let client_io = hyper::upgrade::on(request);
    let relay = Relay {
        alias: upstream.alias().to_owned(),
        url,
    };

    tokio::spawn(async move {
        let (client, opened) = tokio::join!(
            tokio::time::timeout(HANDSHAKE_TIMEOUT, client_io),
            tokio::time::timeout(HANDSHAKE_TIMEOUT, upstream_io),
        );

        let (Ok(Ok(client)), Ok(Ok(opened))) = (client, opened) else {
            warn!(
                upstream = %relay.alias,
                url = %relay.url,
                "the tunnel could not be established on both ends"
            );

            return;
        };

        open_tunnel(&relay, TokioIo::new(client), TokioIo::new(opened)).await;
    });

    let mut response = Response::from_parts(head, axum::body::Body::empty());

    ErrorSource::Upstream.set_on(&mut response);

    response
}

/// The outbound request of a WebSocket handshake (R7).
///
/// The Phase 4 request headers are kept — hop-by-hop headers stripped, the
/// `Host` rewritten, the passthrough and rules applied — and the values the
/// handshake cannot do without are re-added from the inbound request.
fn handshake_headers(inbound: &HeaderMap, upstream: &Upstream, endpoint: &Endpoint) -> HeaderMap {
    let mut headers = gateway_headers::build_request_headers(inbound, upstream, endpoint, 0);

    // An upgrade carries no body, so the length Phase 4's builder declared is
    // dropped rather than forwarded.
    headers.remove(header::CONTENT_LENGTH);

    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));

    for name in HANDSHAKE_HEADERS {
        if let Some(value) = inbound.get(&name) {
            headers.insert(name, value.clone());
        }
    }

    headers
}

/// Passes the refusal of the upstream through as the client's answer (R4).
///
/// The handshake was refused, so there is no tunnel to open and nothing to
/// relay; the upstream's own answer tells the caller why.
fn refusal(upstream: &Upstream, response: http::Response<toolkit_http::ResponseBody>) -> Response {
    let (mut parts, body) = response.into_parts();

    gateway_headers::build_response_headers(upstream, &mut parts.headers);

    let mut response = Response::from_parts(parts, axum::body::Body::new(body));

    ErrorSource::Upstream.set_on(&mut response);

    response
}

/// The absolute URL of a WebSocket dial, for logs (R4).
fn target_url(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    let scheme = if endpoint.is_plaintext() {
        "http"
    } else {
        "https"
    };
    let authority = format!("{}:{}", endpoint.host.as_str(), endpoint.port);

    match query {
        Some(query) => format!("{scheme}://{authority}{path}?{query}"),
        None => format!("{scheme}://{authority}{path}"),
    }
}

/// The tunnel that is being relayed, for its lifecycle logs.
struct Relay {
    /// The alias of the upstream the tunnel goes to.
    alias: String,
    /// The URL the tunnel was established with.
    url: String,
}

/// Relays the bytes of an established tunnel in both directions (R4).
///
/// The relay ends when either side closes, and the other side is closed with
/// it, so no half-open tunnel is left behind.
async fn open_tunnel(relay: &Relay, mut client: TunnelIo, mut upgraded: TunnelIo) {
    info!(upstream = %relay.alias, url = %relay.url, "the tunnel is open");

    match copy_bidirectional(&mut client, &mut upgraded).await {
        Ok(bytes) => closed_tunnel(relay, bytes),
        Err(error) => failed_tunnel(relay, &error),
    }
}

/// Logs the close of a tunnel that both sides ended (R4).
fn closed_tunnel(relay: &Relay, (sent, received): (u64, u64)) {
    debug!(
        upstream = %relay.alias,
        url = %relay.url,
        client_to_upstream = sent,
        upstream_to_client = received,
        "the tunnel closed"
    );
}

/// Logs the failure of a tunnel (R4).
fn failed_tunnel(relay: &Relay, error: &std::io::Error) {
    warn!(
        upstream = %relay.alias,
        url = %relay.url,
        error = %error,
        "the tunnel failed"
    );
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::http::StatusCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;
    use crate::OagwConfig;
    use crate::domain::model::{Host, PROTOCOL_HTTP, RouteSpec, Scheme, UpstreamSpec};
    use crate::domain::store::ConfigService;
    use crate::proxy::register_proxy_routes;

    /// How long a test waits for a read before giving up on it.
    const READ_TIMEOUT: Duration = Duration::from_secs(5);

    /// The sample key of RFC 6455 §1.3, and the accept the upstream must send
    /// back for it.
    const RFC6455_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    const RFC6455_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    /// An unmasked text frame carrying `hello`, as an upstream would send it.
    const HELLO_FRAME: &[u8] = b"\x81\x05hello";

    /// An unmasked text frame carrying `echo!`, as the test client sends it.
    const ECHO_FRAME: &[u8] = b"\x81\x05echo!";

    /// The alias every seeded upstream is given.
    const ALIAS: &str = "upstream";

    // -- harness ----------------------------------------------------------

    /// A gateway with plaintext upstreams allowed, so a raw socket can play the
    /// upstream.
    fn config() -> OagwConfig {
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        }
    }

    /// Seeds one plaintext upstream with the given endpoint and one route of it.
    fn seed(config_service: &ConfigService, port: u16, methods: &[&str], path: &str) {
        let spec: UpstreamSpec = serde_json::from_value(serde_json::json!({
            "alias": ALIAS,
            "protocol": PROTOCOL_HTTP,
            "server": {
                "endpoints": [{ "host": "127.0.0.1", "port": port, "scheme": "http" }]
            }
        }))
        .unwrap();
        let created = config_service.create_upstream(Uuid::nil(), &spec).unwrap();

        let mut route_spec: RouteSpec = serde_json::from_value(serde_json::json!({
            "match": { "http": { "methods": methods, "path": path } }
        }))
        .unwrap();
        route_spec.upstream_id = created.id;
        config_service
            .create_route(Uuid::nil(), &route_spec)
            .unwrap();
    }

    /// Serves a data plane on an ephemeral port, with one seeded plaintext
    /// upstream.
    async fn served_gateway(
        config: OagwConfig,
        upstream_port: u16,
        methods: &[&str],
        path: &str,
    ) -> SocketAddr {
        let config_service = Arc::new(ConfigService::new(config));
        seed(&config_service, upstream_port, methods, path);

        let service = Arc::new(ProxyService::new(config_service).unwrap());
        let router = register_proxy_routes(Router::new(), service);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        address
    }

    /// Serves exactly one raw upstream connection on an ephemeral port, handed
    /// to `handler` once the request head has been read.
    async fn serve_once<F, Fut>(handler: F) -> u16
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            handler(socket).await;
        });

        port
    }

    /// Reads and returns the head of a raw request, up to its blank line.
    async fn read_request_head(socket: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];

        while find(&buffer, b"\r\n\r\n").is_none() {
            let read = tokio::time::timeout(READ_TIMEOUT, socket.read(&mut chunk))
                .await
                .unwrap()
                .unwrap();
            buffer.extend_from_slice(&chunk[..read]);
        }

        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// The offset of `marker` in `haystack`, when it is there.
    fn find(haystack: &[u8], marker: &[u8]) -> Option<usize> {
        haystack
            .windows(marker.len())
            .position(|window| window == marker)
    }

    /// A raw client connection to the gateway, read through a buffer so the
    /// handshake head and the tunnel bytes behind it can be asserted separately.
    struct RawClient {
        socket: TcpStream,
        buffer: Vec<u8>,
    }

    impl RawClient {
        async fn connect(address: SocketAddr) -> Self {
            Self {
                socket: TcpStream::connect(address).await.unwrap(),
                buffer: Vec::new(),
            }
        }

        async fn send(&mut self, request: &str) {
            self.socket.write_all(request.as_bytes()).await.unwrap();
            self.socket.flush().await.unwrap();
        }

        async fn write(&mut self, frame: &[u8]) {
            self.socket.write_all(frame).await.unwrap();
            self.socket.flush().await.unwrap();
        }

        /// Reads until `marker` has been seen, and returns what was read.
        async fn read_until(&mut self, marker: &str) -> String {
            let marker = marker.as_bytes();

            loop {
                if let Some(position) = find(&self.buffer, marker) {
                    let read = self
                        .buffer
                        .drain(..position + marker.len())
                        .collect::<Vec<u8>>();
                    return String::from_utf8_lossy(&read).into_owned();
                }

                let mut chunk = [0_u8; 1024];
                let read = tokio::time::timeout(READ_TIMEOUT, self.socket.read(&mut chunk))
                    .await
                    .expect("the read does not hang")
                    .expect("the read succeeds");

                assert!(
                    read > 0,
                    "the connection closed before `{}` was seen",
                    String::from_utf8_lossy(marker)
                );
                self.buffer.extend_from_slice(&chunk[..read]);
            }
        }

        /// Reads `len` raw tunnel bytes, whatever they are.
        async fn read_bytes(&mut self, len: usize) -> Vec<u8> {
            while self.buffer.len() < len {
                let mut chunk = [0_u8; 1024];
                let read = tokio::time::timeout(READ_TIMEOUT, self.socket.read(&mut chunk))
                    .await
                    .expect("the read does not hang")
                    .expect("the read succeeds");

                assert!(
                    read > 0,
                    "the connection closed before {len} bytes were read"
                );
                self.buffer.extend_from_slice(&chunk[..read]);
            }

            self.buffer.drain(..len).collect()
        }

        /// Reads until the gateway closes the connection.
        async fn read_to_end(&mut self) -> String {
            let mut rest = std::mem::take(&mut self.buffer);
            let mut chunk = [0_u8; 1024];

            loop {
                match tokio::time::timeout(READ_TIMEOUT, self.socket.read(&mut chunk)).await {
                    Ok(Ok(0) | Err(_)) | Err(_) => break,
                    Ok(Ok(read)) => rest.extend_from_slice(&chunk[..read]),
                }
            }

            String::from_utf8_lossy(&rest).into_owned()
        }
    }

    // -- the tunnel -------------------------------------------------------

    #[tokio::test]
    async fn test_an_upgrade_is_relayed_and_the_tunnel_carries_both_ways() {
        let (heads_tx, mut heads_rx) = mpsc::unbounded_channel::<String>();

        // The upstream answers the handshake, greets the client, echoes one
        // frame back and reports the moment the tunnel closes behind it.
        let port = serve_once(move |mut socket| async move {
            heads_tx.send(read_request_head(&mut socket).await).ok();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                         connection: upgrade\r\nsec-websocket-accept: {RFC6455_ACCEPT}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(HELLO_FRAME).await.unwrap();
            socket.flush().await.unwrap();

            let mut frame = [0_u8; ECHO_FRAME.len()];
            socket.read_exact(&mut frame).await.unwrap();
            socket.write_all(&frame).await.unwrap();
            socket.flush().await.unwrap();

            let mut rest = [0_u8; 16];
            let closed = socket.read(&mut rest).await;
            let read = closed.unwrap_or(0);
            heads_tx
                .send(format!("tunnel closed after {read} bytes"))
                .ok();
        })
        .await;

        let address = served_gateway(config(), port, &["GET"], "/socket").await;
        let mut client = RawClient::connect(address).await;
        client
            .send(&format!(
                "GET /oagw/v1/proxy/upstream/socket HTTP/1.1\r\nhost: gateway\r\n\
                 upgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-key: {RFC6455_KEY}\r\n\
                 sec-websocket-version: 13\r\nx-oagw-target-host: somewhere.example.com\r\n\r\n"
            ))
            .await;

        // The upstream's own accept is copied back verbatim, so the caller
        // verifies the very handshake the upstream performed (R4).
        let head = client.read_until("\r\n\r\n").await;
        assert!(head.contains("HTTP/1.1 101"), "{head}");
        assert!(head.contains("upgrade: websocket"), "{head}");
        assert!(head.contains("connection: upgrade"), "{head}");
        assert!(
            head.contains(&format!("sec-websocket-accept: {RFC6455_ACCEPT}")),
            "{head}"
        );

        // The upstream speaks first and the client hears it before writing a
        // single byte of its own.
        let greeting = client.read_bytes(HELLO_FRAME.len()).await;
        assert_eq!(greeting, HELLO_FRAME);

        // What the client sends comes back, untouched by the gateway.
        client.write(ECHO_FRAME).await;
        let echoed = client.read_bytes(ECHO_FRAME.len()).await;
        assert_eq!(echoed, ECHO_FRAME);

        // Walking away closes the tunnel on the upstream side too: the upstream
        // reads nothing more after its echo was relayed back.
        drop(client);
        let _ = tokio::time::timeout(READ_TIMEOUT, heads_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let closed = tokio::time::timeout(READ_TIMEOUT, heads_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            closed.starts_with("tunnel closed after 0 bytes"),
            "{closed}"
        );
    }

    #[tokio::test]
    async fn test_the_upstream_receives_an_honest_handshake() {
        let (heads_tx, mut heads_rx) = mpsc::unbounded_channel::<String>();

        let port = serve_once(move |mut socket| async move {
            let head = read_request_head(&mut socket).await;
            heads_tx.send(head).ok();

            // Refuse the upgrade: the test only looks at the request.
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        })
        .await;

        let address = served_gateway(config(), port, &["GET"], "/socket").await;
        let mut client = RawClient::connect(address).await;
        client
            .send(&format!(
                "GET /oagw/v1/proxy/upstream/socket HTTP/1.1\r\nhost: gateway\r\n\
                 upgrade: websocket\r\nconnection: upgrade\r\nkeep-alive: timeout=5\r\n\
                 sec-websocket-key: {RFC6455_KEY}\r\nsec-websocket-version: 13\r\n\r\n"
            ))
            .await;
        let _ = client.read_to_end().await;

        let head = heads_rx.recv().await.unwrap();

        // The values the handshake cannot do without reach the upstream.
        assert!(head.contains("GET /socket HTTP/1.1"), "{head}");
        assert!(head.contains("upgrade: websocket"), "{head}");
        assert!(head.contains("connection: upgrade"), "{head}");
        assert!(
            head.contains(&format!("sec-websocket-key: {RFC6455_KEY}")),
            "{head}"
        );
        assert!(head.contains("sec-websocket-version: 13"), "{head}");

        // Hop-by-hop headers and the gateway's own plumbing do not travel, and
        // an upgrade declares no length.
        assert!(head.contains("host: 127.0.0.1:"), "{head}");
        assert!(!head.contains("host: gateway"), "{head}");
        assert!(!head.contains("keep-alive"), "{head}");
        assert!(!head.contains("x-oagw-target-host"), "{head}");
        assert!(!head.contains("content-length"), "{head}");
    }

    #[tokio::test]
    async fn test_an_upstream_refusal_is_passed_through_as_the_answer() {
        let port = serve_once(move |mut socket| async move {
            read_request_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                      content-length: 11\r\n\r\n{\"denied\":1}",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
        })
        .await;

        let address = served_gateway(config(), port, &["GET"], "/socket").await;
        let mut client = RawClient::connect(address).await;
        client
            .send(&format!(
                "GET /oagw/v1/proxy/upstream/socket HTTP/1.1\r\nhost: gateway\r\n\
                 upgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-key: {RFC6455_KEY}\r\n\
                 sec-websocket-version: 13\r\n\r\n"
            ))
            .await;

        // No 101 is invented for a handshake the upstream refused (R4).
        let head = client.read_until("\r\n\r\n").await;
        assert!(head.contains("HTTP/1.1 401"), "{head}");
        assert!(!head.contains("101"), "{head}");
        assert!(head.contains("x-oagw-error-source: upstream"), "{head}");

        let rest = client.read_to_end().await;
        assert!(rest.contains("\"denied\""), "{rest}");
    }

    // -- the schemes a tunnel may dial (R10) ------------------------------

    #[test]
    fn test_a_tunnel_to_a_plaintext_endpoint_needs_the_policy() {
        let allowed = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let forbidden = OagwConfig {
            allow_http_upstream: false,
            ..OagwConfig::default()
        };
        let plaintext = Endpoint::new(Scheme::Http, Host::parse("127.0.0.1").unwrap(), 8080);
        let secured = Endpoint::new(Scheme::Https, Host::parse("127.0.0.1").unwrap(), 8443);

        assert!(forward::check_scheme_policy(&allowed, &plaintext).is_ok());
        assert!(forward::check_scheme_policy(&allowed, &secured).is_ok());
        assert!(forward::check_scheme_policy(&forbidden, &secured).is_ok());

        let error = forward::check_scheme_policy(&forbidden, &plaintext).unwrap_err();

        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE.as_u16());
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[test]
    fn test_a_plaintext_ws_endpoint_cannot_be_registered_without_the_policy() {
        let config_service = ConfigService::new(OagwConfig {
            allow_http_upstream: false,
            ..OagwConfig::default()
        });
        let spec: UpstreamSpec = serde_json::from_value(serde_json::json!({
            "alias": ALIAS,
            "protocol": PROTOCOL_HTTP,
            "server": {
                "endpoints": [{ "host": "127.0.0.1", "port": 8080, "scheme": "http" }]
            }
        }))
        .unwrap();

        let error = config_service
            .create_upstream(Uuid::nil(), &spec)
            .unwrap_err();

        assert_eq!(error.status(), StatusCode::BAD_REQUEST.as_u16());
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    /// A handshake is dialled over the endpoint's HTTP scheme: plaintext for
    /// `ws`, TLS for `wss` (R4, R10).
    #[test]
    fn test_the_dial_url_follows_the_scheme_of_the_endpoint() {
        let ws = Endpoint::new(Scheme::Http, Host::parse("echo.example.com").unwrap(), 8080);
        let wss = Endpoint::new(Scheme::Wss, Host::parse("echo.example.com").unwrap(), 443);

        assert_eq!(
            target_url(&ws, "/socket", None),
            "http://echo.example.com:8080/socket"
        );
        assert_eq!(
            target_url(&wss, "/socket", Some("room=1")),
            "https://echo.example.com:443/socket?room=1"
        );
    }
}
