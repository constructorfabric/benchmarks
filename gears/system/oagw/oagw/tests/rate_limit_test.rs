//! Rate-limit enforcement (T044): the token bucket behind
//! `rate_limit.sustained` + `burst`, the `429` problem response with its
//! `Retry-After`, and the `X-RateLimit-*` advertising ADR 0003 makes
//! conditional on `response_headers`.
//!
//! The buckets live in the harness's own store, so every test starts from an
//! empty one and no test can deplete another's quota.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use httpmock::prelude::*;
use serde_json::json;

/// A gear whose upstream carries the given rate limit.
async fn gear_with_limit(limit: serde_json::Value) -> (Harness, MockServer) {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("metered", "127.0.0.1", stub.port(), "http");
    body["rate_limit"] = limit;
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "upstream created");
    let upstream_id = read_json(response).await["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;
    (harness, stub)
}

/// One metered `GET`, for tests that only look at status and headers.
async fn get(harness: &Harness) -> axum::http::Response<axum::body::Body> {
    harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/metered/v1/models",
            &[],
            None,
        ))
        .await
}

fn header(response: &axum::http::Response<axum::body::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[tokio::test]
async fn the_first_request_reports_the_counters() {
    let (harness, _stub) = gear_with_limit(json!({
        "sustained": { "rate": 3, "window": "second" }
    }))
    .await;

    let response = get(&harness).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-ratelimit-limit"), Some("3".into()));
    assert_eq!(header(&response, "x-ratelimit-remaining"), Some("2".into()));
    let reset = header(&response, "x-ratelimit-reset").expect("reset advertised");
    assert!(reset.parse::<i64>().is_ok(), "`{reset}` is an epoch second");
}

#[tokio::test]
async fn exhausting_the_bucket_returns_429_with_retry_after() {
    let (harness, _stub) = gear_with_limit(json!({
        "sustained": { "rate": 1, "window": "second" },
        "burst": { "capacity": 1 }
    }))
    .await;

    assert_eq!(get(&harness).await.status(), StatusCode::OK);
    let response = get(&harness).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    let retry_after = header(&response, "retry-after").expect("Retry-After on a 429");
    assert!(
        retry_after.parse::<u64>().is_ok() && retry_after != "0",
        "`Retry-After: {retry_after}` names a positive wait"
    );
    assert_eq!(header(&response, "x-oagw-error-source"), Some("gateway".into()));

    let problem = read_json(response).await;
    assert_eq!(problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
    assert_eq!(problem["status"], 429);
    assert_eq!(problem["retry_after_seconds"], retry_after.parse::<u64>().unwrap());
    assert!(problem["title"].as_str().is_some());
}

#[tokio::test]
async fn the_bucket_recovers_when_the_window_passes() {
    let (harness, _stub) = gear_with_limit(json!({
        "sustained": { "rate": 20, "window": "second" },
        "burst": { "capacity": 1 }
    }))
    .await;

    assert_eq!(get(&harness).await.status(), StatusCode::OK);
    assert_eq!(get(&harness).await.status(), StatusCode::TOO_MANY_REQUESTS);
    // 20 tokens a second refill the single capacity in well under a second.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(get(&harness).await.status(), StatusCode::OK, "the bucket refilled");
}

#[tokio::test]
async fn cost_consumes_more_than_one_token() {
    let (harness, _stub) = gear_with_limit(json!({
        "sustained": { "rate": 4, "window": "second" },
        "cost": 2
    }))
    .await;

    assert_eq!(header(&get(&harness).await, "x-ratelimit-remaining"), Some("2".into()));
    assert_eq!(header(&get(&harness).await, "x-ratelimit-remaining"), Some("0".into()));
    assert_eq!(get(&harness).await.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn response_headers_false_withholds_the_counters() {
    let (harness, _stub) = gear_with_limit(json!({
        "sustained": { "rate": 5, "window": "second" },
        "response_headers": false
    }))
    .await;

    let response = get(&harness).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-ratelimit-limit"), None);
    assert_eq!(header(&response, "x-ratelimit-remaining"), None);
    assert_eq!(header(&response, "x-ratelimit-reset"), None);
    assert_eq!(header(&response, "x-oagw-error-source"), Some("upstream".into()));
}

#[tokio::test]
async fn withheld_counters_stay_withheld_when_the_limit_is_exceeded() {
    let (harness, _stub) = gear_with_limit(json!({
        "sustained": { "rate": 1, "window": "second" },
        "burst": { "capacity": 1 },
        "response_headers": false
    }))
    .await;

    assert_eq!(get(&harness).await.status(), StatusCode::OK);
    let response = get(&harness).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // `Retry-After` is not a counter: it is the instruction the caller needs.
    assert!(header(&response, "retry-after").is_some());
    assert_eq!(header(&response, "x-ratelimit-limit"), None);
    assert_eq!(header(&response, "x-ratelimit-remaining"), None);
    assert_eq!(header(&response, "x-ratelimit-reset"), None);
}

#[tokio::test]
async fn an_upstream_without_a_rate_limit_is_never_metered() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/open");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let upstream_id = create_upstream(&harness, "open", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/open", &["GET"]).await;

    for _ in 0..6 {
        let response = harness
            .send(harness.proxy_request("GET", "/oagw/v1/proxy/open/v1/open", &[], None))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "x-ratelimit-limit"), None);
    }
}

