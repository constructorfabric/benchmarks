//! End-to-end data-plane tests over the registered REST surface.
//!
//! Each test starts its own echo upstream on an ephemeral port (bound to
//! `127.0.0.1:0`) and shuts it down before returning; the gateway itself is
//! driven through `axum::Router` without binding any socket of its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue};
use oagw::api::rest::handlers::OagwApi;
use oagw::api::rest::routes;
use oagw::config::OagwConfig;
use oagw::domain::gts_helpers;
use oagw::domain::repo::{RouteRepository as _, UpstreamRepository as _};
use oagw::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchRule, PathSuffixMode, PluginListConfig,
    Route, Upstream, UpstreamServer,
};
use oagw::infra::management::ManagementService;
use oagw::infra::proxy::headers::TARGET_HOST_HEADER;
use oagw::infra::proxy::service::{DataPlane, DataPlaneDeps};
use oagw::infra::storage::memory::MemoryStore;
use oagw::infra::tenant::TenantHierarchy;
use serde_json::Value;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt as _;
use uuid::Uuid;

/// What the echo upstream received, as reported back to the caller.
#[derive(Clone)]
struct EchoUpstream {
    label: &'static str,
    hits: Arc<AtomicUsize>,
}

async fn echo(axum::Extension(state): axum::Extension<EchoUpstream>, request: Request) -> Response {
    state.hits.fetch_add(1, Ordering::Relaxed);
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in request.headers() {
        seen.entry(name.as_str().to_ascii_lowercase())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    let body = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    axum::Json(serde_json::json!({
        "label": state.label,
        "method": method,
        "path": path,
        "query": query,
        "headers": seen,
        "body": String::from_utf8_lossy(&body),
    }))
    .into_response()
}

/// Binds an echo upstream on `127.0.0.1:0`.
///
/// The returned task handle must be aborted by the caller (see [`stop`]) so no
/// listener outlives the test.
async fn spawn_echo(
    label: &'static str,
) -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/{*path}", axum::routing::any(echo))
        .layer(axum::Extension(EchoUpstream {
            label,
            hits: Arc::clone(&hits),
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, server, hits)
}

fn stop(server: &tokio::task::JoinHandle<()>) {
    server.abort();
}

/// A gateway wired to one in-memory store, called as an anonymous tenant.
///
/// The api-gateway normally injects the caller's security context and applies
/// its own prefix (empty in this deployment), so the gear-relative `/oagw/v1`
/// paths are served exactly as registered.
fn gateway() -> (axum::Router, Arc<MemoryStore>) {
    let store = MemoryStore::new();
    let config = OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let data_plane = Arc::new(
        DataPlane::new(DataPlaneDeps {
            store: Arc::clone(&store),
            tenants: TenantHierarchy::new(None, Duration::from_secs(60)),
            credstore: None,
            config,
        })
        .unwrap(),
    );
    let api = OagwApi {
        management: Arc::new(ManagementService::new(
            Arc::clone(&store),
            Some(Arc::clone(&data_plane)),
            true,
        )),
        data_plane,
    };
    let openapi = OpenApiRegistryImpl::new();
    let router = routes::register_routes(axum::Router::new(), &openapi, Arc::new(api))
        .layer(axum::Extension(SecurityContext::anonymous()));
    (router, store)
}

/// Registers `upstream` for the anonymous tenant and returns its id.
fn register_upstream(store: &MemoryStore, endpoints: Vec<Endpoint>, alias: &str) -> Uuid {
    let id = Uuid::new_v4();
    store
        .insert_upstream(Upstream {
            id,
            tenant_id: Uuid::nil(),
            enabled: true,
            alias: alias.to_owned(),
            alias_explicit: true,
            tags: Vec::new(),
            server: UpstreamServer { endpoints },
            protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    id
}

fn loopback(port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Http,
        host: "127.0.0.1".to_owned(),
        port,
    }
}

async fn call(router: &axum::Router, request: Request) -> (http::StatusCode, Value, HeaderMap) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    (status, json, headers)
}

fn proxy_request(
    method: &'static str,
    target: &str,
    request_headers: &[(&str, &str)],
    body: &str,
) -> Request {
    let mut builder = Request::builder().method(method).uri(target);
    for (name, value) in request_headers {
        builder = builder.header(*name, HeaderValue::from_str(value).unwrap());
    }
    builder
        .body(axum::body::Body::from(body.to_owned()))
        .unwrap()
}

fn header_values<'a>(seen: &'a Value, name: &str) -> Vec<&'a str> {
    seen["headers"][name]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Header transformation end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxied_request_strips_hop_by_hop_and_routing_headers() {
    let (addr, server, hits) = spawn_echo("upstream-a").await;
    let (router, store) = gateway();
    register_upstream(&store, vec![loopback(addr.port())], "echo.example");
    let expected_host = format!("127.0.0.1:{}", addr.port());

    let (status, seen, _) = call(
        &router,
        proxy_request(
            "POST",
            "/oagw/v1/proxy/echo.example/v1/messages?tenant=acme",
            &[
                ("connection", "keep-alive"),
                ("transfer-encoding", "chunked"),
                ("x-oagw-target-host", "127.0.0.1"),
                ("host", "client.example"),
                ("x-api-key", "client-secret"),
                ("content-type", "application/json"),
            ],
            "{\"message\":\"hello\"}",
        ),
    )
    .await;

    assert_eq!(status, http::StatusCode::OK, "{seen}");
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert_eq!(seen["method"], "POST");
    assert_eq!(seen["path"], "/v1/messages");
    assert_eq!(seen["query"], "tenant=acme");
    assert_eq!(seen["body"], "{\"message\":\"hello\"}");
    assert!(header_values(&seen, "connection").is_empty());
    assert!(header_values(&seen, "transfer-encoding").is_empty());
    assert!(header_values(&seen, TARGET_HOST_HEADER).is_empty());
    // `Host` is replaced by the upstream authority, never the caller's.
    assert_eq!(header_values(&seen, "host"), vec![expected_host.as_str()]);
    // End-to-end headers survive.
    assert_eq!(header_values(&seen, "x-api-key"), vec!["client-secret"]);
    assert_eq!(header_values(&seen, "content-type"), vec!["application/json"]);
    assert_eq!(header_values(&seen, "x-forwarded-proto"), vec!["https"]);
    // The caller's authority is preserved as provenance even though `Host`
    // itself is rewritten to the upstream.
    assert_eq!(header_values(&seen, "x-forwarded-host"), vec!["client.example"]);
    stop(&server);
}

#[tokio::test]
async fn pinned_target_host_reaches_the_pinned_endpoint() {
    let (addr, server, hits) = spawn_echo("upstream-a").await;
    let (router, store) = gateway();
    register_upstream(&store, vec![loopback(addr.port())], "pool.example");

    // Both spellings must resolve to the same endpoint, and the pinned header
    // must be consumed rather than forwarded.
    for target in ["127.0.0.1", &format!("127.0.0.1:{}", addr.port())] {
        let (status, seen, _) = call(
            &router,
            proxy_request(
                "GET",
                "/oagw/v1/proxy/pool.example/v1/pick",
                &[("x-oagw-target-host", target)],
                "",
            ),
        )
        .await;
        assert_eq!(status, http::StatusCode::OK, "{seen}");
        assert_eq!(seen["label"], "upstream-a");
        assert!(header_values(&seen, TARGET_HOST_HEADER).is_empty());
    }
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    stop(&server);
}

#[tokio::test]
async fn unknown_pinned_target_host_is_rejected_without_contacting_the_upstream() {
    let (addr, server, hits) = spawn_echo("upstream-a").await;
    let (router, store) = gateway();
    register_upstream(&store, vec![loopback(addr.port())], "pool.example");

    let (status, problem, response_headers) = call(
        &router,
        proxy_request(
            "GET",
            "/oagw/v1/proxy/pool.example/v1/ping",
            &[("x-oagw-target-host", "elsewhere.example")],
            "",
        ),
    )
    .await;

    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
    assert_eq!(problem["status"], 400);
    assert_eq!(
        response_headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(hits.load(Ordering::Relaxed), 0, "no upstream may be contacted");
    stop(&server);
}

fn suffix_route(upstream_id: Uuid, path: &str, mode: PathSuffixMode) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::nil(),
        tags: Vec::new(),
        upstream_id,
        r#match: MatchRule {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: mode,
            }),
            grpc: None,
        },
        plugins: PluginListConfig::default(),
        rate_limit: None,
        created_at: 0,
        updated_at: 0,
    }
}

