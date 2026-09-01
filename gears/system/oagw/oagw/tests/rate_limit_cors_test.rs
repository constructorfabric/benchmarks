// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-proxy-data-plane:p2
//! Rate limiting (ADR-0003) and CORS (ADR-0004) on the proxy data plane.
//!
//! Every enforcement test drives a real proxy request: the quota and the origin
//! are decided after the upstream and the route resolve and before the dial, so
//! a refusal is observable as a problem document whose upstream mock was never
//! called. The hierarchical cases seed the same alias into two tenants of one
//! store and let [`common::StaticTenantChain`] stand in for the
//! `tenant_resolver`.

mod common;

use anyhow::{Context as _, Result};
use axum::http::StatusCode;
use common::{
    ERROR_SOURCE, Harness, LogCapture, ProxyHarness, StaticTenantChain, domain_route,
    domain_upstream, https_upstream, loopback_endpoint, problem_type, proxy_config,
};
use httpmock::prelude::{GET, MockServer};
use oagw::domain::model::{
    BurstConfig, CorsConfig, HttpMethod, RateLimitConfig, SharingMode, SustainedRate,
};
use std::sync::Arc;
use uuid::Uuid;

/// `X-OAGW-` proxy route of every test.
const PROXY_PATH: &str = "/oagw/v1/proxy/api.vendor.com/v1/chat";
/// Alias every test routes through.
const ALIAS: &str = "api.vendor.com";
/// The root tenant [`StaticTenantChain`] reports as the single ancestor.
const ROOT: Uuid = Uuid::nil();

/// Tenant the management-API write-path tests act as.
fn tenant() -> Uuid {
    Uuid::now_v7()
}

// ── Configuration helpers ────────────────────────────────────────────────

/// A token-bucket policy over the members the tests assert on.
fn rate_limit(
    rate: u64,
    capacity: u64,
    sharing: SharingMode,
    strategy: &str,
    cost: u64,
    response_headers: bool,
) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: "token_bucket".to_owned(),
        sustained: SustainedRate {
            rate,
            window: "second".to_owned(),
        },
        burst: Some(BurstConfig { capacity }),
        // `global` keeps every request of a test on the same counter, so a
        // policy is observable without a second caller.
        scope: "global".to_owned(),
        strategy: strategy.to_owned(),
        cost,
        response_headers,
    }
}

/// A CORS policy over the members the tests assert on.
fn cors_config(enabled: bool, origins: &[&str], methods: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled,
        allowed_origins: origins.iter().map(ToString::to_string).collect(),
        allowed_methods: methods.iter().map(ToString::to_string).collect(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

/// A CORS policy that also exposes headers and allows credentials.
fn cors_policy(mut policy: CorsConfig) -> CorsConfig {
    policy.expose_headers = Vec::from(["x-request-id".to_owned(), "x-trace".to_owned()]);
    policy.allow_credentials = true;
    policy
}

// ── Seeding ──────────────────────────────────────────────────────────────

/// An upstream of `tenant` whose policy members the test controls.
fn upstream(
    tenant: Uuid,
    alias: &str,
    port: u16,
    rate_limit: Option<RateLimitConfig>,
    cors: Option<CorsConfig>,
) -> oagw::domain::model::Upstream {
    let mut record = domain_upstream(tenant, alias, Vec::from([loopback_endpoint(port)]), true);
    record.rate_limit = rate_limit;
    record.cors = cors;
    record
}

/// Seed the `/v1/chat` route of `upstream_id`.
fn seed_route(harness: &ProxyHarness, upstream_id: Uuid) {
    seed_route_of(
        harness,
        harness.tenant(),
        upstream_id,
        "/v1/chat",
        None,
        None,
    );
}

/// Seed a route of `tenant` with the policies the test spells.
fn seed_route_of(
    harness: &ProxyHarness,
    tenant: Uuid,
    upstream_id: Uuid,
    path: &str,
    rate_limit: Option<RateLimitConfig>,
    cors: Option<CorsConfig>,
) -> Uuid {
    let mut route = domain_route(
        tenant,
        upstream_id,
        &[HttpMethod::Get, HttpMethod::Post],
        path,
        &[],
    );
    route.rate_limit = rate_limit;
    route.cors = cors;
    let id = route.id;
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| panic!("the test route must seed: {error}"));
    id
}

/// A harness whose upstream answers `port` and carries the two policies.
fn harness_with(
    port: u16,
    rate_limit: Option<RateLimitConfig>,
    cors: Option<CorsConfig>,
) -> ProxyHarness {
    let harness = ProxyHarness::new();
    let record = upstream(harness.tenant(), ALIAS, port, rate_limit, cors);
    seed_route(&harness, harness.seed_upstream(record));
    harness
}

/// Seed the child upstream of the harness tenant **and** its ancestor of the
/// root tenant, both under the same alias.
fn chained_harness(
    port: u16,
    child: Option<RateLimitConfig>,
    ancestor: Option<RateLimitConfig>,
) -> ProxyHarness {
    let harness = ProxyHarness::with_config_and_chain(&proxy_config(), Arc::new(StaticTenantChain));
    let record = upstream(harness.tenant(), ALIAS, port, child, None);
    seed_route(&harness, harness.seed_upstream(record));
    harness.seed_upstream(upstream(ROOT, ALIAS, port, ancestor, None));
    harness
}

/// One proxied request with the headers the test spells.
async fn proxy_with<'h>(harness: &'h ProxyHarness, headers: &'h [(&str, &str)]) -> common::Reply {
    match harness.proxy("GET", PROXY_PATH, headers, b"").await {
        Ok(reply) => reply,
        Err(error) => panic!("the proxy request must be sent: {error}"),
    }
}

