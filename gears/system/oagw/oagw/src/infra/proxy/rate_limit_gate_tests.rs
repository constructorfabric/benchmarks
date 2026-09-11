//! Sibling unit tests of the enforcement step of entry 2.7
//! (`cpt-cf-oagw-dod-rate-limiting-enforcement-point`,
//! `cpt-cf-oagw-dod-rate-limiting-hot-path-cost`).
//!
//! The tests drive the real [`DataPlaneServiceImpl`] over the in-memory store,
//! with the counter registry bound to a [`SharedClock`], so the timing of a
//! refusal is exact rather than wall-clock dependent.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use uuid::Uuid;

use super::service::{DataPlaneLimits, DataPlaneServiceImpl};
use super::rate_limiter::RateLimiterRegistry;
use crate::domain::dto::{Endpoint, EndpointScheme, HttpMethod, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow, SustainedRate};
use crate::domain::proxy::{ProxyContext, ProxyResponse};
use crate::domain::rate_limit::{RateLimitResource, SharedClock};
use crate::domain::repo::RouteRecord;
use crate::domain::services::management::{Actor, AncestorResolver, AuthorizeError, ManagementAuthorizer};
use crate::domain::error::DomainError;
use crate::infra::storage::Storage;
use crate::test_support::{route_for, upstream};

const EPOCH: u64 = 1_700_000_000;

// -- local fakes -------------------------------------------------------------

/// An authorizer that allows everything: the gate under test sits after the
/// authorization of the management surface, not of the proxy path.
struct AllowAll;

#[async_trait::async_trait]
impl ManagementAuthorizer for AllowAll {
    async fn authorize(&self, _actor: &Actor, _permission: &str, _resource_id: &str) -> Result<(), AuthorizeError> {
        Ok(())
    }
}

/// An ancestor resolver that answers the empty chain, so only the addressed
/// tenant contributes a layer.
struct NoAncestors;

#[async_trait::async_trait]
impl AncestorResolver for NoAncestors {
    async fn ancestors(&self, _actor: &Actor, _tenant_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        Ok(Vec::new())
    }
}

// -- the fixture -------------------------------------------------------------

fn limits() -> DataPlaneLimits {
    DataPlaneLimits { allow_http_upstream: true, max_body_size_bytes: 1 << 20, proxy_timeout_secs: 5 }
}

