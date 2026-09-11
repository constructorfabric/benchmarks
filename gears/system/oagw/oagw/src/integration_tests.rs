//! End-to-end tests over the real router.
//!
//! These drive the registered Axum routes, not the services behind them, so
//! they cover the wire contract: paths, status codes, response bodies,
//! validation, error semantics and the three proxying modes (plain HTTP,
//! server-sent events, WebSocket). The upstream is a purpose-built HTTP/1.1
//! server in [`upstream`] rather than a mocking library, because the SSE and
//! WebSocket cases need byte-level control over framing and timing.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::handlers::AppState;
use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, BASIC_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, PROTOCOL_HTTP,
    REQUEST_ID_TRANSFORM_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID,
};
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::{
    PluginRepository, PluginUsageRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::services::management::ControlPlane;
use crate::domain::tenant::TenantChain;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::DataPlane;
use crate::infra::storage::{
    MemoryPluginRepo, MemoryPluginUsageRepo, MemoryRouteRepo, MemoryStore, MemoryUpstreamRepo,
};
use crate::infra::tenant_dir::StaticTenantChain;

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

mod upstream {
    //! A minimal HTTP/1.1 upstream with the exact behaviours the proxy path
    //! needs to be tested against.

    use std::net::SocketAddr;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Start the upstream on an ephemeral port and return its address.
    pub async fn spawn() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle(stream));
            }
        });
        addr
    }

    /// Read one request and answer it.
    async fn handle(mut stream: TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        // Read until the header block is complete.
        let head_end = loop {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(index) = find_head_end(&buffer) {
                break index;
            }
        };

        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_owned();
        let mut method = request_line.split_whitespace();
        let verb = method.next().unwrap_or_default().to_owned();
        let target = method.next().unwrap_or_default().to_owned();

        let mut content_length = 0_usize;
        let mut header_lines = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_owned();
                if name == "content-length" {
                    content_length = value.parse().unwrap_or(0);
                }
                header_lines.push((name, value));
            }
        }

        // Drain the declared body.
        let mut body = buffer[head_end..].to_vec();
        while body.len() < content_length {
            let Ok(read) = stream.read(&mut chunk).await else {
                break;
            };
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }

        let path = target.split('?').next().unwrap_or("/").to_owned();
        let query = target.split_once('?').map(|(_, q)| q.to_owned());

        match path.as_str() {
            "/ws" | "/v1/ws" => {
                websocket_echo(stream, &header_lines).await;
            }
            "/sse" | "/v1/sse" => {
                let head = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Transfer-Encoding: chunked\r\n\
                    Connection: close\r\n\r\n";
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for index in 0..3 {
                    let event = format!("data: event-{index}\n\n");
                    let framed = format!("{:x}\r\n{event}\r\n", event.len());
                    if stream.write_all(framed.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = stream.flush().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let _ = stream.write_all(b"0\r\n\r\n").await;
                let _ = stream.shutdown().await;
            }
            "/slow" | "/v1/slow" => {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "/no-content-type" | "/v1/no-content-type" => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = stream.shutdown().await;
            }
            "/teapot" | "/v1/teapot" => {
                let body = br#"{"error":"i am a teapot"}"#;
                let head = format!(
                    "HTTP/1.1 418 I'm a teapot\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            }
            _ => {
                // Echo everything the proxy sent, so a test can assert on the
                // method, path, query, headers and body the upstream saw.
                let echo = serde_json::json!({
                    "method": verb,
                    "path": path,
                    "query": query,
                    "headers": header_lines
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeMap<String, String>>(),
                    "body": String::from_utf8_lossy(&body),
                });
                let payload = serde_json::to_vec(&echo).unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     X-Upstream-Marker: seen\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&payload).await;
                let _ = stream.shutdown().await;
            }
        }
    }

    /// Complete a WebSocket handshake, then echo every byte back.
    async fn websocket_echo(mut stream: TcpStream, headers: &[(String, String)]) {
        let key = headers
            .iter()
            .find(|(name, _)| name == "sec-websocket-key")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        // The accept value is not verified by the proxy (it is relayed), so a
        // deterministic echo of the offered key is enough to assert that the
        // client's key travelled all the way through.
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: accept-for-{key}\r\n\r\n"
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        let mut chunk = [0_u8; 1024];
        while let Ok(read) = stream.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            if stream.write_all(&chunk[..read]).await.is_err() {
                break;
            }
            let _ = stream.flush().await;
        }
    }

    /// Index just past the `\r\n\r\n` that ends a header block.
    fn find_head_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Tenant identities the tests use.
const ROOT_TENANT: Uuid = Uuid::from_u128(0x1111_1111_0000_0000_0000_0000_0000_0001);
const LEAF_TENANT: Uuid = Uuid::from_u128(0x1111_1111_0000_0000_0000_0000_0000_0002);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x1111_1111_0000_0000_0000_0000_0000_0003);

struct Harness {
    router: Router,
    state: Arc<AppState>,
}

fn security_context(tenant: Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x2222_2222_0000_0000_0000_0000_0000_0001))
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

fn test_config() -> OagwConfig {
    OagwConfig {
        // Loopback upstreams are the point of these tests.
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            allow_private_networks: false,
        },
        proxy_timeout_secs: 2,
        connect_timeout_secs: 2,
        idle_timeout_secs: 2,
        ..OagwConfig::default()
    }
}

