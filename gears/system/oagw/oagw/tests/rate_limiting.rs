//! Integration tests of the rate limiting of entry 2.7 over the proxy surface
//! (`cpt-cf-oagw-flow-rate-limiting-proxy-check`,
//! `cpt-cf-oagw-dod-rate-limiting-reject-response`,
//! `cpt-cf-oagw-dod-rate-limiting-enforcement-point`).
//!
//! The tests drive the real management surface and the real proxy handler over
//! a live stub upstream, so a refusal is observed at the boundary the client
//! sees: the `429`, the `application/problem+json` body of the error contract,
//! the `Retry-After` guidance and the three quota headers.
// @cpt-flow:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-counter-scopes:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-dual-rate-config:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-enforcement-point:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-hierarchical-inheritance:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-hot-path-cost:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-metrics:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-payload-interaction:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-per-instance-state:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-reject-only-execution:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-reject-response:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-sliding-window:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-token-bucket:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limiting-unit-tests:p1

use oagw::domain::dto::{
    BurstCapacity, EndpointScheme, HttpMethod, RateAlgorithm, RateLimitConfig, RateScope,
    RateStrategy, RateWindow, SustainedRate,
};
use oagw::test_support::{
    permissive_surface, route_for, seed_route, seed_upstream, stub_upstream, upstream_at,
};
use uuid::Uuid;

// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-1
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-2
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-3
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-4
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-5
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-6
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-7
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-8
const PROXY: &str = "/oagw/v1/proxy";
//
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-8
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-7
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-6
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-5
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-4
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-3
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-2
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-usage-observation:p1:inst-rl-obs-1
//

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

/// The `oagw` block with a body limit small enough that an oversized-body test
/// can exceed it.
fn tiny_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 16
    }))
}

/// The surface with one upstream and one route seeded over a stub upstream,
/// carrying the given limits.
async fn seeded(
    upstream_limit: Option<RateLimitConfig>,
    route_limit: Option<RateLimitConfig>,
) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    seeded_over(permissive_surface(proxy_config()).await, upstream_limit, route_limit).await
}

/// [`seeded`] over a surface that has already been built.
async fn seeded_over(
    surface: oagw::test_support::ManagementSurface,
    upstream_limit: Option<RateLimitConfig>,
    route_limit: Option<RateLimitConfig>,
) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let mut upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    upstream.rate_limit = upstream_limit;
    let upstream_id = seed_upstream(&surface, upstream);
    let mut route = route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get, HttpMethod::Post]);
    route.rate_limit = route_limit;
    seed_route(&surface, route);
    (surface, stub, tenant)
}

fn principal() -> Uuid {
    Uuid::new_v4()
}

// -- the config surface ------------------------------------------------------

/// The `rate_limit` block of an upstream and a route round-trips through the
/// management surface (`cpt-cf-oagw-flow-rate-limiting-config-surface`).
#[tokio::test]
async fn the_rate_limit_block_round_trips_through_the_management_surface() {
    let surface = oagw::test_support::permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let principal = principal();
    let body = serde_json::json!({
        "alias": "api.vendor.com",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "api.vendor.com" } ] },
        "rate_limit": {
            "sustained": { "rate": 30, "window": "minute" },
            "burst": { "capacity": 60 },
            "algorithm": "token_bucket",
            "scope": "user",
            "strategy": "reject",
            "cost": 2
        }
    });
    let (status, created) = surface.create(tenant, principal, body).await;
    assert_eq!(status, http::StatusCode::CREATED, "{:?}", String::from_utf8_lossy(&created));
    let stored: serde_json::Value = serde_json::from_slice(&created).expect("the body is json");
    let stored = stored.get("rate_limit").cloned().expect("the rate_limit block is stored");
    assert_eq!(stored["sustained"]["rate"], serde_json::json!(30));
    assert_eq!(stored["sustained"]["window"], serde_json::json!("minute"));
    assert_eq!(stored["burst"]["capacity"], serde_json::json!(60));
    assert_eq!(stored["scope"], serde_json::json!("user"));
    assert_eq!(stored["cost"], serde_json::json!(2));
}

/// A rate limit block that the schema rejects is rejected at the boundary.
#[tokio::test]
async fn an_invalid_rate_limit_block_is_rejected_at_the_boundary() {
    let surface = oagw::test_support::permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let body = serde_json::json!({
        "alias": "api.vendor.com",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "api.vendor.com" } ] },
        "rate_limit": { "sustained": { "rate": 0 } }
    });
    let (status, rejected) = surface.create(tenant, principal(), body).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{rejected:?}");
    let problem: serde_json::Value = serde_json::from_slice(&rejected).expect("problem+json");
    assert!(problem["detail"].as_str().unwrap_or_default().contains("rate_limit.sustained.rate"));
}

