//! Integration tests of the `GET /metrics` surface of entry 2.9 over the real
//! router the gear registers
//! (`cpt-cf-oagw-dod-observability-and-state-metric-surface`,
//! `cpt-cf-oagw-flow-observability-and-state-request-metrics`,
//! `cpt-cf-oagw-flow-observability-and-state-rate-limit-metrics`).
//!
//! Every test drives the real `OagwGear` through `Gear::init` and
//! `RestApiCapability::register_rest`, so the route exists where the
//! registration put it and the series it reports are the ones a real proxied
//! exchange produced.
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-metrics-endpoint:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-rate-limit-metrics:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-request-metrics:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-shared-http-client:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use oagw::domain::dto::{
    BurstCapacity, EndpointScheme, HttpMethod, RateAlgorithm, RateLimitConfig, RateScope,
    RateStrategy, RateWindow, SustainedRate,
};
use oagw::infra::metrics::{
    CIRCUIT_BREAKER_STATE, CIRCUIT_BREAKER_TRANSITIONS_TOTAL, FAMILIES, RATE_LIMIT_USAGE_RATIO,
    REQUEST_DURATION_SECONDS, REQUESTS_IN_FLIGHT, REQUESTS_TOTAL, ROUTING_ENDPOINT_SELECTED,
    UPSTREAM_AVAILABLE, UPSTREAM_CONNECTIONS,
};
use oagw::test_support::{
    FakePolicyAuthZ, audited_surface_over, permissive_surface, route_for, seed_route, seed_upstream,
    security_context, stub_upstream, upstream_at,
};
use uuid::Uuid;

const PROXY: &str = "/oagw/v1/proxy";

/// The `oagw` block the proxy tests need: `http` upstreams admitted.
fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// A `token_bucket` limit of `rate` per second with `capacity` burst.
fn token_bucket(rate: u32, capacity: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: oagw::SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window: RateWindow::Second },
        burst: Some(BurstCapacity { capacity }),
        budget: None,
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// One proxied exchange by a caller of `tenant` against `/v1/orders`.
async fn proxied(
    surface: &oagw::test_support::ManagementSurface,
    tenant: Uuid,
) -> oagw::test_support::ProxyExchange {
    surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await
}

/// The surface with one upstream and one route matching `/v1` over a stub
/// upstream, returning the stub, the owning tenant and the upstream
/// identifier.
async fn seeded(
    surface: &oagw::test_support::ManagementSurface,
) -> (oagw::test_support::StubUpstream, Uuid, Uuid) {
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(surface, upstream);
    seed_route(surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    (stub, tenant, upstream_id)
}
/// An address no listener holds, so an endpoint pointed at it refuses the
/// connection instead of timing out.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("the probe binds");
    listener.local_addr().expect("the probe address").port()
}

/// A scrape by an administrator of `tenant`.
async fn scrape(surface: &oagw::test_support::ManagementSurface, tenant: Uuid) -> oagw::test_support::ProxyExchange {
    surface.proxy("GET", "/metrics", &[], b"", Some(security_context(tenant, Uuid::new_v4()))).await
}

// -- the registration --------------------------------------------------------

/// `GET /metrics` answers on the gear-relative path **outside** `/oagw/v1`, and
/// no operation exists under `/oagw/v1/metrics`
/// (`inst-os-scrape-1`, `inst-os-scrape-2`).
#[tokio::test]
async fn the_metrics_route_is_outside_the_oagw_prefix() {
    let surface = permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let exchange = scrape(&surface, tenant).await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    assert!(
        exchange.header("content-type").is_some_and(|value| value.starts_with("text/plain")),
        "the exposition content type: {:?}",
        exchange.headers
    );

    let nested = surface
        .proxy("GET", "/oagw/v1/metrics", &[], b"", Some(security_context(tenant, Uuid::new_v4())))
        .await;
    assert_eq!(nested.status, http::StatusCode::NOT_FOUND, "no operation under the prefix");
}