fn build_state(config: OagwConfig, tenants: Arc<dyn TenantChain>) -> Arc<AppState> {
    let config = Arc::new(config);
    let store = Arc::new(MemoryStore::new());
    let upstreams: Arc<dyn UpstreamRepository> =
        Arc::new(MemoryUpstreamRepo::new(Arc::clone(&store)));
    let routes: Arc<dyn RouteRepository> = Arc::new(MemoryRouteRepo::new(Arc::clone(&store)));
    let plugins: Arc<dyn PluginRepository> = Arc::new(MemoryPluginRepo::new(Arc::clone(&store)));
    let usage: Arc<dyn PluginUsageRepository> =
        Arc::new(MemoryPluginUsageRepo::new(Arc::clone(&store)));

    let credstore = Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
        vec![("openai-key".to_owned(), "sk-test-e2e-fake-key".to_owned())],
    ));
    let registries = Arc::new(PluginRegistries::with_builtins(
        credstore,
        None,
        config.token_cache(),
    ));
    let catalog: Arc<dyn PluginCatalog> = Arc::clone(&registries) as Arc<dyn PluginCatalog>;

    let control = Arc::new(ControlPlane::new(
        upstreams, routes, plugins, usage, tenants, catalog, 86_400,
    ));
    let data_plane = Arc::new(DataPlane::new(
        Arc::clone(&control),
        registries,
        Arc::new(OagwMetrics::from_global()),
        Arc::clone(&config),
    ));
    Arc::new(AppState {
        control,
        data_plane,
        max_body_bytes: config.max_body_bytes,
    })
}

impl Harness {
    /// A single-tenant harness.
    fn new() -> Self {
        Self::with_hierarchy(Arc::new(StaticTenantChain::new()), test_config())
    }

    /// A harness with an explicit tenant hierarchy and configuration.
    fn with_hierarchy(tenants: Arc<dyn TenantChain>, config: OagwConfig) -> Self {
        let state = build_state(config, tenants);
        let openapi = toolkit::api::OpenApiRegistryImpl::new();
        let router = crate::api::rest::register_routes(Router::new(), &openapi, Arc::clone(&state));
        Self { router, state }
    }

    /// Send a request as `tenant`.
    async fn send(
        &self,
        tenant: Uuid,
        request: Request<Body>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut request = request;
        request.extensions_mut().insert(security_context(tenant));
        request.extensions_mut().insert(Arc::clone(&self.state));
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("body")
            .to_vec();
        (status, headers, body)
    }

    /// Send a JSON request and parse the JSON response.
    async fn json(
        &self,
        tenant: Uuid,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&value).expect("serialize"))
            }
            None => Body::empty(),
        };
        let (status, _headers, bytes) = self
            .send(tenant, builder.body(body).expect("request"))
            .await;
        let parsed = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes).into_owned()}))
        };
        (status, parsed)
    }
}

/// A minimal, valid upstream body pointing at `addr`.
fn upstream_body(addr: SocketAddr, alias: &str) -> Value {
    json!({
        "alias": alias,
        "protocol": PROTOCOL_HTTP,
        "server": {
            "endpoints": [
                {"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}
            ]
        }
    })
}

/// A route body matching everything under `/` for the common verbs.
fn route_body(upstream_id: &Value, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                "path": path,
                "query_allowlist": ["model", "q"],
                "path_suffix_mode": "append"
            }
        }
    })
}

/// Create an upstream + a catch-all route, returning the upstream id.
async fn provision(harness: &Harness, addr: SocketAddr, alias: &str) -> Value {
    let (status, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(addr, alias)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let (status, route) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    upstream["id"].clone()
}

// ---------------------------------------------------------------------------
// Management API — upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_crud_round_trip() {
    let harness = Harness::new();
    let (status, created) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 443}]},
                "tags": ["llm", "openai"]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["alias"], "api.openai.com", "alias auto-derived");
    assert_eq!(created["enabled"], true, "enabled defaults to true");
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, fetched) = harness
        .json(
            LEAF_TENANT,
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    // The anonymous GTS identifier addresses the same resource.
    let (status, by_gts) = harness
        .json(
            LEAF_TENANT,
            "GET",
            &format!("/oagw/v1/upstreams/gts.cf.core.oagw.upstream.v1~{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_gts["id"], created["id"]);

    let (status, listed) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/upstreams", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"].as_array().expect("items").len(), 1);

    let (status, replaced) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 443}]},
                "enabled": false
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["enabled"], false);
    assert!(
        replaced["tags"].as_array().expect("tags").is_empty(),
        "a full replacement clears omitted optional members"
    );

    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plaintext_scheme_is_accepted_at_create_time() {
    let harness = Harness::new();
    let (status, created) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "local-service",
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 80}]}
            })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "`http` is a legal scheme for the field: {created}"
    );
    assert_eq!(created["server"]["endpoints"][0]["scheme"], "http");
}

#[tokio::test]
async fn alias_conflict_is_a_409_and_alias_is_per_tenant() {
    let harness = Harness::new();
    let body = json!({
        "protocol": PROTOCOL_HTTP,
        "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
    });
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1"
    );

    let (status, _) = harness
        .json(OTHER_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a different tenant may use the same alias"
    );
}

