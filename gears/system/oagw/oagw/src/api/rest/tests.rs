//! Router-level integration tests for the OAGW REST surface.
//!
//! These build the full `/oagw/v1` router (management + proxy data plane)
//! over in-memory repositories and exercise the gateway end to end against
//! real upstreams (httpmock, a loopback WebSocket echo server) via
//! [`tower::ServiceExt::oneshot`] / a live `axum::serve` listener.
//!
//! Covered areas (required by the delivery spec):
//! - HTTP round-trip through the data plane (httpmock);
//! - `apikey` auth injection from the credential store;
//! - `required_headers` guard rejection (RFC 9457 problem body);
//! - rate-limit enforcement (429 + `Retry-After` / `X-RateLimit-*`);
//! - CORS preflight echo and actual-request origin rejection;
//! - SSE (`text/event-stream`) passthrough;
//! - WebSocket echo bridging.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use httpmock::prelude::{GET, MockServer};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use credstore_sdk::CredStoreClientV1;
use credstore_sdk::test_util::MockCredStoreClient;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};

use crate::config::{OagwConfig, SsrfPolicyConfig};
use crate::domain::dto::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, HeaderRules, HttpMatch, MatchConfig,
    PassthroughMode, PluginBinding, PluginsConfig, RateLimitConfig, RateSpec, RateWindow, Route,
    ServerConfig, Upstream,
};
use crate::domain::gts_helpers::{APIKEY_AUTH_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID};
use crate::domain::services::data_plane::DataPlaneService;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::storage::MemoryRepos;

/// Test configuration: plain-`http` loopback upstreams are allowed and SSRF
/// enforcement is off so tests can target local mock servers.
fn test_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicyConfig { enabled: false },
        ..OagwConfig::default()
    }
}

/// Tenant resolver with no ancestors — every tenant is treated as a root.
struct NoAncestorsResolver;

#[async_trait::async_trait]
impl TenantResolverClient for NoAncestorsResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        unimplemented!()
    }
    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        unimplemented!()
    }
    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        unimplemented!()
    }
    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        Ok(GetAncestorsResponse {
            tenant: TenantRef {
                id: TenantId(Uuid::default()),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: None,
                self_managed: false,
            },
            ancestors: Vec::new(),
        })
    }
    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        unimplemented!()
    }
    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _parent_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        unimplemented!()
    }
}

/// Build the full router: control plane + data plane + a per-test tenant
/// security context.
#[allow(clippy::type_complexity)]
fn harness(
    credstore: Arc<dyn CredStoreClientV1>,
) -> (Router, Arc<ControlPlaneService>, SecurityContext, Uuid) {
    let config = test_config();
    let repos = MemoryRepos::new();
    let auth = Arc::new(AuthPluginRegistry::with_builtins(
        credstore,
        None,
        config.token_cache,
        repos.plugins.clone(),
    ));
    let guard = Arc::new(GuardPluginRegistry::with_builtins(repos.plugins.clone()));
    let transform = Arc::new(TransformPluginRegistry::with_builtins(
        repos.plugins.clone(),
    ));

    let control = Arc::new(ControlPlaneService::new(
        repos,
        Arc::new(NoAncestorsResolver),
        Arc::clone(&auth),
        Arc::clone(&guard),
        Arc::clone(&transform),
        config.clone(),
    ));
    let data_plane = Arc::new(DataPlaneService::new(
        Arc::clone(&control),
        auth,
        guard,
        transform,
        config,
    ));

    let tenant = Uuid::new_v4();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap();

    let router = crate::api::rest::routes::register_routes(
        Router::new(),
        &toolkit::api::OpenApiRegistryImpl::new(),
        Arc::clone(&control),
        data_plane,
    )
    .layer(axum::Extension(ctx.clone()));

    (router, control, ctx, tenant)
}

/// Create an `http`-scheme upstream on `host:port` with an explicit alias and
/// configure it through the `extra` closure.
async fn add_upstream(
    control: &ControlPlaneService,
    ctx: &SecurityContext,
    tenant: Uuid,
    host: &str,
    port: u16,
    alias: &str,
    extra: impl FnOnce(&mut Upstream),
) -> Uuid {
    let mut input = Upstream {
        alias: Some(alias.to_owned()),
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: host.to_owned(),
                port,
            }],
        },
        ..Upstream::default()
    };
    extra(&mut input);
    let created = control.create_upstream(ctx, tenant, input).await.unwrap();
    created.id.unwrap()
}