/// A harness whose upstream and route belong to the root tenant, so any
/// tenant below the root reaches them through the chain.
fn root_harness(
    port: u16,
    rate_limit: Option<RateLimitConfig>,
    cors: Option<CorsConfig>,
) -> ProxyHarness {
    let harness = ProxyHarness::with_config_and_chain(&proxy_config(), Arc::new(StaticTenantChain));
    let record = upstream(ROOT, ALIAS, port, rate_limit, cors);
    let id = harness.seed_upstream(record);
    seed_route_of(&harness, ROOT, id, "/v1/chat", None, None);
    harness
}

/// A policy of `scope` with the sharing mode the test needs.
fn scoped_policy(rate: u64, capacity: u64, scope: &str, sharing: SharingMode) -> RateLimitConfig {
    let mut policy = rate_limit(rate, capacity, sharing, "reject", 1, true);
    scope.clone_into(&mut policy.scope);
    policy
}

/// A policy in `window` rather than the default `second`.
fn windowed_policy(
    rate: u64,
    capacity: u64,
    window: &str,
    sharing: SharingMode,
) -> RateLimitConfig {
    let mut policy = rate_limit(rate, capacity, sharing, "reject", 1, true);
    window.clone_into(&mut policy.sustained.window);
    policy
}

/// A CORS policy in `sharing` rather than the default `private`.
fn shared_cors(sharing: SharingMode, origins: &[&str]) -> CorsConfig {
    let mut policy = cors_config(true, origins, &["GET", "POST"]);
    policy.sharing = sharing;
    policy
}

/// One proxied request as an identity the test spells.
///
/// The `user` scope is keyed on the subject, so that test needs two callers
/// whose subject ids stay fixed across the requests it compares.
async fn proxy_identity<'h>(
    harness: &'h ProxyHarness,
    identity: &toolkit_security::SecurityContext,
    headers: &'h [(&str, &str)],
) -> common::Reply {
    match harness
        .proxy_as_identity(identity, "GET", PROXY_PATH, headers, b"")
        .await
    {
        Ok(reply) => reply,
        Err(error) => panic!("the proxy request must be sent: {error}"),
    }
}

/// An identity of `tenant` with a subject the test controls.
fn identity_of(tenant: Uuid, subject: Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|error| panic!("a test identity must build: {error}"))
}

/// A mock that answers every `GET /v1/chat` with 200 and an empty body.
fn ok_mock(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200);
    })
}

// ── Rate limiting (ADR-0003) ─────────────────────────────────────────────