#[tokio::test]
async fn upstream_validation_rejects_bad_input() {
    let harness = Harness::new();
    let cases: Vec<(&str, Value)> = vec![
        (
            "unknown member",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "a.example.com"}]},
                "nope": true
            }),
        ),
        ("missing server", json!({"protocol": PROTOCOL_HTTP})),
        (
            "empty endpoint pool",
            json!({"protocol": PROTOCOL_HTTP, "server": {"endpoints": []}}),
        ),
        (
            "unknown protocol",
            json!({
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.telepathy.v1",
                "server": {"endpoints": [{"scheme": "https", "host": "a.example.com"}]}
            }),
        ),
        (
            "malformed hostname",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "-bad-.example.com"}]}
            }),
        ),
        (
            "mixed schemes in one pool",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "https", "host": "us.vendor.com", "port": 443},
                    {"scheme": "wss", "host": "eu.vendor.com", "port": 443}
                ]}
            }),
        ),
        (
            "mixed ports in one pool",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "https", "host": "us.vendor.com", "port": 443},
                    {"scheme": "https", "host": "eu.vendor.com", "port": 8443}
                ]}
            }),
        ),
        (
            "user-provided alias for a derivable pool",
            json!({
                "alias": "my-openai",
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            }),
        ),
        (
            "missing alias for an IP pool",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "10.0.1.1"}]}
            }),
        ),
        (
            "unknown auth plugin",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "auth": {"type": BASIC_AUTH_PLUGIN_ID}
            }),
        ),
        (
            "non-bindable guard plugin",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "plugins": {"items": [TIMEOUT_GUARD_PLUGIN_ID]}
            }),
        ),
        (
            "credentialed wildcard CORS",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "cors": {"enabled": true, "allowed_origins": ["*"], "allow_credentials": true}
            }),
        ),
        (
            "zero rate",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "rate_limit": {"sustained": {"rate": 0}}
            }),
        ),
        (
            "malformed tag",
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "tags": ["Not A Tag"]
            }),
        ),
    ];

    for (name, body) in cases {
        let (status, problem) = harness
            .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {problem}");
        assert_eq!(
            problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            "{name}"
        );
    }
}

#[tokio::test]
async fn unknown_auth_plugin_detail_names_the_plugin() {
    let harness = Harness::new();
    let (_status, problem) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "auth": {"type": BASIC_AUTH_PLUGIN_ID}
            })),
        )
        .await;
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("unknown auth plugin"), "{detail}");
}