/// Create a catch-all `path` route for `methods` on the upstream.
fn add_route(
    control: &ControlPlaneService,
    tenant: Uuid,
    upstream_id: Uuid,
    methods: &[&str],
    path: &str,
    plugins: Option<PluginsConfig>,
    rate_limit: Option<RateLimitConfig>,
) {
    let route = Route {
        upstream_id,
        r#match: MatchConfig::Http {
            http: HttpMatch {
                methods: methods.iter().map(ToString::to_string).collect(),
                path: path.to_owned(),
                ..Default::default()
            },
        },
        plugins,
        rate_limit,
        ..Route::default()
    };
    control.create_route(tenant, route).unwrap();
}

/// Drive a single request through the router, returning status, headers and
/// the buffered body.
async fn send(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

#[tokio::test]
async fn http_round_trip_via_httpmock() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/widgets");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"name":"widget","count":3}"#);
    });
    let port = server.address().port();

    let (router, control, ctx, tenant) = harness(Arc::new(MockCredStoreClient::empty()));
    let up_id = add_upstream(
        &control,
        &ctx,
        tenant,
        "127.0.0.1",
        port,
        "mock-api",
        |_| {},
    )
    .await;
    add_route(&control, tenant, up_id, &["GET"], "/", None, None);

    let (status, headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/mock-api/v1/widgets")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"count\":3"), "unexpected body: {text}");
    mock.assert();
}

#[tokio::test]
async fn apikey_auth_injects_secret_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/secure")
            .header("x-api-key", "hunter2-secret");
        then.status(200).body("authorized");
    });
    let port = server.address().port();

    let credstore: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![(
            "oagw-apikey".to_owned(),
            "hunter2-secret".to_owned(),
        )]));
    let (router, control, ctx, tenant) = harness(credstore);
    let up_id = add_upstream(
        &control,
        &ctx,
        tenant,
        "127.0.0.1",
        port,
        "secured-api",
        |u| {
            u.auth = Some(AuthConfig {
                plugin_type: Some(APIKEY_AUTH_PLUGIN_ID.to_owned()),
                config: serde_json::json!({
                    "header": "x-api-key",
                    "secret_ref": "cred://oagw-apikey"
                })
                .as_object()
                .cloned()
                .unwrap(),
                ..AuthConfig::default()
            });
        },
    )
    .await;
    add_route(&control, tenant, up_id, &["GET"], "/", None, None);

    let (status, _headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/secured-api/secure")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "authorized");
    mock.assert();
}

#[tokio::test]
async fn required_headers_guard_rejects_missing_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/ping");
        then.status(200).body("pong");
    });
    let port = server.address().port();

    let (router, control, ctx, tenant) = harness(Arc::new(MockCredStoreClient::empty()));
    let up_id = add_upstream(
        &control,
        &ctx,
        tenant,
        "127.0.0.1",
        port,
        "guard-api",
        |u| {
            // Forward inbound headers so the guard can enforce on
            // client-supplied values (the default passthrough policy
            // forwards none).
            u.headers = Some(HeaderRules {
                request: crate::domain::dto::HeaderTransform {
                    passthrough: PassthroughMode::All,
                    ..Default::default()
                },
                ..Default::default()
            });
        },
    )
    .await;
    let mut guard_cfg = serde_json::Map::new();
    guard_cfg.insert(
        "required_request_headers".to_owned(),
        serde_json::json!("x-request-id"),
    );
    add_route(
        &control,
        tenant,
        up_id,
        &["GET"],
        "/",
        Some(PluginsConfig {
            items: vec![PluginBinding {
                plugin_ref: REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
                config: guard_cfg,
            }],
            ..PluginsConfig::default()
        }),
        None,
    );

    // Missing header -> gateway 400 problem (never reaches the upstream).
    let (status, headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/guard-api/ping")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(headers["content-type"], "application/problem+json");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], 400);
    assert_eq!(json["title"], "Validation Error");
    assert!(json["detail"].as_str().unwrap().contains("x-request-id"));
    assert_eq!(mock.calls(), 0);

    // Present header -> forwarded upstream.
    let (status, _headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/guard-api/ping")
            .header("x-request-id", "req-123")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "pong");
    mock.assert();
}