/// A burst up to the capacity succeeds; the next request is refused.
#[tokio::test]
async fn a_burst_up_to_the_capacity_is_served_then_the_next_request_is_refused() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(1, 2, SharingMode::Private, "reject", 1, true)),
        None,
    );

    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "body: {}",
        refused.text
    );
    assert_eq!(
        refused.problem_type(),
        Some(problem_type("rate_limit.exceeded.v1"))
    );
    assert_eq!(refused.header(ERROR_SOURCE), Some("gateway"));
    assert_eq!(mock.calls(), 2, "a refused request is never dialled");
    Ok(())
}

/// The quota headers ride on a success and on a 429, with a retry guidance.
#[tokio::test]
async fn the_quota_headers_ride_on_a_success_and_on_a_429() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(1, 2, SharingMode::Private, "reject", 1, true)),
        None,
    );

    let allowed = proxy_with(&harness, &[]).await;
    assert_eq!(
        allowed.header("x-ratelimit-limit"),
        Some("1"),
        "the effective sustained rate per window"
    );
    assert_eq!(allowed.header("x-ratelimit-remaining"), Some("1"));
    let reset = allowed
        .header("x-ratelimit-reset")
        .and_then(|value| value.parse::<u64>().ok())
        .context("x-ratelimit-reset must be an epoch second")?;
    assert!(reset > 0, "the bucket refills at {reset}");

    let second = proxy_with(&harness, &[]).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(second.header("x-ratelimit-remaining"), Some("0"));
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-limit"), Some("1"));
    assert_eq!(refused.header("x-ratelimit-remaining"), Some("0"));
    let retry = refused
        .header("retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .context("a 429 carries Retry-After")?;
    assert!(retry >= 1, "RFC 6585 asks for seconds, not for zero");
    Ok(())
}

/// A costly request consumes its cost, so a bucket admits fewer of them.
#[tokio::test]
async fn a_cost_of_ten_admits_fewer_requests_than_a_cost_of_one() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    // One request of cost 4 empties a 4-token bucket for good.
    let costly = harness_with(
        server.port(),
        Some(rate_limit(4, 4, SharingMode::Private, "reject", 4, true)),
        None,
    );
    assert_eq!(proxy_with(&costly, &[]).await.status, StatusCode::OK);
    assert_eq!(
        proxy_with(&costly, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    // The same bucket at cost 1 serves four.
    let cheap = harness_with(
        server.port(),
        Some(rate_limit(4, 4, SharingMode::Private, "reject", 1, true)),
        None,
    );
    for _ in 0..4 {
        assert_eq!(proxy_with(&cheap, &[]).await.status, StatusCode::OK);
    }
    assert_eq!(
        proxy_with(&cheap, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    Ok(())
}

/// `response_headers: false` suppresses the three quota headers.
#[tokio::test]
async fn a_policy_without_response_headers_emits_none() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(5, 5, SharingMode::Private, "reject", 1, false)),
        None,
    );
    let reply = proxy_with(&harness, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.header("x-ratelimit-limit").is_none());
    assert!(reply.header("x-ratelimit-remaining").is_none());
    assert!(reply.header("x-ratelimit-reset").is_none());
    Ok(())
}

/// An ancestor that enforces its policy caps a child configured higher.
#[tokio::test]
async fn an_ancestor_that_enforces_caps_a_higher_child() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = chained_harness(
        server.port(),
        Some(rate_limit(
            1_000,
            1_000,
            SharingMode::Enforce,
            "reject",
            1,
            true,
        )),
        Some(rate_limit(2, 2, SharingMode::Enforce, "reject", 1, true)),
    );
    // The effective rate is the ancestor's: two requests, then the refusal.
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-limit"), Some("2"));
    assert_eq!(mock.calls(), 2);
    Ok(())
}

/// `inherit` with no own limit takes the ancestor's.
#[tokio::test]
async fn an_inherited_policy_without_an_own_limit_uses_the_ancestors() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = chained_harness(
        server.port(),
        Some(rate_limit(
            1_000,
            1_000,
            SharingMode::Inherit,
            "reject",
            1,
            true,
        )),
        Some(rate_limit(1, 1, SharingMode::Inherit, "reject", 1, true)),
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-limit"), Some("1"));
    Ok(())
}

