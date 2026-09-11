//! End-to-end tests of the data plane.
//!
//! The proxy is exercised over a real axum server with a real mock upstream on
//! the other side, because the three paths it must forward — plain HTTP, a
//! server-sent-event stream and a WebSocket upgrade — differ in how the bytes
//! move, and only a socket shows that.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use axum::response::Response;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use oagw::config::OagwConfig;
use oagw::credstore_client::SharedCredentialStore;
use oagw::domain::model::{
    Endpoint, HttpMatch, MatchRule, PluginBinding, PluginDefinition, PluginSet, PluginType,
    Protocol, Route, Scheme, Upstream,
};
use oagw::domain::ratelimit::RateLimiter;
use oagw::domain::service::Service;
use oagw::domain::store::Store;
use oagw::infra::outbound::Outbound;
use oagw::domain::plugin::ControlPlane;
use oagw::infra::token_cache::TokenCache;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use tower::ServiceExt;

// ── fixtures ────────────────────────────────────────────────────────────────

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

/// A mock upstream: `/echo/json` reflects the request, `/sse` streams three
/// chunked events, `/ws` performs a WebSocket echo and `/ws-denied` refuses the
/// switch.
async fn mock_upstream() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock upstream");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve_upstream(socket));
        }
    });
    (port, handle)
}

async fn serve_upstream(mut socket: TcpStream) {
    let mut head = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        if socket.read_exact(&mut byte).await.is_err() {
            return;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head);
    let request_line = text.lines().next().unwrap_or_default();
    let path = request_line.split(' ').nth(1).unwrap_or_default();
    let path = path.split('?').next().unwrap_or_default();
    match path {
        "/echo/json" => {
            respond(
                &mut socket,
                "200 OK",
                "application/json",
                b"{\"served\":\"by the upstream\"}",
            )
            .await;
        }
        "/sse" => {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            for index in 0..3 {
                let event = format!("data: event-{index}\n\n");
                socket
                    .write_all(format!("{:x}\r\n{}\r\n", event.len(), event).as_bytes())
                    .await
                    .unwrap();
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        }
        "/ws" => websocket_echo(&mut socket).await,
        "/ws-denied" => {
            respond(
                &mut socket,
                "426 Upgrade Required",
                "application/json",
                b"{\"error\":\"no switch\"}",
            )
            .await;
        }
        _ => respond(&mut socket, "404 Not Found", "application/json", b"{}").await,
    }
}

async fn respond(socket: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();
}

/// Complete a WebSocket handshake and echo every frame back verbatim.
async fn websocket_echo(socket: &mut TcpStream) {
    let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
    socket
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Accept: ",
        )
        .await
        .unwrap();
    socket.write_all(accept.as_bytes()).await.unwrap();
    socket.write_all(b"\r\n\r\n").await.unwrap();
    loop {
        let Some(frame) = read_frame(socket).await else {
            return;
        };
        let (opcode, payload) = frame;
        if opcode == 8 {
            write_frame(socket, 8, &payload).await;
            return;
        }
        write_frame(socket, opcode, &payload).await;
    }
}

async fn read_frame(socket: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut first = [0_u8; 2];
    if socket.read_exact(&mut first).await.is_err() {
        return None;
    }
    let opcode = first[0] & 0x0F;
    let masked = first[1] & 0x80 != 0;
    let mut length = u64::from(first[1] & 0x7F);
    if length == 126 {
        let mut extended = [0_u8; 2];
        socket.read_exact(&mut extended).await.unwrap();
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        socket.read_exact(&mut extended).await.unwrap();
        length = u64::from_be_bytes(extended);
    }
    let mut mask = [0_u8; 4];
    if masked {
        socket.read_exact(&mut mask).await.ok()?;
    }
    let mut payload = vec![0_u8; length as usize];
    if length > 0 {
        socket.read_exact(&mut payload).await.ok()?;
    }
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Some((opcode, payload))
}