#[tokio::test]
async fn path_suffix_mode_disabled_forwards_only_the_route_base_path() {
    let (addr, server, hits) = spawn_echo("upstream-a").await;
    let (router, store) = gateway();
    let upstream_id = register_upstream(&store, vec![loopback(addr.port())], "trimmed.example");
    store
        .insert_route(suffix_route(upstream_id, "/v1/*", PathSuffixMode::Disabled))
        .unwrap();

    let (status, seen, _) = call(
        &router,
        proxy_request(
            "GET",
            "/oagw/v1/proxy/trimmed.example/v1/ignored/segments",
            &[],
            "",
        ),
    )
    .await;

    assert_eq!(status, http::StatusCode::OK, "{seen}");
    assert_eq!(seen["path"], "/v1", "the suffix is dropped, not forwarded");
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    stop(&server);
}

#[tokio::test]
async fn a_suffix_is_rejected_when_the_route_has_no_match_marker() {
    let (addr, server, hits) = spawn_echo("upstream-a").await;
    let (router, store) = gateway();
    let upstream_id = register_upstream(&store, vec![loopback(addr.port())], "exact.example");
    store
        .insert_route(suffix_route(upstream_id, "/v1", PathSuffixMode::Append))
        .unwrap();

    let (status, problem, _) = call(
        &router,
        proxy_request("GET", "/oagw/v1/proxy/exact.example/v1/extra", &[], ""),
    )
    .await;

    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(hits.load(Ordering::Relaxed), 0, "nothing reaches the upstream");
    stop(&server);
}