/// An ancestor that *enforces* its cap keeps it active however the descendant
/// shares, so a `private` child cannot walk around it (DESIGN: "ancestor
/// constraints with `sharing: enforce` remain active").
#[tokio::test]
async fn an_enforce_ancestor_still_caps_a_private_descendant() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = chained_harness(
        server.port(),
        Some(rate_limit(50, 50, SharingMode::Private, "reject", 1, true)),
        Some(rate_limit(1, 1, SharingMode::Enforce, "reject", 1, true)),
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-limit"), Some("1"));
    Ok(())
}

/// A policy the resolved upstream declares on itself is a level of its own, so
/// its `enforce` cap applies under a route policy that would allow more.
#[tokio::test]
async fn an_upstream_cap_survives_a_route_policy_of_its_own() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = ProxyHarness::new();
    let record = upstream(
        harness.tenant(),
        ALIAS,
        server.port(),
        Some(windowed_policy(1, 1, "second", SharingMode::Enforce)),
        None,
    );
    let id = harness.seed_upstream(record);
    seed_route_of(
        &harness,
        harness.tenant(),
        id,
        "/v1/chat",
        Some(rate_limit(
            1_000,
            1_000,
            SharingMode::Private,
            "reject",
            1,
            true,
        )),
        None,
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.header("x-ratelimit-limit"),
        Some("1"),
        "the upstream's cap, not the route's allowance"
    );
    Ok(())
}

/// A mixed-window chain compares the policies per second: ten a second caps a
/// thousand a minute, reported in the window of the level that asked.
#[tokio::test]
async fn a_faster_ancestor_caps_a_slower_child_across_windows() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = chained_harness(
        server.port(),
        Some(windowed_policy(
            1_000,
            1_000,
            "minute",
            SharingMode::Private,
        )),
        Some(windowed_policy(10, 10, "second", SharingMode::Enforce)),
    );
    // The burst goes out at once: a refill of ten a second would hand the
    // eleventh request a token if the ten dialled one by one.
    let burst: Vec<_> = (0..10).map(|_| proxy_with(&harness, &[])).collect();
    for reply in futures_util::future::join_all(burst).await {
        assert_eq!(reply.status, StatusCode::OK);
    }
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.header("x-ratelimit-limit"),
        Some("600"),
        "10 a second, restated in the minute the route asked for"
    );
    Ok(())
}

/// The other direction: a hundred a minute caps five a second, and the rate
/// that reaches the client agrees with the refill that enforces it.
#[tokio::test]
async fn a_slower_ancestor_caps_a_faster_child_across_windows() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = chained_harness(
        server.port(),
        Some(windowed_policy(5, 1, "second", SharingMode::Private)),
        Some(windowed_policy(100, 100, "minute", SharingMode::Enforce)),
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let refused = proxy_with(&harness, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.header("x-ratelimit-limit"),
        Some("1"),
        "100 a minute floors to one token a second"
    );
    assert!(refused.header("retry-after").is_some());
    Ok(())
}