#[tokio::test]
async fn implemented_auth_plugins_are_accepted() {
    let harness = Harness::new();
    for plugin in [NOOP_AUTH_PLUGIN_ID, APIKEY_AUTH_PLUGIN_ID] {
        let (status, body) = harness
            .json(
                LEAF_TENANT,
                "POST",
                "/oagw/v1/upstreams",
                Some(json!({
                    "protocol": PROTOCOL_HTTP,
                    "server": {"endpoints": [
                        {"scheme": "https", "host": format!("{}.example.com", plugin.len())}
                    ]},
                    "auth": {"type": plugin, "config": {"secret_ref": "cred://openai-key"}}
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{plugin}: {body}");
    }
}

#[tokio::test]
async fn alias_is_immutable_across_endpoint_changes() {
    let harness = Harness::new();
    let (_, created) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.anthropic.com"}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("immutable"),
        "{problem}"
    );

    // Re-stating the same endpoints is fine.
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn management_api_is_tenant_scoped() {
    let harness = Harness::new();
    let (_, created) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;
    let id = created["id"].as_str().expect("id").to_owned();

    for (method, expected) in [
        ("GET", StatusCode::NOT_FOUND),
        ("DELETE", StatusCode::NOT_FOUND),
    ] {
        let (status, _) = harness
            .json(
                OTHER_TENANT,
                method,
                &format!("/oagw/v1/upstreams/{id}"),
                None,
            )
            .await;
        assert_eq!(status, expected, "{method} from another tenant");
    }
    let (status, listed) = harness
        .json(OTHER_TENANT, "GET", "/oagw/v1/upstreams", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed["items"].as_array().expect("items").is_empty());
}

#[tokio::test]
async fn list_supports_the_documented_query_options() {
    let harness = Harness::new();
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        harness
            .json(
                LEAF_TENANT,
                "POST",
                "/oagw/v1/upstreams",
                Some(json!({
                    "protocol": PROTOCOL_HTTP,
                    "server": {"endpoints": [{"scheme": "https", "host": host}]},
                    "tags": ["shared"]
                })),
            )
            .await;
    }

    let (status, filtered) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20'b.example.com'",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert_eq!(filtered["items"].as_array().expect("items").len(), 1);
    assert_eq!(filtered["items"][0]["alias"], "b.example.com");

    let (_, ordered) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/upstreams?$orderby=alias%20desc",
            None,
        )
        .await;
    assert_eq!(ordered["items"][0]["alias"], "c.example.com");

    let (_, paged) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/upstreams?$top=2&$skip=1",
            None,
        )
        .await;
    assert_eq!(paged["items"].as_array().expect("items").len(), 2);

    let (_, projected) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/upstreams?$select=alias", None)
        .await;
    assert_eq!(projected["items"][0].as_object().expect("object").len(), 1);

    let (status, _) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/upstreams?$top=1000", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "$top is capped at 100");
}

// ---------------------------------------------------------------------------
// Management API — routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_crud_and_validation() {
    let harness = Harness::new();
    let (_, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;
    let upstream_id = upstream["id"].clone();

    let (status, route) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "match": {"http": {"methods": ["get", "POST"], "path": "v1/chat"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    assert_eq!(
        route["match"]["http"]["path"], "/v1/chat",
        "path normalized"
    );
    assert_eq!(
        route["match"]["http"]["methods"][0], "GET",
        "verb normalized"
    );
    assert_eq!(route["enabled"], true);
    let route_id = route["id"].as_str().expect("id").to_owned();

    // Same path + priority + method → 409.
    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "match": {"http": {"methods": ["GET"], "path": "/v1/chat"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // A different priority is a distinct rule.
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "priority": 10,
                "match": {"http": {"methods": ["GET"], "path": "/v1/chat"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // `upstream_id` is immutable.
    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(json!({
                "upstream_id": Uuid::new_v4(),
                "match": {"http": {"methods": ["GET"], "path": "/v1/chat"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");

    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "DELETE",
            &format!("/oagw/v1/routes/{route_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn route_rejects_an_unknown_or_foreign_upstream() {
    let harness = Harness::new();
    let (_, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;

    // Unknown upstream → 400 ValidationError (not 404).
    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": Uuid::new_v4(),
                "match": {"http": {"methods": ["GET"], "path": "/v1"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");

    // Another tenant's upstream is not addressable either.
    let (status, _) = harness
        .json(
            OTHER_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream["id"],
                "match": {"http": {"methods": ["GET"], "path": "/v1"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn route_match_must_name_exactly_one_protocol() {
    let harness = Harness::new();
    let (_, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;

    for body in [
        json!({"upstream_id": upstream["id"], "match": {}}),
        json!({
            "upstream_id": upstream["id"],
            "match": {
                "http": {"methods": ["GET"], "path": "/v1"},
                "grpc": {"service": "s", "method": "m"}
            }
        }),
        json!({
            "upstream_id": upstream["id"],
            "match": {"grpc": {"service": "s", "method": "m"}}
        }),
        json!({
            "upstream_id": upstream["id"],
            "match": {"http": {"methods": ["TRACE"], "path": "/v1"}}
        }),
        json!({
            "upstream_id": upstream["id"],
            "match": {"http": {"methods": [], "path": "/v1"}}
        }),
    ] {
        let (status, problem) = harness
            .json(LEAF_TENANT, "POST", "/oagw/v1/routes", Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {problem}");
    }
}

// ---------------------------------------------------------------------------
// Management API — plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_lifecycle_and_in_use_conflict() {
    let harness = Harness::new();
    let source = "def on_request(ctx):\n    return ctx.next()\n";
    let (status, plugin) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "name": "request_validator",
                "description": "Validates request headers",
                "plugin_type": "guard",
                "phases": ["on_request"],
                "config_schema": {"type": "object"},
                "source_code": source
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    let plugin_ref = plugin["id"].as_str().expect("id").to_owned();
    assert!(
        plugin_ref.starts_with("gts.cf.core.oagw.guard_plugin.v1~"),
        "{plugin_ref}"
    );

    // The source endpoint serves the raw script.
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri(format!("/oagw/v1/plugins/{plugin_ref}/source"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), source);

    // A duplicate name conflicts.
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "name": "request_validator",
                "plugin_type": "guard",
                "source_code": source
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Bind it to an upstream, then deletion must be refused.
    let (status, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
                "plugins": {"items": [{"plugin_ref": plugin_ref}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");

    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    assert_eq!(problem["plugin_id"], plugin_ref);
    assert_eq!(
        problem["referenced_by"]["upstreams"]
            .as_array()
            .expect("upstreams")
            .len(),
        1
    );

    // Unbind, then deletion succeeds.
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_validation_rejects_bad_input() {
    let harness = Harness::new();
    for body in [
        json!({"name": "", "plugin_type": "guard", "source_code": "x"}),
        json!({"name": "n", "plugin_type": "wizard", "source_code": "x"}),
        json!({"name": "n", "plugin_type": "guard", "source_code": ""}),
        json!({
            "name": "n",
            "plugin_type": "guard",
            "source_code": "x",
            "phases": ["on_teatime"]
        }),
    ] {
        let (status, problem) = harness
            .json(LEAF_TENANT, "POST", "/oagw/v1/plugins", Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {problem}");
    }
}

// ---------------------------------------------------------------------------
// Proxy — plain HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxies_a_get_request_and_relays_the_response() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let (status, headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/things?model=gpt-4")
                .header("x-custom", "forwarded")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|v| v.to_str().unwrap()),
        Some("upstream"),
        "a relayed response is tagged as upstream-sourced"
    );
    assert_eq!(
        headers
            .get("x-upstream-marker")
            .map(|v| v.to_str().unwrap()),
        Some("seen"),
        "upstream response headers pass through"
    );

    let echo: Value = serde_json::from_slice(&body).expect("echo json");
    assert_eq!(echo["method"], "GET");
    assert_eq!(echo["path"], "/v1/things");
    assert_eq!(echo["query"], "model=gpt-4");
    assert_eq!(echo["headers"]["x-custom"], "forwarded");
    assert_eq!(
        echo["headers"]["host"],
        format!("{}:{}", addr.ip(), addr.port()),
        "Host is replaced by the upstream authority"
    );
}

#[tokio::test]
async fn proxies_a_post_body_and_strips_the_platform_bearer() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let payload = json!({"model": "gpt-4", "messages": []});
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("POST")
                .uri("/oagw/v1/proxy/echo-service/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer platform-token")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let echo: Value = serde_json::from_slice(&body).expect("echo json");
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["path"], "/v1/chat/completions");
    assert_eq!(echo["body"], serde_json::to_string(&payload).unwrap());
    assert_eq!(echo["headers"]["content-type"], "application/json");
    assert!(
        echo["headers"].get("authorization").is_none(),
        "the caller's platform bearer must not reach the upstream: {echo}"
    );
}

#[tokio::test]
async fn upstream_error_status_is_relayed_untouched() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let (status, headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/teapot")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::IM_A_TEAPOT);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|v| v.to_str().unwrap()),
        Some("upstream")
    );
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"error":"i am a teapot"}"#,
        "the upstream body is passed through as-is"
    );
}

#[tokio::test]
async fn unknown_alias_is_a_route_not_found() {
    let harness = Harness::new();
    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/nope/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(problem["instance"], "/oagw/v1/proxy/nope/v1/x");
}

#[tokio::test]
async fn alias_resolution_is_case_insensitive() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;
    let (status, _) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/Echo-Service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn no_matching_route_is_a_404() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let (_, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(addr, "echo-service")),
        )
        .await;
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream["id"],
                "match": {"http": {"methods": ["GET"], "path": "/v1/allowed"}}
            })),
        )
        .await;

    // Path outside the route prefix.
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/echo-service/v1/other",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Method the route does not allow.
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "DELETE",
            "/oagw/v1/proxy/echo-service/v1/allowed",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Prefix matching respects segment boundaries.
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/echo-service/v1/allowed-but-not-really",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn disabled_upstream_is_a_503_and_disabled_route_stops_matching() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let (_, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(addr, "echo-service")),
        )
        .await;
    let (_, route) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let route_id = route["id"].as_str().unwrap().to_owned();

    // Disable the route: matching skips it.
    let mut disabled_route = route_body(&upstream["id"], "/");
    disabled_route["enabled"] = json!(false);
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(disabled_route),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a disabled route cannot match"
    );

    // Disable the upstream: every request is refused with 503, which takes
    // precedence over route matching.
    let mut disabled_upstream = upstream_body(addr, "echo-service");
    disabled_upstream["enabled"] = json!(false);
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(disabled_upstream),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
}

#[tokio::test]
async fn query_allowlist_is_enforced() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/echo-service/v1/x?model=gpt-4",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "an allowlisted parameter passes");

    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/echo-service/v1/x?secret=1",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["query_parameter"], "secret");
}

#[tokio::test]
async fn path_suffix_mode_disabled_rejects_a_suffix() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let (_, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(addr, "echo-service")),
        )
        .await;
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream["id"],
                "match": {
                    "http": {
                        "methods": ["GET"],
                        "path": "/v1/exact",
                        "path_suffix_mode": "disabled"
                    }
                }
            })),
        )
        .await;

    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/echo-service/v1/exact",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the exact path is allowed");

    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/echo-service/v1/exact/more",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn content_length_mismatch_is_rejected() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    // A POST with no body and no Content-Length is well-formed.
    let (status, echoed) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/proxy/echo-service/v1/x",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{echoed}");

    // A Content-Length that disagrees with the body received is a smuggling
    // signature and is refused.
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("POST")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::CONTENT_LENGTH, "999")
                .body(Body::from("short"))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn oversized_content_length_is_a_413() {
    let addr = upstream::spawn().await;
    let mut config = test_config();
    config.max_body_bytes = 16;
    let harness = Harness::with_hierarchy(Arc::new(StaticTenantChain::new()), config);
    provision(&harness, addr, "echo-service").await;

    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("POST")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("0123456789012345678901234567890123456789"))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let problem: Value = serde_json::from_slice(&body).expect("problem json");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[tokio::test]
async fn unsupported_transfer_encoding_is_rejected() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let (status, _headers, _body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("POST")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::TRANSFER_ENCODING, "gzip")
                .body(Body::from("payload"))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upstream_timeout_is_a_504() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/slow", None)
        .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
    assert!(problem["retry_after_seconds"].is_number());
}