#[tokio::test]
async fn buckets_are_independent_per_upstream() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET);
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let limit = json!({
        "sustained": { "rate": 1, "window": "second" },
        "burst": { "capacity": 1 }
    });
    for alias in ["first", "second"] {
        let mut body = upstream_body(alias, "127.0.0.1", stub.port(), "http");
        body["rate_limit"] = limit.clone();
        let response = harness
            .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let id = read_json(response).await["id"].as_str().unwrap_or_default().to_string();
        create_route(&harness, &id, "/v1/models", &["GET"]).await;
    }

    let path = |alias: &str| format!("/oagw/v1/proxy/{alias}/v1/models");
    assert_eq!(
        harness
            .send(harness.proxy_request("GET", &path("first"), &[], None))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        harness
            .send(harness.proxy_request("GET", &path("second"), &[], None))
            .await
            .status(),
        StatusCode::OK,
        "the second upstream's bucket is its own"
    );
}

#[tokio::test]
async fn a_route_limit_and_an_upstream_limit_yield_the_stricter_one() {
    // US5/AC3: the route overrides the upstream it is bound to, and what the
    // data plane meters is the *effective* limit — so a route limit of 1
    // governs an upstream limit of 10, on the proxy path.
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("layered", "127.0.0.1", stub.port(), "http");
    body["rate_limit"] = json!({
        "sustained": { "rate": 10, "window": "second" },
        "burst": { "capacity": 10 }
    });
    let created = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream_id = read_json(created).await["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let route_id = create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let route = harness
        .send(harness.request("GET", &format!("/oagw/v1/routes/{route_id}"), None))
        .await;
    let mut replacement = read_json(route).await;
    replacement["rate_limit"] = json!({
        "sustained": { "rate": 1, "window": "second" },
        "burst": { "capacity": 1 }
    });
    let updated = harness
        .send(harness.request("PUT", &format!("/oagw/v1/routes/{route_id}"), Some(replacement)))
        .await;
    assert_eq!(updated.status(), StatusCode::OK, "route limit set");

    let path = "/oagw/v1/proxy/layered/v1/models";
    let first = harness
        .send(harness.proxy_request("GET", path, &[], None))
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        header(&first, "x-ratelimit-limit"),
        Some("1".into()),
        "the stricter, route-level limit is the effective one"
    );
    let second = harness
        .send(harness.proxy_request("GET", path, &[], None))
        .await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}