/// `tenant` is keyed on the *calling* tenant, so two callers of one shared
/// upstream never spend each other's budget.
#[tokio::test]
async fn a_tenant_scoped_counter_follows_the_caller() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = root_harness(
        server.port(),
        Some(scoped_policy(1, 1, "tenant", SharingMode::Private)),
        None,
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    // A second caller of the same record starts from a full bucket.
    let other = Uuid::now_v7();
    assert_eq!(
        harness
            .proxy_as("GET", PROXY_PATH, other, &[], b"")
            .await?
            .status,
        StatusCode::OK
    );
    // ... and the first one is refused, because its own bucket is spent.
    assert_eq!(
        proxy_with(&harness, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(mock.calls(), 2);
    Ok(())
}

/// `user` is keyed on the subject, so two callers of one tenant have separate
/// budgets while one caller is held to its own.
#[tokio::test]
async fn a_user_scoped_counter_follows_the_subject() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(scoped_policy(1, 1, "user", SharingMode::Private)),
        None,
    );
    let tenant = harness.tenant();
    let first = identity_of(tenant, Uuid::now_v7());
    let second = identity_of(tenant, Uuid::now_v7());
    assert_eq!(
        proxy_identity(&harness, &first, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        proxy_identity(&harness, &first, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        proxy_identity(&harness, &second, &[]).await.status,
        StatusCode::OK,
        "the other subject's bucket is untouched"
    );
    assert_eq!(mock.calls(), 2);
    Ok(())
}

/// `route` is keyed on the matched route, so two routes of one upstream are
/// two counters.
#[tokio::test]
async fn a_route_scoped_counter_follows_the_route() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(scoped_policy(1, 1, "route", SharingMode::Private)),
        None,
    );
    let id = harness
        .store()
        .find_upstream_by_alias(harness.tenant(), ALIAS)
        .unwrap_or_else(|error| panic!("the store must be readable: {error}"))
        .map(|record| record.id)
        .context("the seeded upstream must be there")?;
    seed_route_of(&harness, harness.tenant(), id, "/v1/other", None, None);
    let other_mock = server.mock(|when, then| {
        when.method(GET).path("/v1/other");
        then.status(200);
    });
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(
        harness
            .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/other", &[], b"")
            .await?
            .status,
        StatusCode::OK,
        "the other route has a bucket of its own"
    );
    assert_eq!(
        proxy_with(&harness, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(mock.calls(), 1);
    assert_eq!(other_mock.calls(), 1, "both routes were forwarded");
    Ok(())
}

/// Only a forwarded hop that parses as an address gets a bucket of its own, so
/// a rotated header cannot mint fresh counters.
#[tokio::test]
async fn a_rotated_forwarded_header_cannot_mint_buckets() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(scoped_policy(1, 1, "ip", SharingMode::Private)),
        None,
    );
    let garbage = ["garbage-1", "not-an-address", "0", "1.2.3.4.5.6.7.8.9"];
    for (index, hop) in garbage.iter().enumerate() {
        let status = proxy_with(&harness, &[("x-forwarded-for", hop)])
            .await
            .status;
        if index == 0 {
            assert_eq!(
                status,
                StatusCode::OK,
                "{hop}: the shared counter still has its token"
            );
        } else {
            assert_eq!(
                status,
                StatusCode::TOO_MANY_REQUESTS,
                "{hop}: an unparsable hop lands on the shared `unknown` counter"
            );
        }
    }
    assert_eq!(mock.calls(), 1, "every later hop was refused");
    // A parsable address is a client of its own.
    assert_eq!(
        proxy_with(&harness, &[("x-forwarded-for", "10.0.0.1")])
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

/// A PUT that tightens the policy starts a fresh bucket, so the new limit is
/// honoured on the next request instead of the budget the old one spent.
#[tokio::test]
async fn a_put_that_tightens_the_limit_is_honoured_on_the_next_request() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = ProxyHarness::new();
    // The record is seeded directly, so the alias stays `api.vendor.com` even
    // though a one-address pool derives `host:port`; the PUT then carries the
    // very pool that alias came from, which the write path reads as no
    // endpoint change.
    let id = harness.seed_upstream(upstream(
        harness.tenant(),
        ALIAS,
        server.port(),
        Some(rate_limit(1, 2, SharingMode::Private, "reject", 1, true)),
        None,
    ));
    seed_route(&harness, id);

    // The generous bucket serves two and refuses the third.
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(
        proxy_with(&harness, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    let body = serde_json::json!({
        "alias": ALIAS,
        "protocol": common::PROTOCOL_HTTP,
        "server": { "endpoints": [common::endpoint("http", "127.0.0.1", server.port())] },
        "rate_limit": policy_json(1, 1),
    });
    let reply = harness
        .call("PUT", &format!("/oagw/v1/upstreams/{id}"), Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(
        proxy_with(&harness, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the tightened capacity, not the spent budget of the old limit"
    );
    Ok(())
}

/// `sustained`/`burst` of a policy on the wire, for the write-path tests.
fn policy_json(rate: u64, capacity: u64) -> serde_json::Value {
    serde_json::json!({
        "sustained": { "rate": rate },
        "burst": { "capacity": capacity }
    })
}

/// An admitted `degrade` request is not reported as an exhausted one.
#[tokio::test]
async fn an_admitted_degraded_request_is_not_reported() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(1, 1, SharingMode::Private, "degrade", 1, true)),
        None,
    );
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let admitted = capture.lines();
    assert!(
        !admitted.iter().any(|line| line.contains("rate limit")),
        "an admitted request is not reported as an exhausted one: {admitted:?}"
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let exhausted = capture.lines();
    let reports = exhausted
        .iter()
        .filter(|line| line.contains("rate limit"))
        .count();
    assert_eq!(reports, 1, "one admission, one report: {exhausted:?}");
    Ok(())
}

/// `queue` waits for the token instead of refusing.
#[tokio::test]
async fn a_queued_request_is_forwarded_when_a_token_frees_up() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(4, 2, SharingMode::Private, "queue", 2, true)),
        None,
    );
    // The first request empties the bucket, so the second has to queue for
    // half a second at four tokens a second; the head budget covers it.
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let reply = proxy_with(&harness, &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);
    assert_eq!(mock.calls(), 2);
    Ok(())
}