async fn write_frame(socket: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mut frame = vec![0x80 | opcode];
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() < 65_536 {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    socket.write_all(&frame).await.unwrap();
}

/// The gateway: a real `Service` pointed at the mock upstream.
struct Harness {
    service: Arc<Service>,
    tenant_id: uuid::Uuid,
}

async fn harness(port: u16) -> Harness {
    let credential_store: SharedCredentialStore =
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(Vec::new()));
    let service = Service::new(
        Store::new(),
        Arc::new(ControlPlane::with_builtins(
            credential_store.clone(),
            Arc::new(TokenCache::new(64)),
        )),
        credential_store,
        None,
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
        Arc::new(RateLimiter::new()),
    );
    let tenant_id = uuid::Uuid::new_v4();
    let security = security_of(tenant_id);

    let plugin = service
        .create_plugin(
            &tenant_id,
            PluginDefinition {
                id: uuid::Uuid::new_v4(),
                tenant_id,
                plugin_ref: String::new(),
                plugin_type: PluginType::Transform,
                name: "x-added".to_owned(),
                description: String::new(),
                config: serde_json::json!({ "add": { "x-added": "yes" } }),
                config_schema: None,
                source_code: None,
                version: 1,
                enabled: true,
                created_at: None,
            },
        )
        .unwrap();

    let upstream = Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id,
        enabled: true,
        alias: "mock.upstream.test".to_owned(),
        tags: Vec::new(),
        server: server_config(port),
        protocol: Protocol::Http,
        auth: None,
        headers: Default::default(),
        plugins: Default::default(),
        rate_limit: None,
        cors: None,
        created_at: None,
        updated_at: None,
    };
    let upstream = service.create_upstream(&security, upstream).await.unwrap();

    let route = Route {
        id: uuid::Uuid::new_v4(),
        tenant_id,
        enabled: true,
        tags: Vec::new(),
        upstream_id: upstream.id,
        match_rule: MatchRule::Http(HttpMatch {
            methods: vec!["GET".to_owned(), "POST".to_owned()],
            path: "/".to_owned(),
            query_allowlist: vec!["model".to_owned()],
            path_suffix_mode: Default::default(),
        }),
        plugins: PluginSet {
            sharing: Default::default(),
            items: vec![PluginBinding {
                plugin_ref: format!("gts.cf.core.oagw.transform_plugin.v1~{}", plugin.id),
                config: None,
            }],
        },
        rate_limit: None,
        created_at: None,
        updated_at: None,
    };
    service.create_route(&security, route.clone()).unwrap();

    Harness {
        service,
        tenant_id,
    }
}

fn server_config(port: u16) -> oagw::domain::model::ServerConfig {
    oagw::domain::model::ServerConfig {
        endpoints: vec![Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port,
        }],
    }
}