fn limit(rate: u32, capacity: u32, scope: RateScope, algorithm: RateAlgorithm) -> RateLimitConfig {
    RateLimitConfig {
        sharing: crate::domain::dto::SharingMode::Private,
        algorithm,
        sustained: SustainedRate { rate, window: RateWindow::Second },
        burst: Some(crate::domain::dto::BurstCapacity { capacity }),
        budget: None,
        scope,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// The port of a live stub upstream answering `200 payload` to every request,
/// so an admitted request completes its exchange.
async fn stub_port() -> u16 {
    crate::test_support::stub_upstream(Vec::new()).await.endpoint().1
}

/// The data plane over one upstream and one route per tenant of `tenants`,
/// with the registry bound to `clock` and the upstream endpoints pointed at
/// the stub. Every tenant addresses the same alias, so two of them resolve two
/// different upstreams over one shared registry.
fn seeded(
    clock: &SharedClock,
    port: u16,
    upstream_limit: Option<RateLimitConfig>,
    route_limit: Option<RateLimitConfig>,
    tenants: &[Uuid],
) -> DataPlaneServiceImpl {
    let storage = Storage::new();
    let (upstreams, routes, _) = storage.repositories();
    for tenant in tenants {
        let mut record = upstream(*tenant, "api.vendor.com");
        record.server = crate::domain::dto::ServerConfig {
            endpoints: vec![Endpoint { scheme: EndpointScheme::Http, host: "127.0.0.1".to_owned(), port }],
        };
        record.rate_limit = upstream_limit.clone();
        upstreams
            .create(*tenant, crate::domain::repo::UpstreamRecord { upstream: record.clone(), plugin_bindings: Vec::new() })
            .expect("the upstream is seeded");
        let mut route = route_for(*tenant, record.id, "/v1", &[HttpMethod::Get]);
        route.rate_limit = route_limit.clone();
        routes
            .create(*tenant, RouteRecord { route: route.clone(), plugin_bindings: Vec::new() })
            .expect("the route is seeded");
    }
    DataPlaneServiceImpl::new(
        upstreams,
        routes,
        Arc::new(NoAncestors),
        Arc::new(AllowAll),
        limits(),
    )
    .with_rate_limiters(Arc::new(RateLimiterRegistry::new(Arc::new(clock.clone()))))
}

/// The data plane over one tenant.
fn service(
    clock: &SharedClock,
    port: u16,
    upstream_limit: Option<RateLimitConfig>,
    route_limit: Option<RateLimitConfig>,
) -> (DataPlaneServiceImpl, Uuid) {
    let tenant = Uuid::new_v4();
    (seeded(clock, port, upstream_limit, route_limit, &[tenant]), tenant)
}

fn context(tenant: Uuid, alias: &str) -> ProxyContext {
    ProxyContext {
        method: "GET".to_owned(),
        alias: alias.to_owned(),
        path_suffix: Some("/v1".to_owned()),
        query: None,
        headers: vec![("host".to_owned(), "api.vendor.com".to_owned())],
        body: Bytes::new(),
        tenant_id: tenant,
        principal_id: Uuid::new_v4(),
        peer_addr: Some("10.0.0.1:5000".to_owned()),
        trace_id: Some("trace-1".to_owned()),
    }
}

/// The value of `name`, compared case-insensitively against the raw header
/// list the pipeline produced.
fn header<'a>(response: &'a ProxyResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The response form of an outcome, so a test can read the headers and the
/// observation of a failure the exchange produced rather than a refusal.
fn as_response(outcome: Result<ProxyResponse, crate::domain::proxy::ProxyFailure>) -> ProxyResponse {
    outcome.unwrap_or_else(|failure| ProxyResponse::gateway_error(
        failure
            .domain()
            .cloned()
            .unwrap_or(DomainError::ProtocolError {
                upstream_id: None,
                host: None,
                path: None,
                trace_id: None,
            }),
        502,
    ))
}

// -- the enforcement point ---------------------------------------------------

/// The `rate_limit` surface of an unconfigured upstream and route is never
/// consulted: no counter is allocated and no quota header is rendered.
#[tokio::test]
async fn an_unconfigured_upstream_is_never_rate_limited() {
    let clock = SharedClock::at(0, EPOCH);
    let (service, tenant) = service(&clock, stub_port().await, None, None);
    let response = as_response(service.execute(context(tenant, "api.vendor.com"), None).await);
    assert!(
        header(&response, "x-ratelimit-limit").is_none(),
        "no quota header is rendered without a configured limit"
    );
    assert!(response.observation.rate_limit.is_none(), "no observation is recorded");
    assert!(service.rate_limiters().is_empty(), "no counter was created");
}

/// A configured limit refuses the request before the upstream is called: the
/// response is the `429` of the error contract, carrying `Retry-After` and the
/// three quota headers, and the upstream is never reached.
#[tokio::test]
async fn a_configured_limit_refuses_before_the_upstream_is_called() {
    let clock = SharedClock::at(0, EPOCH);
    let (service, tenant) =
        service(&clock, stub_port().await, Some(limit(1, 1, RateScope::Tenant, RateAlgorithm::TokenBucket)), None);
    let first = service.execute(context(tenant, "api.vendor.com"), None).await;
    assert!(first.is_ok(), "the first request is admitted: {first:?}");
    let second = service
        .execute(context(tenant, "api.vendor.com"), None)
        .await
        .expect("a refusal is a response, not a failure");
    assert_eq!(second.status, 429);
    assert_eq!(second.source, crate::domain::proxy::ErrorSource::Gateway);
    // `Retry-After` is rendered from the carried error by the REST layer; the
    // data plane carries the guidance on the error itself.
    assert_eq!(second.observation.rate_limit.as_ref().and_then(|o| o.retry_after_seconds), Some(1));
    assert_eq!(header(&second, "x-ratelimit-limit"), Some("1"));
    assert_eq!(header(&second, "x-ratelimit-remaining"), Some("0"));
    assert_eq!(header(&second, "x-ratelimit-reset"), Some("1700000001"));
    assert_eq!(
        second.observation.error_type,
        Some("gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
    );
    let observation = second.observation.rate_limit.as_ref().expect("the refusal is observed");
    assert!(observation.refused);
    assert_eq!(observation.host, "api.vendor.com");
    assert_eq!(observation.path, "/v1");
    assert_eq!(observation.retry_after_seconds, Some(1));
}

/// A refused request consumes nothing, so the counter stays where the last
/// admitted request left it.
#[tokio::test]
async fn a_refusal_consumes_nothing_and_reports_the_depleted_state() {
    let clock = SharedClock::at(0, EPOCH);
    let (service, tenant) =
        service(&clock, stub_port().await, Some(limit(1, 1, RateScope::Tenant, RateAlgorithm::TokenBucket)), None);
    let _ = service.execute(context(tenant, "api.vendor.com"), None).await;
    let refused = service.execute(context(tenant, "api.vendor.com"), None).await.expect("refusal");
    let observation = refused.observation.rate_limit.as_ref().expect("observed");
    assert!(observation.refused);
    assert_eq!(header(&refused, "x-ratelimit-remaining"), Some("0"));
    clock.advance_seconds(1);
    let recovered = service.execute(context(tenant, "api.vendor.com"), None).await;
    assert!(recovered.is_ok(), "the bucket refills one second later");
}

/// `response_headers: false` keeps the quota off the response, and the
/// observation of the decision is recorded either way.
#[tokio::test]
async fn the_quota_headers_are_suppressed_when_the_config_says_so() {
    let clock = SharedClock::at(0, EPOCH);
    let mut config = limit(10, 10, RateScope::Tenant, RateAlgorithm::TokenBucket);
    config.response_headers = false;
    let (service, tenant) = service(&clock, stub_port().await, Some(config), None);
    let response = service.execute(context(tenant, "api.vendor.com"), None).await;
    assert!(response.is_ok(), "{response:?}");
    let response = response.expect("admitted");
    assert!(header(&response, "x-ratelimit-limit").is_none(), "{:?}", response.headers);
    let observation = response.observation.rate_limit.as_ref().expect("the decision is observed");
    assert!(!observation.refused);
    assert!(observation.retry_after_seconds.is_none());
}

/// The counter key of one request is derived once: two tenants never contend
/// for one counter even when they address the same upstream.
#[tokio::test]
async fn two_tenants_addressing_one_upstream_get_distinct_counters() {
    let clock = SharedClock::at(0, EPOCH);
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let service = seeded(
        &clock,
        stub_port().await,
        Some(limit(1, 1, RateScope::Tenant, RateAlgorithm::TokenBucket)),
        None,
        &[first, second],
    );
    let _ = service.execute(context(first, "api.vendor.com"), None).await;
    let refused = service
        .execute(context(first, "api.vendor.com"), None)
        .await
        .expect("the first tenant is refused");
    assert_eq!(refused.status, 429);
    let other = service.execute(context(second, "api.vendor.com"), None).await;
    assert!(other.is_ok(), "the second tenant keeps its own budget: {other:?}");
    assert_eq!(service.rate_limiters().len(), 2);
}

/// A route-level limit is a distinct counter from the upstream-level one, and
/// it wins when both are configured.
#[tokio::test]
async fn a_route_level_limit_wins_and_is_a_distinct_counter() {
    let clock = SharedClock::at(0, EPOCH);
    let (service, tenant) = service(
        &clock,
        stub_port().await,
        Some(limit(10, 10, RateScope::Tenant, RateAlgorithm::TokenBucket)),
        Some(limit(1, 1, RateScope::Tenant, RateAlgorithm::TokenBucket)),
    );
    let _ = service.execute(context(tenant, "api.vendor.com"), None).await;
    let refused = service.execute(context(tenant, "api.vendor.com"), None).await.expect("refused");
    assert_eq!(refused.status, 429, "the route limit of 1 applies, not the upstream limit of 10");
    assert_eq!(header(&refused, "x-ratelimit-limit"), Some("1"));
    assert_eq!(service.rate_limiters().len(), 1, "only the route counter was consulted");
}

/// The `ip` scope keys the counter by the connection peer address, so two
/// principals behind one address share a budget and one behind another do not.
#[tokio::test]
async fn the_ip_scope_keys_the_counter_by_the_peer_address() {
    let clock = SharedClock::at(0, EPOCH);
    let (service, tenant) = service(&clock, stub_port().await, Some(limit(1, 1, RateScope::Ip, RateAlgorithm::TokenBucket)), None);
    let mut first = context(tenant, "api.vendor.com");
    first.peer_addr = Some("10.0.0.1:5000".to_owned());
    let mut second = context(tenant, "api.vendor.com");
    second.peer_addr = Some("10.0.0.2:5000".to_owned());
    let _ = service.execute(first.clone(), None).await;
    let refused = service.execute(first, None).await.expect("refused");
    assert_eq!(refused.status, 429);
    assert!(service.execute(second, None).await.is_ok(), "the second peer keeps its own budget");
}

/// A configured `queue` or `degrade` strategy resolves to the `reject`
/// outcome: the request is refused with `429` and the upstream is never called
/// (`cpt-cf-oagw-dod-rate-limiting-reject-only-execution`).
#[tokio::test]
async fn a_configured_queue_strategy_is_executed_as_a_refusal() {
    let clock = SharedClock::at(0, EPOCH);
    let mut config = limit(1, 1, RateScope::Tenant, RateAlgorithm::TokenBucket);
    config.strategy = RateStrategy::Queue;
    let (service, tenant) = service(&clock, stub_port().await, Some(config), None);
    let _ = service.execute(context(tenant, "api.vendor.com"), None).await;
    let refused = service.execute(context(tenant, "api.vendor.com"), None).await.expect("refused");
    assert_eq!(refused.status, 429, "queue is not executed, so the refusal stands");
}

/// The sliding window applies no burst allowance and releases its oldest
/// instant exactly one window after it was recorded.
#[tokio::test]
async fn the_sliding_window_applies_no_burst_and_releases_its_oldest_instant() {
    let clock = SharedClock::at(0, EPOCH);
    let (service, tenant) = service(&clock, stub_port().await, Some(limit(2, 2, RateScope::Tenant, RateAlgorithm::SlidingWindow)), None);
    let _ = service.execute(context(tenant, "api.vendor.com"), None).await;
    let _ = service.execute(context(tenant, "api.vendor.com"), None).await;
    let refused = service.execute(context(tenant, "api.vendor.com"), None).await.expect("refused");
    assert_eq!(refused.status, 429, "a sliding window applies no burst allowance");
    clock.advance_nanos(1_000_000_000);
    let admitted = service.execute(context(tenant, "api.vendor.com"), None).await;
    assert!(admitted.is_ok(), "the first instant left the window: {admitted:?}");
}

// -- the hot path ------------------------------------------------------------

/// The check alone — the key derivation plus the registry acquisition — sits
/// far inside the less-than-1 ms rate-check budget
/// (`cpt-cf-oagw-dod-rate-limiting-hot-path-cost`).
///
/// The FEATURE names a Criterion benchmark for this ceiling; the crate carries
/// no benchmark harness, so the ceiling is asserted here with a generous
/// wall-clock bound over 10,000 sequential checks, which is recorded as a
/// deviation.
#[tokio::test]
async fn the_check_alone_stays_inside_the_one_millisecond_budget() {
    let clock = SharedClock::at(0, EPOCH);
    let limiter = Arc::new(RateLimiterRegistry::new(Arc::new(clock.clone())));
    let resource = RateLimitResource::Upstream { upstream_id: Uuid::new_v4().to_string() };
    let spec = crate::domain::rate_limit::CounterSpec::of(&limit(
        1_000_000,
        1_000_000,
        RateScope::Tenant,
        RateAlgorithm::TokenBucket,
    ));
    let tenant = Uuid::new_v4().to_string();
    let principal = Uuid::new_v4().to_string();
    let started = Instant::now();
    let rounds = 10_000;
    for _ in 0..rounds {
        let key = crate::domain::rate_limit::counter_key(&crate::domain::rate_limit::CounterKeyContext {
            resource: &resource,
            scope: RateScope::Tenant,
            tenant_id: &tenant,
            principal_id: Some(&principal),
            peer_addr: Some("10.0.0.1:5000"),
            route_id: None,
        });
        let _ = limiter.acquire(&key, &spec);
    }
    let per_check_nanos = started.elapsed().as_nanos() / rounds;
    assert!(
        per_check_nanos < 1_000_000,
        "{per_check_nanos} ns per check is outside the 1 ms budget"
    );
    assert_eq!(limiter.len(), 1, "one counter serves the whole loop");
}
