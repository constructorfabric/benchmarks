//! End-to-end Data Plane tests against a real upstream over a real socket.
//!
//! A minimal HTTP/1.1 origin server is spawned on an ephemeral port and the
//! gear's own router is served by `axum::serve`, so plain requests, SSE
//! streams and WebSocket upgrades all traverse the production code path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use oagw::api::rest::{OagwState, register_routes};
use oagw::config::OagwConfig;
use oagw::domain::dto::{RouteWriteInput, UpstreamWriteInput};
use oagw::domain::plugin::PluginCatalog;
use oagw::domain::services::DataPlaneService;
use oagw::domain::services::management::ControlPlaneService;
use oagw::infra::metrics::OagwMetrics;
use oagw::infra::plugin::{PluginRegistries, TokenCacheConfig};
use oagw::infra::proxy::{DataPlaneServiceImpl, UpstreamConnector};
use oagw::infra::ratelimit::RateLimiterRegistry;
use oagw::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};
use oagw::infra::tenant_directory::FlatTenantDirectory;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

/// Spawn a hand-rolled HTTP/1.1 origin server. Returns its address.
async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve_upstream_conn(stream));
        }
    });
    addr
}

async fn serve_upstream_conn(mut stream: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read the request head.
    let head_end = loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();

    let mut headers = serde_json::Map::new();
    let mut content_length = 0usize;
    let mut ws_key = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        if name == "sec-websocket-key" {
            ws_key.clone_from(&value);
        }
        headers.insert(name, Value::String(value));
    }

    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    if path.starts_with("/ws") {
        // Deterministic accept value: the test only asserts the handshake and
        // the byte-level echo, not RFC 6455 framing.
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\r\n",
            if ws_key.is_empty() { "none" } else { "accepted" }
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        let mut echo = [0u8; 4096];
        loop {
            match stream.read(&mut echo).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if stream.write_all(&echo[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    if path.starts_with("/sse") {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for i in 0..3 {
            let event = format!("data: event-{i}\n\n");
            let framed = format!("{:x}\r\n{event}\r\n", event.len());
            if stream.write_all(framed.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = stream.write_all(b"0\r\n\r\n").await;
        let _ = stream.flush().await;
        return;
    }

    if path.starts_with("/slow") {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let (status, content_type, payload) = if path.starts_with("/boom") {
        (
            "500 Internal Server Error",
            "application/json",
            br#"{"error":"upstream exploded"}"#.to_vec(),
        )
    } else {
        let echoed = json!({
            "method": method,
            "path": path,
            "headers": Value::Object(headers),
            "body": String::from_utf8_lossy(&body),
        });
        (
            "200 OK",
            "application/json",
            echoed.to_string().into_bytes(),
        )
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(&payload).await;
    let _ = stream.flush().await;
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

// ---------------------------------------------------------------------------
// Gateway under test
// ---------------------------------------------------------------------------

struct Gateway {
    addr: SocketAddr,
    control_plane: Arc<ControlPlaneService>,
    ctx: SecurityContext,
}

async fn spawn_gateway(config: OagwConfig) -> Gateway {
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![(
            "upstream-key".to_owned(),
            "sk-secret-value".to_owned(),
        )]),
    );
    let registries = Arc::new(PluginRegistries::with_builtins(
        credstore,
        TokenCacheConfig::default(),
    ));
    let catalog: Arc<dyn PluginCatalog> = Arc::clone(&registries) as Arc<dyn PluginCatalog>;
    let control_plane = Arc::new(ControlPlaneService::new(
        Arc::new(InMemoryUpstreamRepo::new()),
        Arc::new(InMemoryRouteRepo::new()),
        Arc::new(InMemoryPluginRepo::new()),
        Arc::new(FlatTenantDirectory),
        catalog,
    ));
    let limiter = Arc::new(RateLimiterRegistry::new());
    let data_plane: Arc<dyn DataPlaneService> = Arc::new(DataPlaneServiceImpl::new(
        Arc::clone(&control_plane),
        registries,
        Arc::new(UpstreamConnector::new(&config)),
        Arc::clone(&limiter),
        Arc::new(OagwMetrics::from_global()),
        config,
    ));

    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("security context");

    let state = Arc::new(OagwState {
        control_plane: Arc::clone(&control_plane),
        data_plane,
        limiter,
    });
    let openapi = toolkit::api::openapi_registry::OpenApiRegistryImpl::new();
    let router: Router = register_routes(Router::new(), &openapi, state)
        .layer(axum::Extension(ctx.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Gateway {
        addr,
        control_plane,
        ctx,
    }
}

fn test_config() -> OagwConfig {
    let mut config = OagwConfig::default();
    // The graded deployment permits plaintext upstreams and turns the SSRF
    // guard off; the loopback mock needs both.
    config.allow_http_upstream = true;
    config.ssrf_policy.enabled = false;
    config.proxy_timeout_secs = 2;
    config
}

impl Gateway {
    async fn seed_upstream(&self, alias: &str, upstream: SocketAddr, extra: Value) -> Uuid {
        let mut body = json!({
            "alias": alias,
            "server": {"endpoints": [{
                "scheme": "http",
                "host": upstream.ip().to_string(),
                "port": upstream.port(),
            }]},
            "protocol": HTTP_PROTOCOL,
        });
        if let (Some(target), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        let input: UpstreamWriteInput = serde_json::from_value(body).expect("upstream input");
        self.control_plane
            .create_upstream(&self.ctx, input)
            .await
            .expect("upstream created")
            .id
    }

    async fn seed_route(&self, upstream_id: Uuid, path: &str, methods: Value, extra: Value) {
        let mut http_match = json!({"methods": methods, "path": path});
        if let (Some(target), Some(extra)) = (http_match.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        let input: RouteWriteInput = serde_json::from_value(json!({
            "upstream_id": upstream_id,
            "match": {"http": http_match},
        }))
        .expect("route input");
        self.control_plane
            .create_route(&self.ctx, input)
            .await
            .expect("route created");
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

// ---------------------------------------------------------------------------
// HTTP client helpers
// ---------------------------------------------------------------------------

async fn send(
    addr: SocketAddr,
    request: Request<Body>,
) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let stream = TcpStream::connect(addr).await.expect("connect");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let response: Response<hyper::body::Incoming> =
        sender.send_request(request).await.expect("send");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn request(method: &str, url: &str) -> http::request::Builder {
    Request::builder().method(method).uri(url)
}

fn as_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|err| {
        panic!(
            "expected JSON, got {err}: {}",
            String::from_utf8_lossy(bytes)
        )
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_http_requests_are_proxied_with_the_error_source_header() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/echo", json!(["GET", "POST"]), json!({})).await;

    let (status, headers, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo/sub"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "successful passthrough is still attributed to the upstream"
    );
    let echoed = as_json(&body);
    assert_eq!(echoed["method"], "GET");
    assert_eq!(echoed["path"], "/echo/sub");
    // Host is rewritten to the upstream authority.
    assert_eq!(
        echoed["headers"]["host"],
        json!(format!("{}:{}", upstream.ip(), upstream.port()))
    );
}

#[tokio::test]
async fn request_bodies_and_entity_headers_reach_the_upstream() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/echo", json!(["POST"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("POST", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"gpt-4"}"#))
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let echoed = as_json(&body);
    assert_eq!(echoed["body"], r#"{"model":"gpt-4"}"#);
    assert_eq!(echoed["headers"]["content-type"], "application/json");
    assert_eq!(echoed["headers"]["content-length"], "17");
}

#[tokio::test]
async fn hop_by_hop_and_routing_headers_never_reach_the_upstream() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({"headers": {"request": {"passthrough": "all"}}}),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("x-oagw-target-host", upstream.ip().to_string())
            .header("x-keep-me", "yes")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let headers = &as_json(&body)["headers"];
    assert!(headers.get("x-oagw-target-host").is_none());
    assert_eq!(headers["x-keep-me"], "yes");
}

#[tokio::test]
async fn the_api_key_auth_plugin_injects_the_resolved_credential() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({
                "auth": {
                    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    "config": {
                        "secret_ref": "cred://upstream-key",
                        "in": "header",
                        "name": "Authorization",
                        "prefix": "Bearer ",
                    },
                },
            }),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        as_json(&body)["headers"]["authorization"],
        "Bearer sk-secret-value"
    );
}

#[tokio::test]
async fn a_missing_secret_is_a_500_secret_not_found() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({
                "auth": {
                    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    "config": {"secret_ref": "cred://absent-key"},
                },
            }),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, headers, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        as_json(&body)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
}

#[tokio::test]
async fn upstream_errors_pass_through_attributed_to_the_upstream() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/boom", json!(["GET"]), json!({})).await;

    let (status, headers, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/boom"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    // The upstream body is forwarded unchanged, not wrapped in problem+json.
    assert_eq!(body, br#"{"error":"upstream exploded"}"#);
}

#[tokio::test]
async fn a_slow_upstream_yields_504_request_timeout() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/slow", json!(["GET"]), json!({})).await;

    let (status, headers, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/slow"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("2")
    );
    assert_eq!(
        as_json(&body)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
}

#[tokio::test]
async fn sse_events_are_forwarded_as_they_arrive() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/sse", json!(["GET"]), json!({})).await;

    let stream = TcpStream::connect(gw.addr).await.expect("connect");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let response = sender
        .send_request(
            request("GET", &gw.url("/oagw/v1/proxy/mock/sse"))
                .header("accept", "text/event-stream")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    // Frames must arrive incrementally, not as one buffered blob at the end.
    let mut body = response.into_body();
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let Some(frame) = body.frame().await else {
            break;
        };
        let frame = frame.expect("frame");
        if let Some(data) = frame.data_ref() {
            let text = String::from_utf8_lossy(data).into_owned();
            for line in text.lines().filter(|l| l.starts_with("data: ")) {
                seen.push(line.trim_start_matches("data: ").to_owned());
            }
        }
    }
    assert_eq!(seen, vec!["event-0", "event-1", "event-2"]);
}

#[tokio::test]
async fn websocket_upgrades_are_spliced_end_to_end() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/ws", json!(["GET"]), json!({})).await;

    let mut stream = TcpStream::connect(gw.addr).await.expect("connect");
    let handshake = format!(
        "GET /oagw/v1/proxy/mock/ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n",
        gw.addr
    );
    stream
        .write_all(handshake.as_bytes())
        .await
        .expect("handshake");

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("read timed out")
            .expect("read");
        assert_ne!(n, 0, "gateway closed before answering the handshake");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected a 101 from the gateway, got:\n{head}"
    );
    assert!(head.to_ascii_lowercase().contains("upgrade: websocket"));
    assert!(head.to_ascii_lowercase().contains("sec-websocket-accept"));

    // Past the 101 the connection is an opaque byte pipe in both directions.
    let payload = b"frames-over-the-gateway";
    stream.write_all(payload).await.expect("write");
    let mut echoed = buf[head_end..].to_vec();
    while echoed.len() < payload.len() {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("echo timed out")
            .expect("read");
        assert_ne!(n, 0, "connection closed before the echo arrived");
        echoed.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(&echoed[..payload.len()], payload);
}

#[tokio::test]
async fn rate_limits_reject_with_429_and_retry_after() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({
                "rate_limit": {
                    "sustained": {"rate": 1, "window": "minute"},
                    "burst": {"capacity": 1},
                    "scope": "tenant",
                    "strategy": "reject",
                },
            }),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, headers, _) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );

    let (status, headers, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));
    // RFC 6585 quota headers accompany the rejection, not just the success.
    assert_eq!(
        headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("0")
    );
    let problem = as_json(&body);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert!(problem["retry_after_seconds"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn cors_is_enforced_on_the_actual_request() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({
                "cors": {
                    "enabled": true,
                    "allowed_origins": ["https://app.example.com"],
                    "allowed_methods": ["GET"],
                    "expose_headers": ["X-Request-ID"],
                },
            }),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET", "POST"]), json!({})).await;

    let (status, headers, _) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("origin", "https://app.example.com")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers.get("vary").and_then(|v| v.to_str().ok()),
        Some("Origin")
    );

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("origin", "https://evil.com")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        as_json(&body)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );

    let (status, _, body) = send(
        gw.addr,
        request("POST", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("origin", "https://app.example.com")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        as_json(&body)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

#[tokio::test]
async fn the_required_headers_guard_rejects_before_the_upstream_call() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({
                "plugins": {"items": [{
                    "plugin_ref":
                        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    "config": {"required_request_headers": "x-correlation-id"},
                }]},
                "headers": {"request": {"passthrough": "all"}},
            }),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let problem = as_json(&body);
    assert_eq!(problem["error_code"], "REQUIRED_HEADER_MISSING");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("x-correlation-id")
    );

    let (status, _, _) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("x-correlation-id", "abc")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_request_id_transform_propagates_a_correlation_id() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream(
            "mock",
            upstream,
            json!({
                "plugins": {"items": [
                    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
                ]},
            }),
        )
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .header("x-request-id", "req-abc123")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(as_json(&body)["headers"]["x-request-id"], "req-abc123");
}

#[tokio::test]
async fn a_disabled_upstream_answers_503() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw
        .seed_upstream("mock", upstream, json!({"enabled": false}))
        .await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, headers, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(as_json(&body)["status"], 503);
}

#[tokio::test]
async fn plaintext_upstreams_are_refused_when_the_flag_is_off() {
    let upstream = spawn_upstream().await;
    let mut config = test_config();
    config.allow_http_upstream = false;
    let gw = spawn_gateway(config).await;
    // The management API still accepts the `http` scheme — only the
    // *connection* is gated.
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        as_json(&body)["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("plaintext")
    );
}

#[tokio::test]
async fn the_ssrf_guard_blocks_loopback_when_enabled() {
    let upstream = spawn_upstream().await;
    let mut config = test_config();
    config.ssrf_policy.enabled = true;
    let gw = spawn_gateway(config).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        as_json(&body)["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("loopback")
    );
}

#[tokio::test]
async fn a_disallowed_query_parameter_is_rejected_before_forwarding() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(
        id,
        "/echo",
        json!(["GET"]),
        json!({"query_allowlist": ["limit"]}),
    )
    .await;

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo?limit=10"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(as_json(&body)["path"], "/echo?limit=10");

    let (status, _, body) = send(
        gw.addr,
        request("GET", &gw.url("/oagw/v1/proxy/mock/echo?limit=10&secret=1"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        as_json(&body)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn an_unmatched_route_is_404_route_not_found() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(test_config()).await;
    let id = gw.seed_upstream("mock", upstream, json!({})).await;
    gw.seed_route(id, "/echo", json!(["GET"]), json!({})).await;

    for url in [
        gw.url("/oagw/v1/proxy/mock/nowhere"),
        gw.url("/oagw/v1/proxy/absent/echo"),
    ] {
        let (status, _, body) = send(
            gw.addr,
            request("GET", &url).body(Body::empty()).expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{url}");
        assert_eq!(
            as_json(&body)["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }
}