fn security_of(tenant_id: uuid::Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

/// The router with the harness's state and security context layered in.
fn app(harness: &Harness) -> axum::Router {
    let state = oagw::api::ApiState {
        service: harness.service.clone(),
        outbound: Arc::new(Outbound::new(
            4,
            Duration::from_secs(2),
            Duration::from_secs(5),
        )),
    };
    let router = axum::Router::new();
    let router = oagw::api::routes::register_routes(router, &NoopOpenApiRegistry, state);
    router.layer(axum::Extension(security_of(harness.tenant_id)))
}

async fn get(app: axum::Router, path: &str) -> Response {
    app.oneshot(
        Request::builder()
            .method("GET")
            .uri(path)
            .header(header::HOST, "gateway.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn body_of(response: Response) -> bytes::Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_proxy_request_reaches_the_upstream_with_the_plugin_applied() {
    let (port, upstream) = mock_upstream().await;
    let harness = harness(port).await;
    let response = get(app(&harness), "/oagw/v1/proxy/mock.upstream.test/echo/json").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-added"],
        "yes",
        "the route's transform plugin runs on the response"
    );
    assert_eq!(response.headers()["x-oagw-error-source"], "upstream");
    let body = body_of(response).await;
    assert_eq!(body.as_ref(), b"{\"served\":\"by the upstream\"}");
    upstream.abort();
}

#[tokio::test]
async fn an_unknown_alias_is_not_found() {
    let (port, upstream) = mock_upstream().await;
    let harness = harness(port).await;
    let response = get(app(&harness), "/oagw/v1/proxy/no.such.alias/echo/json").await;
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    upstream.abort();
}

#[tokio::test]
async fn a_disallowed_query_parameter_is_rejected() {
    let (port, upstream) = mock_upstream().await;
    let harness = harness(port).await;
    let response =
        get(app(&harness), "/oagw/v1/proxy/mock.upstream.test/echo/json?secret=1").await;
    assert_eq!(response.status(), 400);
    let body = body_of(response).await;
    assert!(String::from_utf8_lossy(&body).contains("secret"));
    upstream.abort();
}

#[tokio::test]
async fn a_server_sent_event_stream_arrives_chunk_by_chunk() {
    let (port, upstream) = mock_upstream().await;
    let harness = harness(port).await;
    let response = get(app(&harness), "/oagw/v1/proxy/mock.upstream.test/sse").await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        events.push(String::from_utf8_lossy(&chunk.unwrap()).into_owned());
    }
    let all = events.concat();
    assert!(all.contains("data: event-0"), "streamed: {all}");
    assert!(all.contains("data: event-2"), "streamed: {all}");
    upstream.abort();
}

/// The upgrade path: the gateway negotiates the switch with the upstream on the
/// raw transport and then pumps the session byte for byte.
#[tokio::test]
async fn a_websocket_upgrade_is_bridged_to_the_upstream() {
    let (port, upstream) = mock_upstream().await;
    let harness = harness(port).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(&harness);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            b"GET /oagw/v1/proxy/mock.upstream.test/ws HTTP/1.1\r\n\
              Host: gateway.test\r\n\
              Authorization: Bearer ignored\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();

    let head = read_head(&mut socket).await;
    assert!(
        head.contains("101 Switching Protocols"),
        "expected the switch, got: {head}"
    );
    assert!(
        head.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "the upstream's accept travels back: {head}"
    );

    let message = "echoed through the gateway".as_bytes();
    write_frame(&mut socket, 1, message).await;
    let (_, payload) = read_frame(&mut socket).await.unwrap();
    assert_eq!(payload, message);

    // A frame large enough to span several reads on both hops.
    let large = vec![7_u8; 70_000];
    write_frame(&mut socket, 2, &large).await;
    let (_, echoed) = read_frame(&mut socket).await.unwrap();
    assert_eq!(echoed, large);

    write_frame(&mut socket, 8, b"").await;
    socket.shutdown().await.unwrap();
    server.abort();
    upstream.abort();
}

#[tokio::test]
async fn a_declined_upgrade_is_relayed_as_a_plain_response() {
    let (port, upstream) = mock_upstream().await;
    let harness = harness(port).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(&harness);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            b"GET /oagw/v1/proxy/mock.upstream.test/ws-denied HTTP/1.1\r\n\
              Host: gateway.test\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();

    let head = read_head(&mut socket).await;
    assert!(head.contains("426 Upgrade Required"), "got: {head}");
    assert_eq!(read_body_of(&mut socket, &head).await, b"{\"error\":\"no switch\"}");
    server.abort();
    upstream.abort();
}

/// Read a response head, headers included, without consuming the body.
async fn read_head(socket: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// The body of a response whose head carried `Content-Length`.
async fn read_body_of(socket: &mut TcpStream, head: &str) -> Vec<u8> {
    let length = head
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 {
        socket.read_exact(&mut body).await.unwrap();
    }
    body
}