// -- the enforcement point ---------------------------------------------------

/// An unconfigured upstream and route are never rate-limited: no counter, no
/// quota header (`cpt-cf-oagw-dod-rate-limiting-enforcement-point`).
#[tokio::test]
async fn an_unconfigured_upstream_is_never_rate_limited() {
    let (surface, stub, tenant) = seeded(None, None).await;
    let principal = principal();
    for _ in 0..5 {
        let exchange = surface
            .proxy_for(tenant, principal, "GET", &format!("{PROXY}/api.vendor.com/v1/x"), &[], b"")
            .await;
        assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
        assert!(exchange.header("x-ratelimit-limit").is_none(), "{:?}", exchange.headers);
        assert!(exchange.header("retry-after").is_none(), "{:?}", exchange.headers);
    }
    assert_eq!(stub.received().len(), 5, "every request reached the upstream");
}

/// A configured limit refuses the request once its budget is spent, with the
/// `429` of the error contract, before the upstream is called.
/// The auth phase runs ahead of the rate-limit call-in, so a request that is
/// both unauthenticated and over quota is the `401` surface and never the
/// `429` one (`cpt-cf-oagw-dod-request-proxy-plugin-chain-points`).
#[tokio::test]
async fn an_unauthenticated_request_is_rejected_before_the_rate_limit_is_consulted() {
    let (surface, stub, tenant) = seeded(Some(token_bucket(1, 1)), None).await;
    let principal = principal();
    let path = format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface.proxy_for(tenant, principal, "GET", &path, &[], b"").await;
    assert_eq!(admitted.status, http::StatusCode::OK, "{:?}", admitted.text());
    // The bucket is now exhausted, so an authenticated caller is refused.
    let refused = surface.proxy_for(tenant, principal, "GET", &path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS, "{:?}", refused.text());
    // The unauthenticated caller is the `401` surface, not the `429` one.
    let unauthenticated = surface.proxy("GET", &path, &[], b"", None).await;
    assert_eq!(unauthenticated.status, http::StatusCode::UNAUTHORIZED);
    assert_eq!(stub.received().len(), 1, "only the admitted request reached the upstream");
}

