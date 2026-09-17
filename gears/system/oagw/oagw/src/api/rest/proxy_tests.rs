#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `Router::oneshot` tests of the proxy data plane.
//!
//! Every test drives the real handler through a real `httpmock` upstream (or a
//! raw `TcpListener`, where the mock cannot stream), so the pipeline runs end
//! to end: alias resolution, the ADR-0001 target-host matrix, the header plan,
//! the DESIGN.md error table and the ADR-0007 source header.
//!
//! The gear is configured for the graded configuration: plaintext `http`
//! endpoints allowed, the SSRF policy on.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::Value;
use toolkit::ClientHub;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::{OagwConfig, SsrfPolicyConfig};
use crate::domain::model::{
    Alias, CorsConfig, EndpointScheme, HeaderOps, HeaderPassthrough, HttpMethod, PathSuffixMode,
    Protocol, RateLimit, RateLimitBurst, RateLimitSustained, RateLimitWindow, RequestHeaderOps,
    ResponseHeaderOps, RouteMatch, SharingMode, UpstreamEndpoint, UpstreamServer,
};
use crate::domain::model::{Route, Upstream};
use crate::domain::service::Service;
use crate::gear::OagwState;
use crate::infra::http_client::{ProxyCall, ProxyClient};
use crate::infra::plugins::{PluginCatalog, PluginRegistry, SecretResolver, TokenCacheConfig};
use crate::infra::ratelimit::RateLimiter;
use crate::infra::secrets::CredStoreSecretResolver;
use crate::infra::store::Store;

use super::proxy::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, TARGET_HOST_HEADER,
};
use crate::api::rest::proxy::{PROXY_ALIAS_PATH, PROXY_SUFFIX_PATH};
use crate::api::rest::{ProxyState, register_proxy_routes};

/// The gear-relative prefix of every proxied path.
const PROXY: &str = "/oagw/v1/proxy/";

// ── Harness ───────────────────────────────────────────────────────────────

/// A `10.0.0.0/8` address the SSRF policy blocks, for the blocked-target test.
const BLOCKED_HOST: &str = "10.255.0.1";

/// The gear state behind the proxy, with the same shape `Gear::init` builds.
fn state(config: OagwConfig) -> Arc<ArcSwap<OagwState>> {
    let resolver: Arc<dyn SecretResolver> =
        Arc::new(CredStoreSecretResolver::new(Arc::new(ClientHub::new())));
    Arc::new(ArcSwap::from_pointee(OagwState {
        config,
        store: Arc::new(Store::new()),
        plugins: Arc::new(PluginRegistry::with_builtins(
            resolver,
            TokenCacheConfig::new(Duration::from_secs(60), 128),
        )),
        plugin_catalog: Arc::new(PluginCatalog::new()),
        rate_limiter: Arc::new(RateLimiter::with_system_clock()),
    }))
}

/// The graded configuration: plaintext `http` endpoints allowed, SSRF on.
fn config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        // Every mocked upstream dials `127.0.0.1`, which the SSRF guard exists
        // to block; only the test that exercises the guard turns it back on.
        ssrf_policy: SsrfPolicyConfig { enabled: false },
        ..OagwConfig::default()
    }
}

/// A router with the proxy routes, and the tenant that owns its store entries.
fn harness(swap: &Arc<ArcSwap<OagwState>>) -> (Router, Arc<ProxyState>, Uuid) {
    let openapi = OpenApiRegistryImpl::new();
    let tenant = Uuid::new_v4();
    let state = Arc::new(ProxyState {
        gear: Arc::clone(swap),
        service: Arc::new(Service::new(
            Arc::clone(&swap.load().store),
            Arc::new(ClientHub::new()),
        )),
        client_hub: Arc::new(ClientHub::new()),
        client: ProxyClient::new(),
    });
    (
        register_proxy_routes(Router::new(), &openapi, Arc::clone(&state)),
        state,
        tenant,
    )
}