#[tokio::test]
async fn unreachable_upstream_is_a_502() {
    // Bind and immediately drop the listener so the port is (almost certainly)
    // closed: a connect failure must surface as a gateway 502, never a panic.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let harness = Harness::new();
    provision(&harness, addr, "dead-service").await;
    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/dead-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
}

#[tokio::test]
async fn plaintext_upstream_needs_the_flag() {
    let addr = upstream::spawn().await;
    let mut config = test_config();
    config.allow_http_upstream = false;
    let harness = Harness::with_hierarchy(Arc::new(StaticTenantChain::new()), config);
    provision(&harness, addr, "echo-service").await;

    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the scheme is accepted at create time but the connection is refused: {problem}"
    );
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("allow_http_upstream"),
        "{problem}"
    );
}

#[tokio::test]
async fn ssrf_guard_refuses_a_loopback_upstream_when_enabled() {
    let addr = upstream::spawn().await;
    let mut config = test_config();
    config.ssrf_policy.enabled = true;
    let harness = Harness::with_hierarchy(Arc::new(StaticTenantChain::new()), config);
    provision(&harness, addr, "echo-service").await;

    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

// ---------------------------------------------------------------------------
// Proxy — headers, auth, guards, transforms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn header_rules_are_applied_to_the_outbound_request() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["headers"] = json!({
        "request": {
            "set": {"x-set": "fixed"},
            "add": {"x-added": "extra"},
            "remove": ["x-drop"],
            "passthrough": "allowlist",
            "passthrough_allowlist": ["x-keep"]
        },
        "response": {
            "set": {"x-response-set": "yes"},
            "remove": ["x-upstream-marker"]
        }
    });
    let (status, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, headers, response_body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header("x-keep", "kept")
                .header("x-drop", "dropped")
                .header("x-other", "not-allowlisted")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let echo: Value = serde_json::from_slice(&response_body).expect("echo");
    assert_eq!(echo["headers"]["x-set"], "fixed");
    assert_eq!(echo["headers"]["x-added"], "extra");
    assert_eq!(echo["headers"]["x-keep"], "kept");
    assert!(echo["headers"].get("x-drop").is_none(), "{echo}");
    assert!(echo["headers"].get("x-other").is_none(), "{echo}");

    assert_eq!(
        headers.get("x-response-set").map(|v| v.to_str().unwrap()),
        Some("yes")
    );
    assert!(
        headers.get("x-upstream-marker").is_none(),
        "a response `remove` rule strips the upstream header"
    );
}

