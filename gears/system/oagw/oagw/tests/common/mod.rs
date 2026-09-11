//! Shared helpers for the integration suite.

#![allow(dead_code, reason = "each integration binary uses a different subset")]

use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use oagw::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, HttpMatch, MatchConfig, PathSuffixMode,
    PluginsConfig, RateLimitConfig, Route, Scheme, ServerConfig, SharingMode, Upstream,
};
use oagw::domain::services::management::{RouteSpec, UpstreamSpec};
use oagw::test_utils::{MockUpstream, TestGateway};
use serde_json::Value;

/// Minimal HTTP response view used by the assertions.
pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Res {
    /// Parse the body as JSON.
    ///
    /// # Panics
    ///
    /// Panics when the body is not JSON — a test asserting on JSON has
    /// already failed if it is not.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "body is not JSON ({err}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// A response header as a string.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Send one request and read the whole body.
///
/// # Panics
///
/// Panics when the request cannot be built or the exchange fails.
pub async fn send(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Res {
    let client: Client<_, axum::body::Body> =
        Client::builder(TokioExecutor::new()).build_http();
    let mut builder = Request::builder().method(method).uri(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(axum::body::Body::empty, |b| {
            axum::body::Body::from(b.to_owned())
        }))
        .expect("build request");

    let response = client.request(request).await.expect("upstream exchange");
    let (parts, incoming) = response.into_parts();
    let bytes = incoming.collect().await.expect("read body").to_bytes();
    Res {
        status: parts.status,
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

/// `GET` helper.
pub async fn get(url: &str) -> Res {
    send(Method::GET, url, &[], None).await
}

/// `GET` with headers.
pub async fn get_with(url: &str, headers: &[(&str, &str)]) -> Res {
    send(Method::GET, url, headers, None).await
}

/// `POST` a JSON body.
pub async fn post_json(url: &str, body: &Value) -> Res {
    send(
        Method::POST,
        url,
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await
}

/// `PUT` a JSON body.
pub async fn put_json(url: &str, body: &Value) -> Res {
    send(
        Method::PUT,
        url,
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await
}

/// `DELETE` helper.
pub async fn delete(url: &str) -> Res {
    send(Method::DELETE, url, &[], None).await
}

/// Read a streaming response frame by frame until `predicate` is satisfied or
/// the stream ends. Returns the accumulated text.
///
/// # Panics
///
/// Panics when the request fails.
pub async fn stream_text(url: &str, until: impl Fn(&str) -> bool) -> (StatusCode, HeaderMap, String) {
    let client: Client<_, axum::body::Body> =
        Client::builder(TokioExecutor::new()).build_http();
    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .body(axum::body::Body::empty())
        .expect("build request");
    let response = client.request(request).await.expect("exchange");
    let (parts, mut incoming) = response.into_parts();

    let mut text = String::new();
    while let Some(frame) = incoming.frame().await {
        let Ok(frame) = frame else { break };
        if let Some(chunk) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(chunk));
            if until(&text) {
                break;
            }
        }
    }
    (parts.status, parts.headers, text)
}

/// A gateway plus a mock upstream, wired to each other.
pub struct Fixture {
    pub gateway: TestGateway,
    pub upstream: MockUpstream,
}

impl Fixture {
    /// Start both with default settings.
    pub async fn start() -> Self {
        Self::with_builder(oagw::test_utils::TestGatewayBuilder::new()).await
    }

    /// Start both, customizing the gateway.
    pub async fn with_builder(builder: oagw::test_utils::TestGatewayBuilder) -> Self {
        let upstream = MockUpstream::start().await;
        let gateway = builder.start().await;
        Self { gateway, upstream }
    }

    /// Absolute proxy URL for `alias` plus an optional suffix.
    pub fn proxy_url(&self, alias: &str, suffix: &str) -> String {
        if suffix.is_empty() {
            format!("{}/oagw/v1/proxy/{alias}", self.gateway.base_url)
        } else {
            format!(
                "{}/oagw/v1/proxy/{alias}/{}",
                self.gateway.base_url,
                suffix.trim_start_matches('/')
            )
        }
    }

    /// Absolute management URL.
    pub fn api_url(&self, path: &str) -> String {
        format!("{}/oagw/v1{path}", self.gateway.base_url)
    }

    /// A one-endpoint plaintext pool pointing at the mock upstream.
    pub fn endpoints(&self) -> ServerConfig {
        ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Http,
                host: self.upstream.host(),
                port: Some(self.upstream.port()),
            }],
        }
    }

    /// Register an upstream through the Control Plane.
    ///
    /// # Panics
    ///
    /// Panics when creation is rejected.
    pub async fn upstream(&self, alias: &str, tweak: impl FnOnce(&mut UpstreamSpec)) -> Upstream {
        let mut spec = UpstreamSpec {
            alias: Some(alias.to_owned()),
            enabled: None,
            tags: Vec::new(),
            server: self.endpoints(),
            protocol: oagw::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        };
        tweak(&mut spec);
        self.gateway
            .control_plane
            .create_upstream(&self.gateway.security_context, spec)
            .await
            .expect("create upstream")
    }

    /// Register a route through the Control Plane.
    ///
    /// # Panics
    ///
    /// Panics when creation is rejected.
    pub async fn route(
        &self,
        upstream: &Upstream,
        methods: &[&str],
        path: &str,
        tweak: impl FnOnce(&mut RouteSpec),
    ) -> Route {
        let mut spec = RouteSpec {
            upstream_id: Some(upstream.id),
            enabled: None,
            priority: None,
            tags: Vec::new(),
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        };
        tweak(&mut spec);
        self.gateway
            .control_plane
            .create_route(&self.gateway.security_context, spec)
            .await
            .expect("create route")
    }

    /// The common case: one upstream with one wide-open route.
    pub async fn simple(&self, alias: &str) -> Upstream {
        let upstream = self.upstream(alias, |_| {}).await;
        self.route(&upstream, &["GET", "POST", "PUT", "DELETE", "PATCH"], "/v1", |_| {})
            .await;
        upstream
    }
}

/// Convenience constructors mirroring the domain defaults.
pub fn auth(plugin_ref: &str, config: Value) -> AuthConfig {
    AuthConfig {
        plugin_ref: Some(plugin_ref.to_owned()),
        sharing: SharingMode::Private,
        config: config.as_object().cloned().unwrap_or_default(),
    }
}

/// Build a plugin chain from `(plugin_ref, config)` pairs.
pub fn chain(items: Vec<(&str, Value)>) -> PluginsConfig {
    PluginsConfig {
        sharing: SharingMode::Private,
        items: items
            .into_iter()
            .map(|(plugin_ref, config)| oagw::domain::model::PluginBinding {
                plugin_ref: plugin_ref.to_owned(),
                config: config.as_object().cloned().unwrap_or_default(),
            })
            .collect(),
    }
}

/// A token-bucket limit of `rate` per minute with capacity `capacity`.
pub fn per_minute(rate: u32, capacity: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: oagw::domain::model::RateAlgorithm::TokenBucket,
        sustained: oagw::domain::model::SustainedRate {
            rate,
            window: oagw::domain::model::RateWindow::Minute,
        },
        burst: oagw::domain::model::BurstConfig {
            capacity: Some(capacity),
        },
        budget: None,
        scope: oagw::domain::model::RateScope::Tenant,
        strategy: oagw::domain::model::RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// Header rules that forward everything.
pub fn passthrough_all() -> HeadersConfig {
    HeadersConfig {
        request: oagw::domain::model::RequestHeadersConfig {
            passthrough: oagw::domain::model::PassthroughMode::All,
            ..oagw::domain::model::RequestHeadersConfig::default()
        },
        ..HeadersConfig::default()
    }
}

/// A permissive CORS policy for `origin`.
pub fn cors_for(origin: &str, methods: &[&str]) -> CorsConfig {
    CorsConfig {
        enabled: true,
        allowed_origins: vec![origin.to_owned()],
        allowed_methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        expose_headers: vec!["X-Request-ID".to_owned()],
        ..CorsConfig::default()
    }
}

/// Assert a response is an OAGW problem document with the given status and
/// GTS type, and that it is attributed to the gateway.
///
/// # Panics
///
/// Panics with the offending body when any part does not match.
pub fn assert_gateway_problem(res: &Res, status: StatusCode, gts_type: &str) {
    assert_eq!(
        res.status,
        status,
        "unexpected status; body: {}",
        String::from_utf8_lossy(&res.body)
    );
    assert_eq!(
        res.header("content-type"),
        Some("application/problem+json"),
        "gateway errors are RFC 9457 problem documents"
    );
    assert_eq!(
        res.header("x-oagw-error-source"),
        Some("gateway"),
        "gateway errors are attributed to the gateway"
    );
    let body = res.json();
    assert_eq!(body["type"], serde_json::json!(gts_type));
    assert_eq!(body["status"], serde_json::json!(status.as_u16()));
}

/// Minimal WebSocket client: performs the handshake over a raw socket and
/// exchanges one text frame.
pub mod ws {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Outcome of a handshake attempt.
    pub struct Handshake {
        /// Status line of the response, e.g. `HTTP/1.1 101 Switching Protocols`.
        pub status_line: String,
        /// Response header block, lowercased names.
        pub headers: Vec<(String, String)>,
        /// The live socket, positioned just after the header block.
        pub socket: TcpStream,
        /// Bytes already read past the header block.
        pub leftover: Vec<u8>,
    }

    impl Handshake {
        /// A header value by lowercase name.
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// Open a WebSocket connection through the gateway.
    ///
    /// # Panics
    ///
    /// Panics when the socket cannot be opened or the response is malformed.
    pub async fn connect(addr: std::net::SocketAddr, path: &str) -> Handshake {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        socket
            .write_all(request.as_bytes())
            .await
            .expect("send handshake");

        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.expect("read handshake");
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
        }
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("handshake response has a header block");
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let leftover = buf[split + 4..].to_vec();

        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or_default().to_owned();
        let headers = lines
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            })
            .collect();

        Handshake {
            status_line,
            headers,
            socket,
            leftover,
        }
    }

    /// Send one masked text frame.
    ///
    /// # Panics
    ///
    /// Panics when the socket write fails.
    pub async fn send_text(socket: &mut TcpStream, text: &str) {
        let payload = text.as_bytes();
        let mask = [0x12_u8, 0x34, 0x56, 0x78];
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        let mut frame = vec![0x81_u8];
        assert!(payload.len() < 126, "test frames stay short");
        frame.push(0x80 | u8::try_from(payload.len()).unwrap_or(0));
        frame.extend_from_slice(&mask);
        frame.extend_from_slice(&masked);
        socket.write_all(&frame).await.expect("send frame");
    }

    /// Read one unmasked text frame.
    ///
    /// # Panics
    ///
    /// Panics when the socket read fails or the frame is not short text.
    pub async fn recv_text(socket: &mut TcpStream, leftover: &mut Vec<u8>) -> String {
        while leftover.len() < 2 {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read frame header");
            assert!(read > 0, "connection closed before a frame arrived");
            leftover.extend_from_slice(&chunk[..read]);
        }
        let length = usize::from(leftover[1] & 0x7f);
        assert!(length < 126, "test frames stay short");
        while leftover.len() < 2 + length {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read frame payload");
            assert!(read > 0, "connection closed mid-frame");
            leftover.extend_from_slice(&chunk[..read]);
        }
        let text = String::from_utf8_lossy(&leftover[2..2 + length]).to_string();
        leftover.drain(..2 + length);
        text
    }
}

/// Build a `HeaderValue`, panicking on an invalid value.
///
/// # Panics
///
/// Panics when `value` is not a legal header value.
pub fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("valid header value")
}

/// Build a `HeaderName`, panicking on an invalid name.
///
/// # Panics
///
/// Panics when `name` is not a legal header name.
pub fn header_name(name: &'static str) -> HeaderName {
    HeaderName::from_static(name)
}

/// Keep `Arc` in scope for the helpers above.
pub type Shared<T> = Arc<T>;