/// A request with no resolvable context is refused with `401` before any
/// metric value is read (`inst-os-scrape-3`).
#[tokio::test]
async fn the_scrape_refuses_an_unauthenticated_caller() {
    let surface = permissive_surface(proxy_config()).await;
    let exchange = surface.proxy("GET", "/metrics", &[], b"", None).await;
    assert_eq!(exchange.status, http::StatusCode::UNAUTHORIZED);
    let document: serde_json::Value = serde_json::from_slice(&exchange.body).expect("problem+json");
    assert_eq!(document["status"], 401, "{document}");
}

/// A caller the admin boundary denies — one that holds the proxy-invoke
/// permission, so it is not an authentication failure — is refused with `403`
/// and no series is disclosed (`inst-os-scrape-4`).
#[tokio::test]
async fn a_proxy_permission_holder_is_refused_with_forbidden() {
    let authz = FakePolicyAuthZ::denying(&[oagw::infra::authorization::PERM_METRICS]);
    let (surface, _audit) = audited_surface_over(proxy_config(), authz).await;
    let (_stub, tenant, _upstream_id) = seeded(&surface).await;

    // The same caller proxies successfully, so the refusal is the metrics
    // boundary and nothing else.
    let exchange = proxied(&surface, tenant).await;
    assert_eq!(exchange.status, http::StatusCode::OK, "the proxy permission is granted");

    let refused = scrape(&surface, tenant).await;
    assert_eq!(refused.status, http::StatusCode::FORBIDDEN, "{}", refused.text());
    assert!(!refused.text().contains("oagw_requests_total"), "no series is disclosed");
}

/// The exposition of a registry with no recorded series is still the twelve
/// declared families, each with its `# HELP` and `# TYPE` line
/// (`inst-os-scrape-5`).
#[tokio::test]
async fn the_exposition_declares_every_registered_family() {
    let surface = permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let body = scrape(&surface, tenant).await.text();
    for family in FAMILIES {
        assert!(body.contains(&format!("# HELP {family} ")), "`{family}` is declared: {body}");
        assert!(body.contains(&format!("# TYPE {family} ")), "`{family}` is typed: {body}");
    }
    assert_eq!(body.matches("# HELP ").count(), FAMILIES.len(), "no unregistered family");
}

/// One proxied exchange produces the request counter, the closed in-flight
/// gauge, the routing pair, the duration histogram and the available gauge
/// (`inst-os-req-1` .. `-7`).
#[tokio::test]
async fn a_proxied_exchange_records_its_series() {
    let surface = permissive_surface(proxy_config()).await;
    let (stub, tenant, upstream_id) = seeded(&surface).await;
    let exchange = proxied(&surface, tenant).await;
    assert_eq!(exchange.status, http::StatusCode::OK);

    let registry = surface.gear.metrics().expect("the registry is published");
    let (host, port) = stub.endpoint();
    let endpoint = format!("{host}:{port}");
    assert_eq!(
        registry.value(REQUESTS_TOTAL, &[("host", "api.vendor.com"), ("http.response.status_code", "200")]),
        Some(1.0),
        "the request family is recorded once the status is known"
    );
    assert_eq!(
        registry.value(REQUESTS_IN_FLIGHT, &[("host", "api.vendor.com")]),
        Some(0.0),
        "the exchange closes the gauge it opened"
    );
    assert_eq!(
        registry.value(
            ROUTING_ENDPOINT_SELECTED,
            &[("upstream_id", &upstream_id.to_string()), ("endpoint_host", endpoint.as_str())],
        ),
        Some(1.0),
        "the routing pair is recorded"
    );
    assert_eq!(
        registry.value(UPSTREAM_AVAILABLE, &[("host", "api.vendor.com"), ("endpoint", endpoint.as_str())]),
        Some(1.0),
        "a successful call reports the endpoint available"
    );
    assert!(
        !registry.series(REQUEST_DURATION_SECONDS).is_empty(),
        "the duration histogram is observed"
    );
    // The connection the exchange held is back in the pool once the exchange is
    // over, so the gauge reports the shared client's pool state. The pool is
    // keyed by the host the connection is opened to, not by the addressed alias
    // (`inst-os-client-3`, `cpt-cf-oagw-dod-observability-and-state-shared-client`).
    let active = registry.value(UPSTREAM_CONNECTIONS, &[("host", &host), ("state", "active")]);
    let idle = registry.value(UPSTREAM_CONNECTIONS, &[("host", &host), ("state", "idle")]);
    let max = registry.value(UPSTREAM_CONNECTIONS, &[("host", &host), ("state", "max")]);
    assert_eq!(active, Some(0.0), "the exchange released its connection: {active:?}");
    assert_eq!(idle, max, "the pool holds every connection idle again: {idle:?}, {max:?}");
    assert!(max.is_some_and(|ceiling| ceiling > 0.0), "the pool ceiling is published: {max:?}");
    assert!(
        registry
            .value(UPSTREAM_CONNECTIONS, &[("host", "api.vendor.com"), ("state", "active")])
            .is_none(),
        "the alias is not a pool key: only the resolved endpoint host is"
    );
    assert!(
        !registry.series(UPSTREAM_AVAILABLE).is_empty(),
        "the available gauge is published"
    );
    assert!(
        scrape(&surface, tenant).await.text().contains("oagw_requests_total{"),
        "the counter reaches the exposition"
    );
}