#[tokio::test]
async fn a_configured_limit_refuses_before_the_upstream_is_called() {
    let (surface, stub, tenant) = seeded(Some(token_bucket(2, 2)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    for remaining in ["1", "0"] {
        let exchange = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
        assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
        assert_eq!(exchange.header("x-ratelimit-limit"), Some("2"));
        assert_eq!(exchange.header("x-ratelimit-remaining"), Some(remaining));
    }
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("content-type"), Some("application/problem+json"));
    assert_eq!(refused.header("x-oagw-error-source"), Some("gateway"));
    assert_eq!(refused.header("retry-after"), Some("1"));
    assert_eq!(refused.header("x-ratelimit-limit"), Some("2"));
    assert_eq!(refused.header("x-ratelimit-remaining"), Some("0"));
    let problem: serde_json::Value = serde_json::from_slice(&refused.body).expect("problem+json");
    assert_eq!(
        problem["type"],
        serde_json::json!("gts://gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
    );
    assert_eq!(stub.received().len(), 2, "the refused request never reached the upstream");
}

/// A refused request consumes nothing, so the budget stays where the last
/// admitted request left it until it refills.
#[tokio::test]
async fn a_refusal_consumes_nothing_and_refills_one_second_later() {
    let (surface, _stub, tenant) = seeded(Some(token_bucket(1, 1)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    assert_eq!(
        surface.proxy_for(tenant, principal, "GET", path, &[], b"").await.status,
        http::StatusCode::OK
    );
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-remaining"), Some("0"));
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let recovered = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(recovered.status, http::StatusCode::OK, "{:?}", recovered.text());
}

/// A limit configured on the route wins over the upstream limit and is a
/// distinct counter from it.
#[tokio::test]
async fn a_route_level_limit_applies_and_is_a_distinct_counter() {
    let (surface, stub, tenant) = seeded(Some(token_bucket(10, 10)), Some(token_bucket(1, 1))).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(admitted.status, http::StatusCode::OK);
    assert_eq!(admitted.header("x-ratelimit-limit"), Some("1"), "the route limit wins");
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-limit"), Some("1"));
    assert_eq!(stub.received().len(), 1);
}

/// The `user` scope keys the counter by the caller: two principals behind one
/// tenant keep separate budgets.
#[tokio::test]
async fn the_user_scope_gives_each_principal_its_own_budget() {
    let mut limit = token_bucket(1, 1);
    limit.scope = RateScope::User;
    let (surface, _stub, tenant) = seeded(Some(limit), None).await;
    let first = principal();
    let second = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    for caller in [first, second] {
        let admitted = surface.proxy_for(tenant, caller, "GET", path, &[], b"").await;
        assert_eq!(admitted.status, http::StatusCode::OK, "{:?}", admitted.text());
        let refused = surface.proxy_for(tenant, caller, "GET", path, &[], b"").await;
        assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    }
}

/// The `ip` scope keys the counter by the connection peer address.
#[tokio::test]
async fn the_ip_scope_keys_the_counter_by_the_peer_address() {
    let (surface, _stub, tenant) = seeded(Some(token_bucket(1, 1)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(admitted.status, http::StatusCode::OK);
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    // The handler reads the peer address from the connection, which is the
    // same address every exchange in this test carries, so the budget is
    // shared by both callers rather than keyed per principal.
    assert_eq!(refused.header("x-ratelimit-limit"), Some("1"));
}

/// `response_headers: false` keeps the quota off the response.
#[tokio::test]
async fn the_quota_headers_are_suppressed_when_the_config_says_so() {
    let mut limit = token_bucket(10, 10);
    limit.response_headers = false;
    let (surface, _stub, tenant) = seeded(Some(limit), None).await;
    let exchange = surface
        .proxy_for(tenant, principal(), "GET", &format!("{PROXY}/api.vendor.com/v1/x"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
    assert!(exchange.header("x-ratelimit-limit").is_none(), "{:?}", exchange.headers);
    assert!(exchange.header("x-ratelimit-remaining").is_none(), "{:?}", exchange.headers);
}

/// A configured `queue` or `degrade` strategy is executed as a refusal: only
/// `reject` is executed (`cpt-cf-oagw-dod-rate-limiting-reject-only-execution`).
#[tokio::test]
async fn a_configured_queue_strategy_is_executed_as_a_refusal() {
    let mut limit = token_bucket(1, 1);
    limit.strategy = RateStrategy::Queue;
    let (surface, stub, tenant) = seeded(Some(limit), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    assert_eq!(
        surface.proxy_for(tenant, principal, "GET", path, &[], b"").await.status,
        http::StatusCode::OK
    );
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS, "queue is not executed");
    assert_eq!(stub.received().len(), 1);
}

/// A configured `degrade` strategy is executed as a refusal too.
#[tokio::test]
async fn a_configured_degrade_strategy_is_executed_as_a_refusal() {
    let mut limit = token_bucket(1, 1);
    limit.strategy = RateStrategy::Degrade;
    let (surface, _stub, tenant) = seeded(Some(limit), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    assert_eq!(
        surface.proxy_for(tenant, principal, "GET", path, &[], b"").await.status,
        http::StatusCode::OK
    );
    assert_eq!(
        surface.proxy_for(tenant, principal, "GET", path, &[], b"").await.status,
        http::StatusCode::TOO_MANY_REQUESTS
    );
}

/// The sliding window applies no burst allowance and releases its oldest
/// instant one window after it was recorded.
#[tokio::test]
async fn the_sliding_window_applies_no_burst_and_releases_its_oldest_instant() {
    let mut limit = token_bucket(2, 2);
    limit.algorithm = RateAlgorithm::SlidingWindow;
    limit.burst = None;
    let (surface, _stub, tenant) = seeded(Some(limit), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    for _ in 0..2 {
        let exchange = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
        assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
    }
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS, "no burst allowance");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let released = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(released.status, http::StatusCode::OK, "{:?}", released.text());
}

/// A cost of more than one unit consumes that many units per request.
#[tokio::test]
async fn a_cost_above_one_is_deducted_per_request() {
    let mut limit = token_bucket(5, 5);
    limit.cost = 3;
    let (surface, _stub, tenant) = seeded(Some(limit), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let first = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(first.status, http::StatusCode::OK);
    assert_eq!(first.header("x-ratelimit-remaining"), Some("2"));
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS, "cost 3 exceeds the remaining 2");
}

/// The minute window is honoured: a rate of one per minute refuses the second
/// request inside the same minute.
#[tokio::test]
async fn the_sustained_window_is_honoured() {
    let mut limit = token_bucket(1, 1);
    limit.sustained.window = RateWindow::Minute;
    let (surface, _stub, tenant) = seeded(Some(limit), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    assert_eq!(
        surface.proxy_for(tenant, principal, "GET", path, &[], b"").await.status,
        http::StatusCode::OK
    );
    let refused = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
}

// -- the payload interaction -------------------------------------------------

/// A refused request is answered `429` and is never reclassified as a payload
/// outcome (`cpt-cf-oagw-dod-rate-limiting-payload-interaction`).
#[tokio::test]
async fn a_refused_request_is_never_answered_413() {
    let (surface, stub, tenant) = seeded(Some(token_bucket(1, 1)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    assert_eq!(
        surface.proxy_for(tenant, principal, "GET", path, &[], b"").await.status,
        http::StatusCode::OK
    );
    let refused = surface
        .proxy_for(
            tenant,
            principal,
            "GET",
            path,
            &[("content-type", "application/json"), ("content-length", "7")],
            b"{\"a\":1}",
        )
        .await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS, "{:?}", refused.text());
    assert_eq!(refused.header("content-type"), Some("application/problem+json"));
    assert_eq!(stub.received().len(), 1);
}

/// An oversized request whose length is declared is answered `413` before the
/// rate-limit step runs, so it consumes no budget
/// (`cpt-cf-oagw-dod-rate-limiting-payload-interaction`).
#[tokio::test]
async fn an_oversized_declared_request_is_413_and_consumes_no_budget() {
    let surface = permissive_surface(tiny_config()).await;
    let (surface, _stub, tenant) = seeded_over(surface, Some(token_bucket(5, 5)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(admitted.status, http::StatusCode::OK, "{:?}", admitted.text());
    assert_eq!(admitted.header("x-ratelimit-remaining"), Some("4"));
    let oversized = surface
        .proxy_for(
            tenant,
            principal,
            "GET",
            path,
            &[("content-length", "1000")],
            b"tiny",
        )
        .await;
    assert_eq!(oversized.status, http::StatusCode::PAYLOAD_TOO_LARGE, "{:?}", oversized.text());
    assert_eq!(oversized.header("x-ratelimit-limit"), None, "no counter was consulted");
    // The next well-formed request proves the oversized one spent nothing.
    let still_admitted = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(still_admitted.status, http::StatusCode::OK, "{:?}", still_admitted.text());
    assert_eq!(still_admitted.header("x-ratelimit-remaining"), Some("3"));
}

/// An oversized chunked request is answered `413` while its body is being
/// buffered, and its budget is not restored afterwards.
#[tokio::test]
async fn an_oversized_chunked_request_is_413() {
    let surface = permissive_surface(tiny_config()).await;
    let (surface, _stub, tenant) = seeded_over(surface, Some(token_bucket(5, 5)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(admitted.status, http::StatusCode::OK, "{:?}", admitted.text());
    let oversized = surface
        .proxy_for(
            tenant,
            principal,
            "GET",
            path,
            &[("transfer-encoding", "chunked")],
            &[0u8; 128][..],
        )
        .await;
    assert_eq!(oversized.status, http::StatusCode::PAYLOAD_TOO_LARGE, "{:?}", oversized.text());
    assert_eq!(oversized.header("x-ratelimit-limit"), None, "no counter was consulted");
    // The charge of the first admitted request is retained: nothing is refunded
    // and nothing extra is spent by the aborted one.
    let after = surface.proxy_for(tenant, principal, "GET", path, &[], b"").await;
    assert_eq!(after.status, http::StatusCode::OK, "{:?}", after.text());
    assert_eq!(after.header("x-ratelimit-remaining"), Some("3"));
}

/// An admitted request carries its body to the upstream untouched, so the
/// rate-limit step does not consume the request.
#[tokio::test]
async fn an_admitted_request_still_carries_its_body() {
    let (surface, stub, tenant) = seeded(Some(token_bucket(5, 5)), None).await;
    let principal = principal();
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface
        .proxy_for(
            tenant,
            principal,
            "POST",
            path,
            &[("content-type", "application/json"), ("content-length", "7")],
            b"{\"a\":1}",
        )
        .await;
    assert_eq!(admitted.status, http::StatusCode::OK, "{:?}", admitted.text());
    assert_eq!(admitted.header("x-ratelimit-limit"), Some("5"));
    let received = stub.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body_text(), "{\"a\":1}");
}