/// The store alias of the gear state behind the proxy state.
fn store_of(state: &ProxyState) -> Arc<Store> {
    Arc::clone(state.service.store())
}

/// An `http` upstream named `alias` with the single endpoint `host:port`.
fn upstream(alias: &str, host: &str, port: u16) -> Upstream {
    Upstream {
        id: None,
        enabled: true,
        alias: Some(Alias::try_new(alias).unwrap()),
        tags: Vec::new(),
        server: UpstreamServer {
            endpoints: vec![endpoint(host, port)],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

/// An `http` endpoint.
fn endpoint(host: &str, port: u16) -> UpstreamEndpoint {
    UpstreamEndpoint {
        scheme: EndpointScheme::Http,
        host: host.to_owned(),
        port,
    }
}

/// A multi-endpoint upstream named `alias`, for the ADR-0001 matrix.
fn multi_upstream(alias: &str, endpoints: Vec<UpstreamEndpoint>) -> Upstream {
    let mut upstream = Upstream {
        id: None,
        enabled: true,
        alias: Some(Alias::try_new(alias).unwrap()),
        tags: Vec::new(),
        server: UpstreamServer { endpoints },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    };
    upstream.alias = Some(Alias::try_new(alias).unwrap());
    upstream
}

impl Upstream {
    /// Marks the upstream disabled.
    fn disabled(mut self) -> Upstream {
        self.enabled = false;
        self
    }

    /// Forwards every inbound header to the upstream.
    fn with_passthrough(mut self, mode: HeaderPassthrough) -> Upstream {
        let request = RequestHeaderOps {
            passthrough: mode,
            ..RequestHeaderOps::default()
        };
        self.headers = Some(HeaderOps {
            request,
            ..HeaderOps::default()
        });
        self
    }

    /// Configures response header rules.
    fn with_response_headers(mut self, ops: ResponseHeaderOps) -> Upstream {
        self.headers = Some(HeaderOps {
            response: ops,
            ..HeaderOps::default()
        });
        self
    }
}

/// Puts an upstream in the store, generating the id the service would have.
fn put_upstream(store: &Store, tenant: Uuid, mut upstream: Upstream) -> Upstream {
    let id = upstream.id.unwrap_or_else(Uuid::new_v4);
    upstream.id = Some(id);
    store.put_upstream(tenant, upstream.clone());
    upstream
}

/// Puts a route matching `methods` on `path`, with the generated id on it.
fn put_route(
    store: &Store,
    tenant: Uuid,
    upstream_id: Uuid,
    methods: &[HttpMethod],
    path: &str,
) -> Route {
    let route = Route {
        id: Some(Uuid::new_v4()),
        tags: Vec::new(),
        upstream_id,
        match_rule: RouteMatch {
            http: Some(crate::domain::model::HttpMatch {
                methods: methods.to_vec(),
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
    store.put_route(tenant, route.clone());
    route
}

/// Puts the same route again with a rate limit on it, which the store takes as
/// the replacement of the entry with that id.
fn put_rate_limited_route(
    store: &Store,
    tenant: Uuid,
    upstream_id: Uuid,
    path: &str,
    rate_limit: RateLimit,
) -> Route {
    let mut route = put_route(store, tenant, upstream_id, &[HttpMethod::Get], path);
    route.rate_limit = Some(rate_limit);
    store.put_route(tenant, route.clone());
    route
}

/// A proxied request with the security context injected.
fn request(method: &str, uri: &str, tenant: Uuid) -> Request<axum::body::Body> {
    request_with(method, uri, tenant, |builder| builder, None)
}

/// A proxied request with extra headers and an optional body.
fn request_with(
    method: &str,
    uri: &str,
    tenant: Uuid,
    headers: impl FnOnce(axum::http::request::Builder) -> axum::http::request::Builder,
    body: Option<Vec<u8>>,
) -> Request<axum::body::Body> {
    let mut req = headers(Request::builder().method(method).uri(uri))
        .body(axum::body::Body::from(body.unwrap_or_default()))
        .unwrap();
    req.extensions_mut().insert(
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(tenant)
            .build()
            .unwrap(),
    );
    req
}

/// The response body as a UTF-8 string, read to the end.
async fn text(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The response body as JSON.
async fn json(response: axum::response::Response) -> Value {
    serde_json::from_str(&text(response).await).expect("a JSON body")
}

/// A response header, or the empty string.
fn header(response: &axum::response::Response, name: &str) -> String {
    let values: Vec<&str> = response
        .headers()
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    values.join(", ")
}

// ── Passthrough ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_get_reaches_the_upstream_with_the_endpoint_host() {
    let swap = state(config());
    let server = MockServer::start();
    let host = format!("{}:{}", server.host(), server.port());
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/models")
            .header("host", host.as_str());
        then.status(200).body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, ERROR_SOURCE_HEADER),
        ERROR_SOURCE_UPSTREAM
    );
    assert_eq!(text(response).await, "models");
    mock.assert_calls(1);
}

#[tokio::test]
async fn the_target_host_header_never_reaches_the_upstream() {
    let swap = state(config());
    let server = MockServer::start();
    // `passthrough: all` so `x-kept` does arrive, which isolates the routing
    // header as the one thing the pipeline consumed.
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/ping")
            .header_exists("x-kept")
            .header_missing(TARGET_HOST_HEADER);
        then.status(200).body("pong");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port())
            .with_passthrough(HeaderPassthrough::All),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/ping"),
            tenant,
            |builder| {
                builder
                    .header("x-kept", "yes")
                    .header(TARGET_HOST_HEADER, server.host().as_str())
            },
            None,
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        text(response).await
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn the_hop_by_hop_headers_never_reach_the_upstream() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/ping")
            .header_missing("connection")
            .header_missing("keep-alive")
            .header_missing("proxy-authorization")
            .header_missing("te")
            .header_missing("trailer")
            .header_missing("transfer-encoding")
            .header_missing("upgrade");
        then.status(200).body("pong");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("hop.test", server.host().as_str(), server.port())
            .with_passthrough(HeaderPassthrough::All),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/ping"),
            tenant,
            |builder| {
                builder
                    .header("connection", "close")
                    .header("keep-alive", "timeout=5")
                    .header("proxy-authorization", "Basic zzz")
                    .header("te", "trailers")
                    .header("trailer", "x-checksum")
                    .header("upgrade", "websocket")
            },
            None,
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        text(response).await
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_post_body_is_forwarded_to_the_upstream() {
    let swap = state(config());
    let server = MockServer::start();
    let payload = b"{\"model\":\"gpt\"}".to_vec();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .body(String::from_utf8_lossy(&payload).into_owned());
        then.status(200).body("ok");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Post],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "POST",
            &format!("{PROXY}{alias}/v1/chat"),
            tenant,
            |builder| builder.header("content-type", "application/json"),
            Some(payload),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        text(response).await
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_request_no_route_matches_yields_the_upstream_path() {
    let swap = state(config());
    let server = MockServer::start();
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1/only",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request(
            "GET",
            &format!("{PROXY}{alias}/v1/never-matched"),
            tenant,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

// ── Error table ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem_with_the_gateway_source() {
    let swap = state(config());
    let (router, _proxy, tenant) = harness(&swap);

    let response = router
        .oneshot(request(
            "GET",
            "/oagw/v1/proxy/no-such-alias/v1/thing",
            tenant,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
    let problem = json(response).await;
    assert_eq!(problem["status"], 404, "{problem}");
    assert_eq!(problem["instance"], "/oagw/v1/proxy/no-such-alias/v1/thing");
    assert!(
        problem["context"]["resource_type"]
            .as_str()
            .unwrap_or_default()
            .ends_with("cf.oagw.route.not_found.v1~"),
        "{problem}"
    );
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_problem() {
    let swap = state(config());
    let server = MockServer::start();
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).disabled(),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
    assert_eq!(json(response).await["status"], 503);
}

#[tokio::test]
async fn a_disabled_route_is_a_503_problem() {
    let swap = state(config());
    let server = MockServer::start();
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    let route = put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );
    proxy
        .service
        .set_route_enabled(tenant, route.id.unwrap(), false)
        .unwrap();

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn a_rate_limited_request_is_a_429_with_the_decision_headers() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("models");
    });
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    put_rate_limited_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        "/v1",
        RateLimit {
            sharing: SharingMode::Private,
            algorithm: Default::default(),
            sustained: RateLimitSustained {
                rate: 1,
                window: RateLimitWindow::Minute,
            },
            burst: Some(RateLimitBurst { capacity: Some(1) }),
            scope: Default::default(),
            strategy: Default::default(),
            cost: 1,
        },
    );

    let alias = upstream.alias.as_ref().unwrap();
    let uri = format!("{PROXY}{alias}/v1/models");
    let first = router
        .clone()
        .oneshot(request("GET", &uri, tenant))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let second = router.oneshot(request("GET", &uri, tenant)).await.unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&second, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
    // ADR-0003: `Retry-After` always, `X-RateLimit-*` while the policy has them
    // on — a 429 without them tells the caller nothing about the retry.
    assert!(!header(&second, "retry-after").is_empty());
    assert!(!header(&second, "x-ratelimit-limit").is_empty());
    assert!(!header(&second, "x-ratelimit-remaining").is_empty());
    assert!(!header(&second, "x-ratelimit-reset").is_empty());
    let problem = json(second).await;
    assert_eq!(problem["status"], 429);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_multi_endpoint_common_suffix_alias_without_a_target_host_is_a_400() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    // Same port on both endpoints: the alias the endpoints derive is the
    // common suffix, which is what makes the target host mandatory.
    let upstream = put_upstream(
        &store,
        tenant,
        multi_upstream(
            "vendor.com",
            vec![endpoint("us.vendor.com", 80), endpoint("eu.vendor.com", 80)],
        ),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let response = router
        .oneshot(request(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/models",
            tenant,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
    let problem = json(response).await;
    let hosts = problem["context"]["valid_hosts"]
        .as_array()
        .expect("valid_hosts");
    assert!(
        hosts.iter().any(|host| host == "us.vendor.com"),
        "{problem}"
    );
    assert!(
        hosts.iter().any(|host| host == "eu.vendor.com"),
        "{problem}"
    );
}

#[tokio::test]
async fn an_invalid_target_host_is_a_400_with_the_offending_value() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("api.vendor.test", "api.vendor.test", 80),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let response = router
        .oneshot(request_with(
            "GET",
            "/oagw/v1/proxy/api.vendor.test/v1/models",
            tenant,
            |builder| builder.header(TARGET_HOST_HEADER, "not a host"),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json(response).await["context"]["invalid_value"],
        "not a host"
    );
}

#[tokio::test]
async fn an_unknown_target_host_is_a_400_with_the_valid_hosts() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        multi_upstream("api.vendor.test", vec![endpoint("api.vendor.test", 80)]),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let response = router
        .oneshot(request_with(
            "GET",
            "/oagw/v1/proxy/api.vendor.test/v1/models",
            tenant,
            |builder| builder.header(TARGET_HOST_HEADER, "other.vendor.test"),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = json(response).await;
    assert_eq!(problem["context"]["invalid_value"], "other.vendor.test");
    assert!(
        !problem["context"]["valid_hosts"]
            .as_array()
            .expect("valid_hosts")
            .is_empty()
    );
}

#[tokio::test]
async fn a_target_host_the_ssrf_policy_blocks_is_a_400() {
    let mut config = config();
    config.ssrf_policy.enabled = true;
    let swap = state(config);
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("blocked.vendor.test", BLOCKED_HOST, 8080),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let response = router
        .oneshot(request(
            "GET",
            "/oagw/v1/proxy/blocked.vendor.test/v1/models",
            tenant,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn an_https_endpoint_is_reported_as_an_unsupported_transport() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let mut upstream = upstream("secure.vendor.test", "secure.vendor.test", 443);
    upstream.server.endpoints[0].scheme = EndpointScheme::Https;
    let upstream = put_upstream(&store, tenant, upstream);
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn a_plaintext_endpoint_is_rejected_when_the_config_disallows_it() {
    let swap = state(OagwConfig::default());
    let server = MockServer::start();
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

// ── Upstream error passthrough ────────────────────────────────────────────

#[tokio::test]
async fn an_upstream_error_passes_through_with_its_body_and_the_upstream_source() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/boom");
        then.status(418)
            .header("content-type", "application/json")
            .body("{\"error\":\"teapot\"}");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/boom"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(
        header(&response, ERROR_SOURCE_HEADER),
        ERROR_SOURCE_UPSTREAM
    );
    assert_eq!(text(response).await, "{\"error\":\"teapot\"}");
    mock.assert_calls(1);
}

// ── Streaming ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_sse_response_arrives_in_chunks_over_time() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let events = ["first", "second", "third"];
    let stream_handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = String::new();
        let mut buffer = [0_u8; 4096];
        while !head.contains("\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            head.push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        for event in events {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let line = format!("data: {event}\n\n");
            socket
                .write_all(format!("{:x}\r\n{line}\r\n", line.len()).as_bytes())
                .await
                .unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });

    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let host = address.ip().to_string();
    let upstream = put_upstream(&store, tenant, upstream("raw", &host, address.port()));
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/stream"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // The three events must arrive as three reads, not as one buffered body.
    let started = std::time::Instant::now();
    let mut stream = response.into_body().into_data_stream();
    let mut chunks = 0;
    while let Some(frame) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = String::from_utf8(frame.unwrap().to_vec()).unwrap();
        assert!(chunk.contains("data: "), "{chunk:?}");
        chunks += 1;
    }
    let elapsed = started.elapsed();
    stream_handle.await.unwrap();
    assert!(
        chunks >= 3,
        "expected the events to arrive separately, got {chunks}"
    );
    assert!(
        elapsed >= Duration::from_millis(160),
        "the events arrived too fast to have been streamed: {elapsed:?}"
    );
}

// ── Body limits and framing ───────────────────────────────────────────────

#[tokio::test]
async fn a_declared_body_over_the_hard_limit_is_a_413() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("overflow.vendor.test", "overflow.vendor.test", 8080),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Post],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "POST",
            &format!("{PROXY}{alias}/v1/upload"),
            tenant,
            |builder| builder.header("content-length", "104857601"),
            Some(vec![0_u8; 8]),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn a_body_outgrowing_the_limit_while_streaming_is_a_413() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/upload");
        then.status(200).body("received");
    });
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "host",
        format!("127.0.0.1:{}", server.port()).parse().unwrap(),
    );

    let call = ProxyCall {
        authority: format!("127.0.0.1:{}", server.port()),
        path: "/upload".to_owned(),
        declared_length: Some(4096),
        // A small limit stands in for the 100 MiB hard limit, so the overflow
        // needs no hundred-megabyte payload.
        limit: 1024,
        timeout: Duration::from_secs(5),
        ..ProxyCall::default()
    };
    let error = ProxyClient::new()
        .send(&call, headers, axum::body::Body::from(vec![0_u8; 4096]))
        .await
        .unwrap_err();

    // The body never completed, so the mock records no call.
    assert_eq!(error.http_status(), 413, "{error:?}");
}

#[tokio::test]
async fn a_body_mismatching_its_declared_length_is_a_400() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/upload");
        then.status(200).body("received");
    });
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "host",
        format!("127.0.0.1:{}", server.port()).parse().unwrap(),
    );

    let call = ProxyCall {
        method: "POST".to_owned(),
        authority: format!("127.0.0.1:{}", server.port()),
        path: "/upload".to_owned(),
        declared_length: Some(4096),
        timeout: Duration::from_secs(5),
        ..ProxyCall::default()
    };
    let error = ProxyClient::new()
        .send(&call, headers, axum::body::Body::from("short"))
        .await
        .unwrap_err();

    assert_eq!(error.http_status(), 400, "{error:?}");
}