/// `degrade` serves the request and still reports the spend.
#[tokio::test]
async fn a_degraded_request_is_forwarded_and_still_reports() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(1, 1, SharingMode::Private, "degrade", 1, true)),
        None,
    );
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    let spent = proxy_with(&harness, &[]).await;
    assert_eq!(spent.status, StatusCode::OK, "body: {}", spent.text);
    assert_eq!(spent.header("x-ratelimit-remaining"), Some("0"));
    assert_eq!(mock.calls(), 2, "degrade never blocks");
    Ok(())
}

/// The write path rejects a policy it cannot enforce.
#[tokio::test]
async fn the_write_path_rejects_an_unenforceable_policy() -> Result<()> {
    let harness = Harness::new();
    let cases: Vec<(&str, serde_json::Value)> = Vec::from([
        (
            "unsupported algorithm",
            serde_json::json!({"sustained": {"rate": 5}, "algorithm": "sliding_window"}),
        ),
        ("zero rate", serde_json::json!({"sustained": {"rate": 0}})),
        (
            "unknown window",
            serde_json::json!({"sustained": {"rate": 5, "window": "week"}}),
        ),
        (
            "empty capacity",
            serde_json::json!({"sustained": {"rate": 5}, "burst": {"capacity": 0}}),
        ),
        (
            "unknown scope",
            serde_json::json!({"sustained": {"rate": 5}, "scope": "cluster"}),
        ),
        (
            "unknown strategy",
            serde_json::json!({"sustained": {"rate": 5}, "strategy": "shed"}),
        ),
        (
            "zero cost",
            serde_json::json!({"sustained": {"rate": 5}, "cost": 0}),
        ),
    ]);
    for (what, payload) in cases {
        let mut body = https_upstream("api.vendor.com", 443);
        body["rate_limit"] = payload;
        let reply = harness
            .call("POST", "/oagw/v1/upstreams", tenant(), Some(body))
            .await?;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{what}: {}",
            reply.text
        );
        assert_eq!(
            reply.problem_type(),
            Some(problem_type("validation.error.v1")),
            "{what}"
        );
    }
    Ok(())
}

/// A deleted upstream leaves no bucket behind: the recreated record starts
/// with a full one.
#[tokio::test]
async fn a_deleted_upstream_leaves_no_spent_bucket_behind() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let policy = rate_limit(1, 2, SharingMode::Private, "reject", 1, true);
    let harness = harness_with(server.port(), Some(policy.clone()), None);
    let id = harness
        .store()
        .find_upstream_by_alias(harness.tenant(), ALIAS)
        .unwrap_or_else(|error| panic!("the store must be readable: {error}"))
        .map(|record| record.id)
        .context("the seeded upstream must be there")?;
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(proxy_with(&harness, &[]).await.status, StatusCode::OK);
    assert_eq!(
        proxy_with(&harness, &[]).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    // The control plane cascades the deletion to the data plane.
    let reply = harness
        .call("DELETE", &format!("/oagw/v1/upstreams/{id}"), None)
        .await?;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);

    // The same record seeded again starts from a full bucket; the cascade
    // removed the route with the upstream, so that is seeded back too.
    let record = upstream(harness.tenant(), ALIAS, server.port(), Some(policy), None);
    seed_route(&harness, harness.seed_upstream(record));
    let reply = proxy_with(&harness, &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);
    assert_eq!(mock.calls(), 3);
    Ok(())
}

// ── CORS (ADR-0004) ──────────────────────────────────────────────────────

