//! Router-level tests for the token-bucket rate limiter.
//!
//! The limiter is in-memory and owned by the data plane; the scenarios here are
//! the observable consequences of that: what is allowed, what is refused, what
//! a refused caller is told and what the upstream never sees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{GATEWAY_SOURCE, Harness, JsonConfig, record, request};

/// A gateway with one upstream carrying `rate_limit`, if given.
async fn gateway_with_rate_limit(rate_limit: serde_json::Value) -> Harness {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream("echo", Some(serde_json::json!({ "rate_limit": rate_limit })))
        .await;
    harness
}

/// How many of `n` back-to-back requests the gateway lets through.
async fn let_through(harness: &Harness, n: usize) -> usize {
    let mut allowed = 0;
    for _ in 0..n {
        let response = record(
            harness
                .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
                .await,
        )
        .await;
        if response.status == StatusCode::OK {
            allowed += 1;
        }
    }
    allowed
}

#[tokio::test]
async fn requests_within_the_sustained_rate_are_allowed() {
    let harness = gateway_with_rate_limit(serde_json::json!({
        "sustained": { "rate": 5, "window": "second" },
    }))
    .await;
    assert_eq!(
        let_through(&harness, 3).await,
        3,
        "three requests fit a bucket of five"
    );
}

#[tokio::test]
async fn burst_capacity_allows_a_burst_above_the_sustained_rate() {
    let harness = gateway_with_rate_limit(serde_json::json!({
        "sustained": { "rate": 1, "window": "second" },
        "burst": { "capacity": 5 },
    }))
    .await;
    assert_eq!(
        let_through(&harness, 5).await,
        5,
        "the burst capacity, not the sustained rate, is the bucket size"
    );
}

#[tokio::test]
async fn without_a_burst_the_capacity_is_the_sustained_rate() {
    let harness = gateway_with_rate_limit(serde_json::json!({
        "sustained": { "rate": 1, "window": "second" },
    }))
    .await;
    let allowed = let_through(&harness, 3).await;
    assert_eq!(allowed, 1, "one token, so only the first request passes: {allowed}");
}

#[tokio::test]
async fn a_rejected_request_is_a_429_that_never_reaches_the_upstream() {
    let harness = gateway_with_rate_limit(serde_json::json!({
        "sustained": { "rate": 1, "window": "second" },
    }))
    .await;
    assert_eq!(let_through(&harness, 1).await, 1);
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert!(
        response.header("retry-after").is_some(),
        "the caller is told when it may retry"
    );
    assert_eq!(
        response.header("x-ratelimit-limit"),
        Some("1"),
        "the budget the bucket was built with"
    );
    assert_eq!(
        response.header("x-ratelimit-remaining"),
        Some("0"),
        "nothing is left"
    );
    assert!(
        response
            .header("x-ratelimit-reset")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some(),
        "the reset is a number of seconds"
    );
}

#[tokio::test]
async fn the_bucket_refills_after_a_window() {
    let harness = gateway_with_rate_limit(serde_json::json!({
        "sustained": { "rate": 1, "window": "second" },
    }))
    .await;
    let first = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let exhausted = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(exhausted.status, StatusCode::TOO_MANY_REQUESTS);

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let refilled = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        refilled.status,
        StatusCode::OK,
        "a second later the tokens are back: {}",
        refilled.raw
    );
}

#[tokio::test]
async fn a_cost_of_two_drains_the_budget_twice_as_fast() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .upstream_with_route(
            "echo",
            Some(serde_json::json!({
                "rate_limit": {
                    "sustained": { "rate": 4, "window": "second" },
                    "cost": 2,
                },
            })),
            None,
        )
        .await;
    // 3 requests × 2 tokens against a 4-token bucket: the third cannot be paid.
    let allowed = let_through(&harness, 3).await;
    assert_eq!(allowed, 2, "two requests consume the four-token budget: {allowed}");
}

#[tokio::test]
async fn a_stricter_route_limit_wins_and_leaves_other_routes_alone() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    let (upstream_id, _) = harness
        .upstream_with_route(
            "shared",
            Some(serde_json::json!({
                "rate_limit": { "sustained": { "rate": 1000, "window": "second" } },
            })),
            None,
        )
        .await;
    // A second route on the same upstream, carrying the tighter limit.
    let response = harness
        .serve(request(
            "POST",
            "/oagw/v1/routes",
            Some(serde_json::json!({
                "upstream_id": upstream_id,
                "match": {
                    "http": { "methods": ["GET"], "path": "/status", "query_allowlist": [] }
                },
                "rate_limit": { "sustained": { "rate": 2, "window": "second" } },
            })),
        ))
        .await;
    let recorded = record(response).await;
    assert_eq!(recorded.status, StatusCode::CREATED, "route: {}", recorded.raw);

    // Two on the narrow route fit; the third does not.
    let mut narrow_allowed = 0;
    for _ in 0..3 {
        let response = record(
            harness
                .serve(request("GET", "/oagw/v1/proxy/shared/status/200", None))
                .await,
        )
        .await;
        if response.status == StatusCode::OK {
            narrow_allowed += 1;
        } else {
            assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS, "{}", response.raw);
        }
    }
    assert_eq!(narrow_allowed, 2, "the route's own limit is what applies: {narrow_allowed}");

    // The other route of the same upstream still has its thousand.
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/shared/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a sibling route is not throttled by its neighbour: {}",
        response.raw
    );
}

#[tokio::test]
async fn a_looser_route_limit_does_not_loosen_the_upstream() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    let (upstream_id, _) = harness
        .upstream_with_route(
            "tight",
            Some(serde_json::json!({
                "rate_limit": { "sustained": { "rate": 2, "window": "second" } },
            })),
            None,
        )
        .await;
    // A second route declaring a far wider budget than its upstream allows.
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": {
                        "http": { "methods": ["GET"], "path": "/status", "query_allowlist": [] }
                    },
                    "rate_limit": { "sustained": { "rate": 1000, "window": "second" } },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.raw);

    // Two fit the upstream's budget of two; the third is refused even though
    // the route it matched declared a thousand.
    let mut allowed = 0;
    for _ in 0..3 {
        let response = record(
            harness
                .serve(request("GET", "/oagw/v1/proxy/tight/status/200", None))
                .await,
        )
        .await;
        if response.status == StatusCode::OK {
            allowed += 1;
        } else {
            assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS, "{}", response.raw);
            assert_eq!(response.header("x-ratelimit-limit"), Some("2"));
        }
    }
    assert_eq!(
        allowed, 2,
        "the merged budget is the tighter of the two levels: {allowed}"
    );
}

#[tokio::test]
async fn a_sliding_window_refuses_beyond_its_rate() {
    let harness = gateway_with_rate_limit(serde_json::json!({
        "algorithm": "sliding_window",
        "sustained": { "rate": 2, "window": "second" },
    }))
    .await;
    assert_eq!(let_through(&harness, 2).await, 2);
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS, "{}", response.raw);
    assert_eq!(response.header("x-ratelimit-limit"), Some("2"));
}