#[tokio::test]
async fn apikey_auth_plugin_injects_the_credential() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["auth"] = json!({
        "type": APIKEY_AUTH_PLUGIN_ID,
        "config": {
            "secret_ref": "cred://openai-key",
            "in": "header",
            "name": "Authorization",
            "prefix": "Bearer "
        }
    });
    let (status, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, _headers, response_body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::AUTHORIZATION, "Bearer platform-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let echo: Value = serde_json::from_slice(&response_body).expect("echo");
    assert_eq!(
        echo["headers"]["authorization"], "Bearer sk-test-e2e-fake-key",
        "the plugin's credential replaces the caller's: {echo}"
    );
}

#[tokio::test]
async fn missing_secret_surfaces_as_secret_not_found() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["auth"] = json!({
        "type": APIKEY_AUTH_PLUGIN_ID,
        "config": {"secret_ref": "cred://absent-key", "in": "header", "name": "x-api-key"}
    });
    let (_, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
    assert!(
        !problem.to_string().contains("sk-test"),
        "no credential material in an error body"
    );
}

#[tokio::test]
async fn required_headers_guard_rejects_and_allows() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["plugins"] = json!({
        "items": [{
            "plugin_ref": REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "config": {"required_request_headers": "x-correlation-id"}
        }]
    });
    let (status, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["error_code"], "REQUIRED_HEADER_MISSING");

    let (status, _headers, _body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header("x-correlation-id", "abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn required_headers_guard_rejects_a_bad_response_with_502() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["plugins"] = json!({
        "items": [{
            "plugin_ref": REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "config": {"required_response_headers": "x-mandatory"}
        }]
    });
    let (_, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, problem) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(problem["error_code"], "REQUIRED_HEADER_MISSING");
}

#[tokio::test]
async fn request_id_transform_propagates_the_correlation_id() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["plugins"] = json!({"items": [REQUEST_ID_TRANSFORM_PLUGIN_ID]});
    let (status, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, headers, response_body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header("x-request-id", "req_abc123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let echo: Value = serde_json::from_slice(&response_body).expect("echo");
    assert_eq!(echo["headers"]["x-request-id"], "req_abc123");
    assert_eq!(
        headers.get("x-request-id").map(|v| v.to_str().unwrap()),
        Some("req_abc123"),
        "the id is echoed back to the caller"
    );
}

// ---------------------------------------------------------------------------
// Proxy — rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_rejects_with_429_and_retry_after() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["rate_limit"] = json!({
        "sustained": {"rate": 1, "window": "minute"},
        "burst": {"capacity": 1},
        "scope": "tenant",
        "strategy": "reject"
    });
    let (status, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    let (status, headers, _) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-ratelimit-limit")
            .map(|v| v.to_str().unwrap()),
        Some("1")
    );
    assert_eq!(
        headers
            .get("x-ratelimit-remaining")
            .map(|v| v.to_str().unwrap()),
        Some("0")
    );

    let (status, headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get(header::RETRY_AFTER).is_some(), "Retry-After");
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|v| v.to_str().unwrap()),
        Some("gateway")
    );
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(problem["retriable"], true);
    assert!(problem["retry_after_seconds"].is_number());
}

#[tokio::test]
async fn route_rate_limit_tightens_the_upstream_limit() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["rate_limit"] = json!({"sustained": {"rate": 1000, "window": "minute"}});
    let (_, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    let mut route = route_body(&upstream["id"], "/");
    route["rate_limit"] = json!({
        "sustained": {"rate": 1, "window": "minute"},
        "burst": {"capacity": 1}
    });
    let (status, created) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/routes", Some(route))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, _) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/proxy/echo-service/v1/x", None)
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the stricter route limit wins over the upstream's"
    );
}