/// A preflight is answered locally, with the echo the ADR asks for.
#[tokio::test]
async fn a_preflight_is_answered_locally_with_the_echo() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["https://app.example.com"], &["GET"])),
    );
    let reply = harness
        .proxy(
            "OPTIONS",
            PROXY_PATH,
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "DELETE"),
                ("access-control-request-headers", "x-trace"),
            ],
            b"",
        )
        .await?;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(reply.header("access-control-allow-methods"), Some("DELETE"));
    assert_eq!(
        reply.header("access-control-allow-headers"),
        Some("x-trace")
    );
    assert_eq!(reply.header("access-control-max-age"), Some("86400"));
    assert_eq!(
        reply.header("vary"),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
    assert_eq!(mock.calls(), 0, "a preflight is never dialled");
    Ok(())
}

/// Preflight detection is not gated on the configuration.
#[tokio::test]
async fn a_preflight_is_answered_even_when_cors_is_disabled() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(server.port(), None, Some(cors_config(false, &[], &[])));
    let reply = harness
        .proxy(
            "OPTIONS",
            PROXY_PATH,
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "GET"),
            ],
            b"",
        )
        .await?;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// An allowed origin is forwarded and the answer names it.
#[tokio::test]
async fn an_allowed_origin_is_forwarded_and_echoed() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(
            true,
            &["https://app.example.com"],
            &["GET", "POST"],
        )),
    );
    let reply = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(reply.header("vary"), Some("Origin"));
    assert_eq!(mock.calls(), 1);
    Ok(())
}

/// A disallowed origin is refused before the dial.
#[tokio::test]
async fn a_disallowed_origin_is_refused_without_a_dial() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["https://app.example.com"], &["GET"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://evil.example.org")]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("cors.origin_not_allowed.v1"))
    );
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// A method the policy does not allow is refused before the dial.
#[tokio::test]
async fn a_disallowed_method_is_refused_without_a_dial() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["*"], &["GET"])),
    );
    let reply = harness
        .proxy(
            "POST",
            PROXY_PATH,
            &[("origin", "https://app.example.com")],
            b"",
        )
        .await?;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("cors.method_not_allowed.v1"))
    );
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// Origin matching is port- and protocol-sensitive.
#[tokio::test]
async fn an_origin_is_matched_port_and_protocol_sensitively() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["https://app.example.com"], &["GET"])),
    );
    for origin in ["https://app.example.com:8443", "http://app.example.com"] {
        let reply = proxy_with(&harness, &[("origin", origin)]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{origin}");
        assert_eq!(
            reply.problem_type(),
            Some(problem_type("cors.origin_not_allowed.v1")),
            "{origin}"
        );
    }
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// No suffix matching either: a shared suffix is not a shared origin.
#[tokio::test]
async fn a_shared_suffix_is_not_a_shared_origin() -> Result<()> {
    let server = MockServer::start();
    let mock = ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["https://example.com"], &["GET"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://evil.com.example.com")]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// `expose_headers` and `allow_credentials` reach the response.
#[tokio::test]
async fn the_credential_and_expose_members_reach_the_response() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_policy(cors_config(
            true,
            &["https://app.example.com"],
            &["GET", "POST"],
        ))),
    );
    let reply = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com"),
        "a credentialed answer echoes the origin instead of `*`"
    );
    assert_eq!(
        reply.header("access-control-allow-credentials"),
        Some("true")
    );
    assert_eq!(
        reply.header("access-control-expose-headers"),
        Some("x-request-id, x-trace")
    );
    Ok(())
}

/// A wildcard policy without credentials answers the wildcard itself.
#[tokio::test]
async fn a_wildcard_policy_without_credentials_answers_the_wildcard() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["*"], &["GET", "POST"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("access-control-allow-origin"), Some("*"));
    assert!(reply.header("access-control-allow-credentials").is_none());
    Ok(())
}

/// The write path refuses a wildcard origin behind credentials.
#[tokio::test]
async fn a_wildcard_origin_with_credentials_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let mut body = https_upstream("api.vendor.com", 443);
    body["cors"] = serde_json::json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allow_credentials": true
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("validation.error.v1"))
    );
    assert!(
        reply.text.contains("allow_credentials"),
        "the detail names the ADR rule: {}",
        reply.text
    );
    Ok(())
}