#[tokio::test]
async fn rate_limit_enforces_429_with_headers() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });
    let port = server.address().port();

    let (router, control, ctx, tenant) = harness(Arc::new(MockCredStoreClient::empty()));
    let up_id = add_upstream(
        &control,
        &ctx,
        tenant,
        "127.0.0.1",
        port,
        "limited-api",
        |_| {},
    )
    .await;
    let rate_limit = RateLimitConfig {
        sustained: RateSpec {
            rate: 1,
            window: RateWindow::Second,
        },
        cost: 1,
        ..RateLimitConfig::default()
    };
    add_route(
        &control,
        tenant,
        up_id,
        &["GET"],
        "/",
        None,
        Some(rate_limit),
    );

    // First request consumes the only token.
    let (status, headers, _) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/limited-api/api")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-ratelimit-limit"], "1");
    assert_eq!(headers["x-ratelimit-remaining"], "1");
    assert!(headers.contains_key("x-ratelimit-reset"));
    mock.assert_calls(1);

    // Second request within the window is rejected 429 with guidance headers.
    let (status, headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/limited-api/api")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["content-type"], "application/problem+json");
    assert!(headers.contains_key("retry-after"));
    assert_eq!(headers["x-ratelimit-limit"], "1");
    assert_eq!(headers["x-ratelimit-remaining"], "0");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], 429);
    assert_eq!(json["title"], "Rate Limit Exceeded");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn cors_preflight_echo_and_disallowed_origin() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/cors-endpoint");
        then.status(200).body("cors-ok");
    });
    let port = server.address().port();

    let (router, control, ctx, tenant) = harness(Arc::new(MockCredStoreClient::empty()));
    let up_id = add_upstream(&control, &ctx, tenant, "127.0.0.1", port, "cors-api", |u| {
        u.cors = Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "OPTIONS".to_owned()],
            ..CorsConfig::default()
        });
    })
    .await;
    add_route(&control, tenant, up_id, &["GET"], "/", None, None);

    // Preflight (OPTIONS + Origin + requested method) -> local 204 echo.
    let (status, headers, _) = send(
        &router,
        Request::builder()
            .method("OPTIONS")
            .uri("/oagw/v1/proxy/cors-api/cors-endpoint")
            .header("origin", "https://app.example.com")
            .header("access-control-request-method", "GET")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert_eq!(headers["access-control-allow-methods"], "GET");
    assert_eq!(mock.calls(), 0);

    // Actual request from a disallowed origin -> 403, never forwarded.
    let (status, body) = {
        let (status, _h, body) = send(
            &router,
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/cors-api/cors-endpoint")
                .header("origin", "https://evil.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        (status, body)
    };
    assert_eq!(status, StatusCode::FORBIDDEN);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], 403);
    assert!(json["detail"].as_str().unwrap().contains("origin"));
    assert_eq!(mock.calls(), 0);

    // Allowed origin forwards.
    let (status, _headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/cors-api/cors-endpoint")
            .header("origin", "https://app.example.com")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "cors-ok");
    mock.assert();
}

#[tokio::test]
async fn sse_streaming_passthrough() {
    let server = MockServer::start();
    let sse_body = "data: ping\n\ndata: pong\n\n";
    let mock = server.mock(|when, then| {
        when.method(GET).path("/events");
        then.status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(sse_body);
    });
    let port = server.address().port();

    let (router, control, ctx, tenant) = harness(Arc::new(MockCredStoreClient::empty()));
    let up_id = add_upstream(&control, &ctx, tenant, "127.0.0.1", port, "sse-api", |_| {}).await;
    add_route(&control, tenant, up_id, &["GET"], "/", None, None);

    let (status, headers, body) = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/sse-api/events")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(String::from_utf8_lossy(&body), sse_body);
    mock.assert();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_echo_bridging() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    // Loops back every message it receives.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = listener.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                use futures_util::{SinkExt, StreamExt};
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let (mut tx, mut rx) = ws.split();
                while let Some(Ok(msg)) = rx.next().await {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let (router, control, ctx, tenant) = harness(Arc::new(MockCredStoreClient::empty()));
    let up_id = add_upstream(
        &control,
        &ctx,
        tenant,
        "127.0.0.1",
        echo_port,
        "ws-echo",
        |_| {},
    )
    .await;
    add_route(&control, tenant, up_id, &["GET"], "/", None, None);

    let gw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = gw_listener.local_addr().unwrap().port();
    let gw_task = tokio::spawn(async move {
        let _serve = axum::serve(gw_listener, router).await;
    });

    let url = format!("ws://127.0.0.1:{gw_port}/oagw/v1/proxy/ws-echo/socket");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(Message::Text("hello".to_owned().into()))
        .await
        .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    match reply {
        Message::Text(text) => assert_eq!(text.as_str(), "hello"),
        other => panic!("expected echoed text, got {other:?}"),
    }
    let _close = ws.close(None).await;
    drop(ws);

    gw_task.abort();
    echo_task.abort();
}