// ---------------------------------------------------------------------------
// Proxy — CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preflight_is_answered_locally_without_an_upstream() {
    let harness = Harness::new();
    // Deliberately no upstream: a preflight must not need one.
    let (status, headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("OPTIONS")
                .uri("/oagw/v1/proxy/api.example.com/users")
                .header(header::ORIGIN, "https://app.example.com")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "Content-Type")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().unwrap()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .map(|v| v.to_str().unwrap()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_MAX_AGE)
            .map(|v| v.to_str().unwrap()),
        Some("86400")
    );
}

#[tokio::test]
async fn actual_request_origin_and_method_are_enforced() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    let mut body = upstream_body(addr, "echo-service");
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET"],
        "expose_headers": ["X-Request-ID"]
    });
    let (status, upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    // Allowed origin + method.
    let (status, headers, _) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::ORIGIN, "https://app.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().unwrap()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers.get(header::VARY).map(|v| v.to_str().unwrap()),
        Some("Origin")
    );

    // Disallowed origin.
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::ORIGIN, "https://evil.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );

    // Disallowed method.
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("DELETE")
                .uri("/oagw/v1/proxy/echo-service/v1/x")
                .header(header::ORIGIN, "https://app.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

// ---------------------------------------------------------------------------
// Proxy — X-OAGW-Target-Host
// ---------------------------------------------------------------------------

#[tokio::test]
async fn target_host_behaviour_matrix() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();

    // A multi-endpoint pool with a derived common-suffix alias. Both members
    // resolve to the loopback upstream via /etc/hosts-independent literals is
    // not possible, so the hosts are names that will not resolve — the
    // interesting behaviour is the routing decision, which happens before
    // any connection attempt.
    let (status, upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "us.vendor.invalid", "port": addr.port()},
                    {"scheme": "http", "host": "eu.vendor.invalid", "port": addr.port()}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    // The pool shares a registrable suffix on a non-standard port, so the
    // alias is derived as `suffix:port`.
    let alias = format!("vendor.invalid:{}", addr.port());
    assert_eq!(upstream["alias"], alias);
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&upstream["id"], "/")),
        )
        .await;

    // No header on a common-suffix pool → 400 MissingTargetHost.
    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/x"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(problem["valid_hosts"].as_array().expect("hosts").len(), 2);

    // Malformed header → 400 InvalidTargetHost.
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri(format!("/oagw/v1/proxy/{alias}/v1/x"))
                .header("x-oagw-target-host", "us.vendor.invalid:8443")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );

    // Unknown host → 400 UnknownTargetHost.
    let (status, _headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri(format!("/oagw/v1/proxy/{alias}/v1/x"))
                .header("x-oagw-target-host", "apac.vendor.invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
    assert_eq!(problem["invalid_value"], "apac.vendor.invalid");
}

// ---------------------------------------------------------------------------
// Proxy — hierarchy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn descendant_inherits_and_shadows_an_ancestor_upstream() {
    let addr = upstream::spawn().await;
    let hierarchy = Arc::new(StaticTenantChain::new().with_parent(LEAF_TENANT, ROOT_TENANT));
    let harness = Harness::with_hierarchy(hierarchy, test_config());

    // The root owns the upstream and a route.
    let (status, root_upstream) = harness
        .json(
            ROOT_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(addr, "shared-service")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{root_upstream}");
    harness
        .json(
            ROOT_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&root_upstream["id"], "/")),
        )
        .await;

    // The leaf can proxy through the inherited configuration...
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/shared-service/v1/x",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the alias resolves up the chain");

    // ...but cannot see the resource through the management API.
    let root_id = root_upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            &format!("/oagw/v1/upstreams/{root_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "ancestor rows are invisible");
    let (_, listed) = harness
        .json(LEAF_TENANT, "GET", "/oagw/v1/upstreams", None)
        .await;
    assert!(listed["items"].as_array().expect("items").is_empty());
}

#[tokio::test]
async fn ancestor_enforced_rate_limit_applies_across_shadowing() {
    let addr = upstream::spawn().await;
    let hierarchy = Arc::new(StaticTenantChain::new().with_parent(LEAF_TENANT, ROOT_TENANT));
    let harness = Harness::with_hierarchy(hierarchy, test_config());

    // Root enforces one request per minute.
    let mut root_body = upstream_body(addr, "shared-service");
    root_body["rate_limit"] = json!({
        "sharing": "enforce",
        "sustained": {"rate": 1, "window": "minute"},
        "burst": {"capacity": 1}
    });
    let (status, root_upstream) = harness
        .json(ROOT_TENANT, "POST", "/oagw/v1/upstreams", Some(root_body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{root_upstream}");

    // The leaf shadows the alias with a far looser limit of its own.
    let mut leaf_body = upstream_body(addr, "shared-service");
    leaf_body["rate_limit"] = json!({"sustained": {"rate": 1000, "window": "minute"}});
    let (status, leaf_upstream) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(leaf_body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{leaf_upstream}");
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&leaf_upstream["id"], "/")),
        )
        .await;

    let (status, _) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/shared-service/v1/x",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/shared-service/v1/x",
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the ancestor's enforced limit survives shadowing: {problem}"
    );
}