/// The upstream-facing gauge recovers from `0` to `1` for the same endpoint
/// series after a successful call (`inst-os-req-7`, `inst-os-req-8`).
#[tokio::test]
async fn the_available_gauge_recovers_from_zero_to_one() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let _stub = stub;
    let tenant = Uuid::new_v4();
    // The endpoint address is unbound, so the connection is refused and the
    // gauge reports the endpoint unavailable.
    let port = free_port().await;
    let host = "127.0.0.1".to_owned();
    let upstream_id = seed_upstream(
        &surface,
        upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port),
    );
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    let failed = proxied(&surface, tenant).await;
    assert_eq!(failed.status, http::StatusCode::BAD_GATEWAY, "{}", failed.text());
    let registry = surface.gear.metrics().expect("the registry is published");
    let endpoint = format!("{host}:{port}");
    assert_eq!(
        registry.value(UPSTREAM_AVAILABLE, &[("host", "api.vendor.com"), ("endpoint", endpoint.as_str())]),
        Some(0.0),
        "the failed transport call reports the endpoint unavailable"
    );

    // The same endpoint address, now served: the call succeeds and the same
    // series moves to `1`.
    oagw::test_support::stub_upstream_at(port, Vec::new()).await;
    let recovered = proxied(&surface, tenant).await;
    assert_eq!(recovered.status, http::StatusCode::OK, "{}", recovered.text());
    assert_eq!(
        registry.value(UPSTREAM_AVAILABLE, &[("host", "api.vendor.com"), ("endpoint", endpoint.as_str())]),
        Some(1.0),
        "the successful call recovers the same series"
    );
}

/// The rate-limit `path` label is the normalized route match pattern, never the
/// raw request path (`inst-os-rl-4`, `inst-os-rl-7`).
#[tokio::test]
async fn the_rate_limit_path_label_is_the_route_pattern() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let mut upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    upstream.rate_limit = Some(token_bucket(1_000, 1_000));
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            &format!("{PROXY}/api.vendor.com/v1/orders"),
            &[],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());

    let registry = surface.gear.metrics().expect("the registry is published");
    let ratio = registry.value(RATE_LIMIT_USAGE_RATIO, &[("host", "api.vendor.com"), ("path", "/v1")]);
    assert!(ratio.is_some_and(|value| (0.0..=1.0).contains(&value)), "the ratio is published: {ratio:?}");
    let exposition = scrape(&surface, tenant).await.text();
    assert!(
        !exposition.contains("path=\"/v1/orders\""),
        "the label is the route pattern, never the raw request path: {exposition}"
    );
}

/// The breaker families the graded configuration implements no behavior for are
/// still declared, with no sample line
/// (`cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface`,
/// `inst-os-cb-2`).
#[tokio::test]
async fn the_registered_but_unimplemented_families_carry_no_series() {
    let surface = permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let body = scrape(&surface, tenant).await.text();
    for family in [CIRCUIT_BREAKER_STATE, CIRCUIT_BREAKER_TRANSITIONS_TOTAL] {
        assert!(body.contains(&format!("# TYPE {family} ")), "`{family}` is registered: {body}");
        assert!(
            !body.contains(&format!("\n{family}")),
            "`{family}` emits no series in the graded configuration: {body}"
        );
    }
}