/// CORS off: no header of its own on an otherwise identical request.
#[tokio::test]
async fn a_disabled_policy_adds_no_cors_header() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(false, &["*"], &["GET"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.header("access-control-allow-origin").is_none());
    assert!(reply.header("access-control-allow-credentials").is_none());
    assert!(reply.header("access-control-expose-headers").is_none());
    Ok(())
}

/// An upstream origin set that *enforces* is what a route policy of its own is
/// judged against, so the route cannot widen it.
#[tokio::test]
async fn an_upstream_cors_set_survives_a_route_policy_of_its_own() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = ProxyHarness::new();
    let record = upstream(
        harness.tenant(),
        ALIAS,
        server.port(),
        None,
        Some(shared_cors(
            SharingMode::Enforce,
            &["https://parent.example.com"],
        )),
    );
    let id = harness.seed_upstream(record);
    seed_route_of(
        &harness,
        harness.tenant(),
        id,
        "/v1/chat",
        None,
        Some(shared_cors(
            SharingMode::Enforce,
            &["https://route.example.com"],
        )),
    );
    // The upstream's origin is the one that counts.
    let allowed = proxy_with(&harness, &[("origin", "https://parent.example.com")]).await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.text);
    // ... and the route's is not.
    let refused = proxy_with(&harness, &[("origin", "https://route.example.com")]).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text);
    Ok(())
}

/// A 429 is still a CORS answer: the origin was allowed, so the browser gets
/// the allow-origin next to the quota headers.
#[tokio::test]
async fn a_429_for_an_allowed_origin_still_speaks_cors() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(1, 1, SharingMode::Private, "reject", 1, true)),
        Some(cors_config(true, &["https://app.example.com"], &["GET"])),
    );
    assert_eq!(
        proxy_with(&harness, &[("origin", "https://app.example.com")])
            .await
            .status,
        StatusCode::OK
    );
    let refused = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(refused.header("vary"), Some("Origin"));
    Ok(())
}

/// A CORS refusal answers the browser too, and it never names the origin it
/// refused.
#[tokio::test]
async fn a_cors_refusal_carries_no_allow_origin() -> Result<()> {
    let server = MockServer::start();
    ok_mock(&server);
    let harness = harness_with(
        server.port(),
        Some(rate_limit(10, 10, SharingMode::Private, "reject", 1, true)),
        Some(cors_config(true, &["https://app.example.com"], &["GET"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://evil.example.org")]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.header("vary"), Some("Origin"));
    assert!(reply.header("access-control-allow-origin").is_none());
    Ok(())
}

/// An enabled policy is authoritative: the upstream's own CORS answer does not
/// ride next to the gateway's.
#[tokio::test]
async fn an_enabled_policy_replaces_the_upstreams_own_cors_answer() -> Result<()> {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200)
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-credentials", "true")
            .header("content-type", "text/plain");
    });
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(true, &["https://app.example.com"], &["GET"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com"),
        "the gateway's answer, not the upstream's wildcard"
    );
    assert!(reply.header("access-control-allow-credentials").is_none());
    Ok(())
}

/// With the policy off, the gateway says nothing about CORS and the upstream's
/// own answer is the client's answer.
#[tokio::test]
async fn a_disabled_policy_leaves_the_upstreams_cors_answer_alone() -> Result<()> {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).header(
            "access-control-allow-origin",
            "https://upstream.example.com",
        );
    });
    let harness = harness_with(
        server.port(),
        None,
        Some(cors_config(false, &["https://app.example.com"], &["GET"])),
    );
    let reply = proxy_with(&harness, &[("origin", "https://app.example.com")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://upstream.example.com"),
        "nothing of ours replaced it"
    );
    Ok(())
}

/// The write path accepts a CORS policy that satisfies ADR-0004.
#[tokio::test]
async fn the_write_path_accepts_a_well_formed_cors_policy() -> Result<()> {
    let harness = Harness::new();
    let mut body = https_upstream("api.vendor.com", 443);
    body["cors"] = serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET", "PUT"],
        "expose_headers": ["x-request-id"],
        "allow_credentials": true
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let stored = reply
        .json
        .get("cors")
        .and_then(|cors| cors.get("allow_credentials"))
        .and_then(serde_json::Value::as_bool);
    assert_eq!(stored, Some(true), "the policy round-trips: {}", reply.text);
    Ok(())
}