#[tokio::test]
async fn ancestor_disabled_upstream_disables_it_for_descendants() {
    let addr = upstream::spawn().await;
    let hierarchy = Arc::new(StaticTenantChain::new().with_parent(LEAF_TENANT, ROOT_TENANT));
    let harness = Harness::with_hierarchy(hierarchy, test_config());

    let mut root_body = upstream_body(addr, "shared-service");
    root_body["enabled"] = json!(false);
    harness
        .json(ROOT_TENANT, "POST", "/oagw/v1/upstreams", Some(root_body))
        .await;

    let (_, leaf_upstream) = harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(addr, "shared-service")),
        )
        .await;
    harness
        .json(
            LEAF_TENANT,
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&leaf_upstream["id"], "/")),
        )
        .await;

    let (status, problem) = harness
        .json(
            LEAF_TENANT,
            "GET",
            "/oagw/v1/proxy/shared-service/v1/x",
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a descendant cannot re-enable an ancestor-disabled upstream: {problem}"
    );
}

#[tokio::test]
async fn ancestor_enforced_auth_cannot_be_overridden() {
    let addr = upstream::spawn().await;
    let hierarchy = Arc::new(StaticTenantChain::new().with_parent(LEAF_TENANT, ROOT_TENANT));
    let harness = Harness::with_hierarchy(hierarchy, test_config());

    let mut root_body = upstream_body(addr, "shared-service");
    root_body["auth"] = json!({
        "type": NOOP_AUTH_PLUGIN_ID,
        "sharing": "enforce"
    });
    let (status, _) = harness
        .json(ROOT_TENANT, "POST", "/oagw/v1/upstreams", Some(root_body))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut leaf_body = upstream_body(addr, "shared-service");
    leaf_body["auth"] = json!({
        "type": APIKEY_AUTH_PLUGIN_ID,
        "config": {"secret_ref": "cred://openai-key", "name": "x-api-key"}
    });
    let (status, problem) = harness
        .json(LEAF_TENANT, "POST", "/oagw/v1/upstreams", Some(leaf_body))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("enforce"),
        "{problem}"
    );
}

// ---------------------------------------------------------------------------
// Proxy — server-sent events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxies_a_server_sent_event_stream() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let (status, headers, body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/sse")
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("text/event-stream")
    );
    assert!(
        headers.get(header::TRANSFER_ENCODING).is_none(),
        "hop-by-hop framing is not relayed"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("data: event-0"), "{text}");
    assert!(text.contains("data: event-1"), "{text}");
    assert!(text.contains("data: event-2"), "{text}");
}

#[tokio::test]
async fn sse_events_arrive_incrementally() {
    use futures_util::StreamExt;

    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    let mut request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/echo-service/sse")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(security_context(LEAF_TENANT));
    request.extensions_mut().insert(Arc::clone(&harness.state));
    let response = harness.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The upstream sleeps between events, so receiving the first frame before
    // the stream ends proves the body is streamed rather than buffered.
    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("first frame arrives promptly")
        .expect("a frame")
        .expect("no stream error");
    assert!(
        String::from_utf8_lossy(&first).contains("data: event-0"),
        "{first:?}"
    );
}

// ---------------------------------------------------------------------------
// Proxy — WebSocket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxies_a_websocket_upgrade_and_splices_the_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let upstream_addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, upstream_addr, "echo-service").await;

    // A real server is needed: the connection-upgrade handle only exists on a
    // live hyper connection, not on a router driven in-process.
    let state = Arc::clone(&harness.state);
    let router = harness.router.clone().layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let state = Arc::clone(&state);
            async move {
                request
                    .extensions_mut()
                    .insert(security_context(LEAF_TENANT));
                request.extensions_mut().insert(state);
                next.run(request).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let mut client = tokio::net::TcpStream::connect(gateway_addr).await.unwrap();
    let handshake = format!(
        "GET /oagw/v1/proxy/echo-service/ws HTTP/1.1\r\n\
         Host: {gateway_addr}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    client.write_all(handshake.as_bytes()).await.unwrap();

    let mut buffer = vec![0_u8; 1024];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buffer))
        .await
        .expect("handshake response arrives")
        .expect("read");
    let response = String::from_utf8_lossy(&buffer[..read]).into_owned();
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "expected an upgrade, got: {response}"
    );
    assert!(
        response.to_ascii_lowercase().contains("upgrade: websocket"),
        "{response}"
    );
    assert!(
        response.contains("accept-for-dGhlIHNhbXBsZSBub25jZQ=="),
        "the client's key reached the upstream and its accept came back: {response}"
    );

    // The session is spliced: bytes written by the client come back from the
    // upstream echo.
    client.write_all(b"ping-through-the-proxy").await.unwrap();
    client.flush().await.unwrap();
    let mut echo = vec![0_u8; 64];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut echo))
        .await
        .expect("echo arrives")
        .expect("read");
    assert_eq!(
        &echo[..read],
        b"ping-through-the-proxy",
        "the byte stream is spliced in both directions"
    );
}

#[tokio::test]
async fn websocket_upgrade_without_an_upgradable_connection_is_a_502() {
    let addr = upstream::spawn().await;
    let harness = Harness::new();
    provision(&harness, addr, "echo-service").await;

    // Driven in-process there is no upgrade handle, which must be reported
    // rather than hanging or panicking.
    let (status, _headers, _body) = harness
        .send(
            LEAF_TENANT,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/echo-service/ws")
                .header(header::CONNECTION, "Upgrade")
                .header(header::UPGRADE, "websocket")
                .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
                .header(header::SEC_WEBSOCKET_VERSION, "13")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