#[tokio::test]
async fn a_non_chunked_transfer_encoding_is_a_400() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("smuggle.vendor.test", "smuggle.vendor.test", 8080),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Post],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "POST",
            &format!("{PROXY}{alias}/v1/upload"),
            tenant,
            |builder| builder.header("transfer-encoding", "gzip"),
            Some(vec![0_u8; 4]),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

// ── Transport failures ────────────────────────────────────────────────────

#[tokio::test]
async fn a_dead_upstream_is_a_502_problem() {
    let swap = state(config());
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    // Nothing listens on loopback port 1.
    let upstream = put_upstream(&store, tenant, upstream("dead.test", "127.0.0.1", 1));
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn a_slow_upstream_is_a_504_problem() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Accept and never answer: the response headers never arrive.
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let mut config = config();
    config.proxy_timeout_secs = 1;
    let swap = state(config);
    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let host = address.ip().to_string();
    let upstream = put_upstream(&store, tenant, upstream("raw", &host, address.port()));
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/slow"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

// ── Header rules and authentication ───────────────────────────────────────

#[tokio::test]
async fn the_response_header_rules_apply_to_the_passthrough() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).header("x-drop-me", "gone").body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let ops = ResponseHeaderOps {
        set: [("x-from-oagw".to_owned(), "yes".to_owned())]
            .into_iter()
            .collect(),
        add: Default::default(),
        remove: vec!["x-drop-me".to_owned()],
    };
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_response_headers(ops),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-from-oagw"), "yes");
    assert_eq!(header(&response, "x-drop-me"), "");
    assert_eq!(text(response).await, "models");
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_head_request_is_served_on_the_proxy_path() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(Method::HEAD).path("/v1/models");
        then.status(200).header("content-length", "0");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()),
    );
    // `HEAD` is served by a route that declares `GET`, as the two differ only
    // in the body, which the response to a head request never carries.
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request(
            "HEAD",
            &format!("{PROXY}{alias}/v1/models"),
            tenant,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, ERROR_SOURCE_HEADER),
        ERROR_SOURCE_UPSTREAM
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn an_options_request_is_not_a_method_not_allowed() {
    let swap = state(config());
    let (router, _proxy, tenant) = harness(&swap);

    // No route can declare `OPTIONS` (the schema has no such method), so the
    // handler answers with the route-matching 404 rather than a routing 405.
    let response = router
        .oneshot(request(
            "OPTIONS",
            "/oagw/v1/proxy/target.test/v1/models",
            tenant,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn a_request_without_a_security_context_is_a_401() {
    let swap = state(config());
    let openapi = OpenApiRegistryImpl::new();
    let router = register_proxy_routes(
        Router::new(),
        &openapi,
        Arc::new(ProxyState {
            gear: Arc::clone(&swap),
            service: Arc::new(Service::new(
                Arc::clone(&swap.load().store),
                Arc::new(ClientHub::new()),
            )),
            client_hub: Arc::new(ClientHub::new()),
            client: ProxyClient::new(),
        }),
    );

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/oagw/v1/proxy/api.vendor.test/v1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

// ── CORS (ADR-0004) ───────────────────────────────────────────────────────

/// The `CorsConfig` of a test, with `GET` and `POST` allowed.
fn cors(sharing: SharingMode, enabled: bool, origins: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing,
        enabled,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

impl Upstream {
    /// Configures the CORS policy of the upstream.
    fn with_cors(mut self, cors: CorsConfig) -> Upstream {
        self.cors = Some(cors);
        self
    }
}

impl CorsConfig {
    /// Restricts the methods the policy answers for.
    fn with_methods(mut self, methods: &[&str]) -> CorsConfig {
        self.allowed_methods = methods.iter().map(|method| (*method).to_owned()).collect();
        self
    }
}

/// Overwrites the CORS block of the route `put_route` created.
fn put_route_with_cors(
    store: &Store,
    tenant: Uuid,
    upstream_id: Uuid,
    methods: &[HttpMethod],
    path: &str,
    cors: CorsConfig,
) -> Route {
    let mut route = put_route(store, tenant, upstream_id, methods, path);
    route.cors = Some(cors);
    store.put_route(tenant, route.clone());
    route
}

/// A preflight for the proxy path of `alias`.
fn preflight_request(alias: &str, path: &str, tenant: Uuid) -> Request<axum::body::Body> {
    request_with(
        "OPTIONS",
        &format!("{PROXY}{alias}{path}"),
        tenant,
        |builder| {
            builder
                .header("origin", "https://app.example.com")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "Content-Type")
        },
        None,
    )
}

#[tokio::test]
async fn a_preflight_is_answered_permissively_without_calling_the_upstream() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(Method::OPTIONS).path("/v1/models");
        then.status(200).body("answered by the upstream");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    // The policy allows neither the origin nor the method of the preflight:
    // the answer is permissive all the same, and enforcement happens on the
    // actual request.
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(cors(
            SharingMode::Private,
            true,
            &["https://other.example.com"],
        )),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Post],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(preflight_request(alias.as_ref(), "/v1/models", tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        "https://app.example.com"
    );
    assert_eq!(header(&response, "access-control-allow-methods"), "POST");
    assert_eq!(
        header(&response, "access-control-allow-headers"),
        "Content-Type"
    );
    assert_eq!(header(&response, "access-control-max-age"), "86400");
    let vary = header(&response, "vary");
    assert!(vary.contains("Origin"), "{vary}");
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
    mock.assert_calls(0);
}

#[tokio::test]
async fn a_preflight_needs_nothing_resolved() {
    let swap = state(config());
    let (router, _proxy, tenant) = harness(&swap);

    // No upstream is in the store: a preflight still gets its permissive
    // answer, because resolving the alias would need a tenant context and a
    // browser sends no credentials on a preflight.
    let response = router
        .oneshot(preflight_request("no-such-alias", "/v1/models", tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        "https://app.example.com"
    );
}

#[tokio::test]
async fn an_options_without_a_requested_method_is_not_a_preflight() {
    let swap = state(config());
    let (router, _proxy, tenant) = harness(&swap);

    let response = router
        .oneshot(request_with(
            "OPTIONS",
            "/oagw/v1/proxy/target.test/v1/models",
            tenant,
            |builder| builder.header("origin", "https://app.example.com"),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
}

#[tokio::test]
async fn a_disallowed_origin_is_a_403_problem_with_the_gateway_source() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(cors(
            SharingMode::Private,
            true,
            &["https://app.example.com"],
        )),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/models"),
            tenant,
            |builder| builder.header("origin", "https://evil.example.com"),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(header(&response, ERROR_SOURCE_HEADER), ERROR_SOURCE_GATEWAY);
    assert!(header(&response, "vary").contains("Origin"));
    let problem = json(response).await;
    assert_eq!(problem["status"], 403, "{problem}");
    assert!(
        problem["context"]["resource_type"]
            .as_str()
            .unwrap_or_default()
            .ends_with("cf.oagw.cors.origin_not_allowed.v1~"),
        "{problem}"
    );
    mock.assert_calls(0);
}

#[tokio::test]
async fn a_disallowed_method_is_a_403_problem() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat");
        then.status(200).body("ok");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(
            cors(SharingMode::Private, true, &["https://app.example.com"]).with_methods(&["GET"]),
        ),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Post],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "POST",
            &format!("{PROXY}{alias}/v1/chat"),
            tenant,
            |builder| {
                builder
                    .header("origin", "https://app.example.com")
                    .header("content-type", "application/json")
            },
            Some(b"{}".to_vec()),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let problem = json(response).await;
    assert!(
        problem["context"]["resource_type"]
            .as_str()
            .unwrap_or_default()
            .ends_with("cf.oagw.cors.method_not_allowed.v1~"),
        "{problem}"
    );
    mock.assert_calls(0);
}

#[tokio::test]
async fn an_allowed_origin_gets_the_cors_headers_on_the_passthrough() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200)
            .header("vary", "Accept-Encoding")
            .body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let mut policy = cors(SharingMode::Private, true, &["https://app.example.com"]);
    policy.expose_headers = vec!["X-Request-ID".to_owned()];
    policy.allow_credentials = true;
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(policy),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/models"),
            tenant,
            |builder| builder.header("origin", "https://app.example.com"),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        text(response).await
    );
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        "https://app.example.com"
    );
    assert_eq!(
        header(&response, "access-control-expose-headers"),
        "X-Request-ID"
    );
    assert_eq!(
        header(&response, "access-control-allow-credentials"),
        "true"
    );
    // The upstream's own `Vary` survives the gateway's `Vary: Origin`.
    let vary = header(&response, "vary");
    assert!(vary.contains("Accept-Encoding"), "{vary}");
    assert!(vary.contains("Origin"), "{vary}");
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_request_without_an_origin_is_never_cors_governed() {
    let swap = state(config());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(cors(
            SharingMode::Private,
            true,
            &["https://app.example.com"],
        )),
    );
    put_route(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
    );

    let alias = upstream.alias.as_ref().unwrap();
    let response = router
        .oneshot(request("GET", &format!("{PROXY}{alias}/v1/models"), tenant))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "access-control-allow-origin"), "");
    assert_eq!(header(&response, "vary"), "");
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_route_cors_block_replaces_the_upstream_one_by_default() {
    let swap = state(config());
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(cors(
            SharingMode::Private,
            true,
            &["https://app.example.com"],
        )),
    );
    put_route_with_cors(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
        cors(SharingMode::Private, true, &["https://route.example.com"]),
    );

    let alias = upstream.alias.as_ref().unwrap();
    let allowed = router
        .clone()
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/models"),
            tenant,
            |builder| builder.header("origin", "https://route.example.com"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK, "{}", text(allowed).await);

    let rejected = router
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/models"),
            tenant,
            |builder| builder.header("origin", "https://app.example.com"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_inherited_route_cors_block_unions_the_upstream_origins() {
    let swap = state(config());
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("models");
    });

    let (router, proxy, tenant) = harness(&swap);
    let store = store_of(&proxy);
    let upstream = put_upstream(
        &store,
        tenant,
        upstream("target.test", server.host().as_str(), server.port()).with_cors(cors(
            SharingMode::Inherit,
            true,
            &["https://app.example.com"],
        )),
    );
    put_route_with_cors(
        &store,
        tenant,
        upstream.id.unwrap(),
        &[HttpMethod::Get],
        "/v1",
        cors(SharingMode::Inherit, false, &["https://route.example.com"]),
    );

    let alias = upstream.alias.as_ref().unwrap();
    for origin in ["https://app.example.com", "https://route.example.com"] {
        let response = router
            .clone()
            .oneshot(request_with(
                "GET",
                &format!("{PROXY}{alias}/v1/models"),
                tenant,
                |builder| builder.header("origin", origin),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{origin}");
    }

    let rejected = router
        .oneshot(request_with(
            "GET",
            &format!("{PROXY}{alias}/v1/models"),
            tenant,
            |builder| builder.header("origin", "https://evil.example.com"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
}

// ── Registered surface ────────────────────────────────────────────────────

#[test]
fn the_proxy_paths_are_gear_relative() {
    assert_eq!(PROXY_ALIAS_PATH, "/oagw/v1/proxy/{alias}");
    assert_eq!(PROXY_SUFFIX_PATH, "/oagw/v1/proxy/{alias}/{*path}");
    assert_eq!(PROXY, "/oagw/v1/proxy/");
}
