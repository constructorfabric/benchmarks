// Created: 2026-08-29 by Constructor Tech
//! Rate limiting over the wire: counters, `429` and the hierarchical `min()`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{json_body, parent, post, tenant};
use httpmock::MockServer;
use serde_json::{Value, json};

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn rate(rate: u32, capacity: u32) -> Value {
    json!({
        "sustained": { "rate": rate, "window": "second" },
        "burst": { "capacity": capacity },
    })
}

fn upstream(alias: &str, server: &MockServer, limit: Value) -> Value {
    json!({
        "alias": alias,
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "rate_limit": limit,
    })
}

#[tokio::test]
async fn fourth_request_is_rejected_with_429() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream("throttled.example.com", &server, rate(3, 3)),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    for expected in [200, 200, 200] {
        let response = harness
            .send(
                "GET",
                "/oagw/v1/proxy/throttled.example.com/api",
                None,
                tenant(),
            )
            .await;
        assert_eq!(response.status(), expected);
        // Counters travel with every accepted response.
        assert_eq!(
            common::header(&response, "x-ratelimit-limit").as_deref(),
            Some("3")
        );
        let remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        assert!(remaining.is_some(), "x-ratelimit-remaining must be present");
        let reset = response
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("x-ratelimit-reset must be a unix epoch");
        assert!(
            reset > 1_700_000_000,
            "reset must be an epoch value: {reset}"
        );
    }

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/throttled.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 429);
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    let retry_after = common::header(&response, "retry-after").expect("retry-after header");
    assert!(retry_after.parse::<u64>().unwrap() >= 1);
    // ADR-0003: the 429 reports the budget it exhausted, next to `Retry-After`.
    let limit = common::header(&response, "x-ratelimit-limit").expect("x-ratelimit-limit");
    assert_eq!(limit, "3", "the configured rate limit is reported");
    let remaining =
        common::header(&response, "x-ratelimit-remaining").expect("x-ratelimit-remaining");
    assert_eq!(
        remaining, "0",
        "the exhausted bucket reports no tokens left"
    );
    let reset = common::header(&response, "x-ratelimit-reset").expect("x-ratelimit-reset");
    assert!(
        reset.parse::<u64>().unwrap() > 1_700_000_000,
        "reset must be an epoch: {reset}"
    );
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(body["status"], 429);
    assert!(body["retry_after_seconds"].as_u64().unwrap() >= 1);
    assert_eq!(
        target.calls(),
        3,
        "the rejected request must not reach the upstream"
    );
}

#[tokio::test]
async fn counters_can_be_switched_off() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(
                "silent.example.com",
                &server,
                json!({ "sustained": { "rate": 5, "window": "second" }, "response_headers": false }),
            ),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/silent.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(common::header(&response, "x-ratelimit-limit"), None);
}

#[tokio::test]
async fn route_limit_can_be_stricter_than_the_upstream() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream("route-limit.example.com", &server, rate(10, 10)),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    let route = json!({
        "upstream_id": upstream_id,
        "rate_limit": rate(2, 2),
        "match": { "http": { "methods": ["GET"], "path": "/" } },
    });
    post(harness.router(), "/oagw/v1/routes", route, tenant()).await;

    for _ in 0..2 {
        let response = harness
            .send(
                "GET",
                "/oagw/v1/proxy/route-limit.example.com/api",
                None,
                tenant(),
            )
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            common::header(&response, "x-ratelimit-limit").as_deref(),
            Some("2")
        );
    }
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/route-limit.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 429);
}

#[tokio::test]
async fn shadowed_ancestor_limit_still_applies() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    // The ancestor's limit is stricter and its upstream is shadowed by the
    // descendant's, but an `enforce` ancestor limit still applies.
    let ancestor = json!({
        "alias": "hierarchy.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "rate_limit": json!({
            "sharing": "enforce",
            "sustained": { "rate": 2, "window": "second" },
            "burst": { "capacity": 2 },
        }),
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", ancestor, parent()).await).await;
    let descendant = json!({
        "alias": "hierarchy.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "rate_limit": rate(1000, 1000),
    });
    let descendant_id = json_body(
        post(harness.router(), "/oagw/v1/upstreams", descendant, tenant()).await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": descendant_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    for _ in 0..2 {
        let response = harness
            .send(
                "GET",
                "/oagw/v1/proxy/hierarchy.example.com/api",
                None,
                tenant(),
            )
            .await;
        assert_eq!(response.status(), 200);
    }
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/hierarchy.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 429);
}

#[tokio::test]
async fn an_upstream_limit_is_one_budget_across_its_routes() {
    // ADR-0003 keys the bucket on the configuring resource: an upstream limit
    // is a single budget, so two routes under the same upstream draw from the
    // same bucket instead of each getting a fresh one.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "shared-budget.example.com",
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "rate_limit": {
                    "sustained": { "rate": 2, "window": "second" },
                    "burst": { "capacity": 2 },
                    "scope": "tenant",
                },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    for path in ["/one", "/two"] {
        post(
            harness.router(),
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": path } },
            }),
            tenant(),
        )
        .await;
    }

    for (path, expected) in [
        ("/oagw/v1/proxy/shared-budget.example.com/one", 200),
        ("/oagw/v1/proxy/shared-budget.example.com/two", 200),
        ("/oagw/v1/proxy/shared-budget.example.com/one", 429),
    ] {
        let response = harness.send("GET", path, None, tenant()).await;
        assert_eq!(response.status(), expected, "{path}");
    }
}

#[tokio::test]
async fn unenforced_limit_strategies_are_refused_at_config_time() {
    let harness = common::Harness::new(common::test_config(), None);
    for strategy in ["queue", "degrade"] {
        let response = post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(
                &format!("{strategy}.example.com"),
                &MockServer::start(),
                json!({
                    "sustained": { "rate": 5, "window": "second" },
                    "strategy": strategy,
                }),
            ),
            tenant(),
        )
        .await;
        assert_eq!(response.status(), 400, "{strategy}");
    }
}

#[tokio::test]
async fn a_cost_larger_than_the_capacity_is_refused_at_config_time() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        upstream(
            "costly.example.com",
            &MockServer::start(),
            json!({
                "sustained": { "rate": 5, "window": "second" },
                "burst": { "capacity": 2 },
                "cost": 5,
            }),
        ),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);
}
