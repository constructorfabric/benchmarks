//! A rate limit is enforced where the gateway's traffic enters.
//!
//! An upstream that declares a small limit exhausts quickly; the interesting
//! part is not that the fourth call fails but what the fourth call says: which
//! headers, whose failure, and whether a neighbour's bucket was touched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use http_body_util::BodyExt;
use serde_json::json;
use std::time::Duration;

const BURST: usize = 3;

/// An upstream limited to `BURST` requests, with a route on `/v1/ping`.
async fn wired(app: &common::TestApp, upstream: &LocalUpstream, alias: &str) -> String {
    let spec = upstream.upstream_spec(alias);
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit",
            "rate_limit": {
                "sustained": {"rate": BURST, "window_secs": 1},
                "burst_capacity": BURST
            }
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    alias
}

async fn call(app: &common::TestApp, alias: &str, path: &str) -> http::Response<axum::body::Body> {
    app.send(app.request(
        http::Method::GET,
        &format!("/oagw/v1/proxy/{alias}{path}"),
        None,
        &[],
    ))
    .await
}

/// Drain the bucket, returning the number of calls it took.
async fn exhaust(app: &common::TestApp, alias: &str) -> http::Response<axum::body::Body> {
    let mut last = None;
    for _ in 0..=BURST {
        last = Some(call(app, alias, "/v1/ping").await);
    }
    last.expect("at least one call")
}

#[tokio::test]
async fn a_request_beyond_the_burst_is_refused_with_429() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "limited").await;

    let refused = exhaust(&app, &alias).await;
    assert_eq!(refused.status(), http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused
            .headers()
            .get(common::error_source_header())
            .and_then(|value| value.to_str().ok()),
        Some(common::error_source_gateway()),
        "the refusal is the gateway's own"
    );
}

#[tokio::test]
async fn the_refusal_names_the_rate_limit_problem() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "typed").await;

    let refused = exhaust(&app, &alias).await;
    let bytes = refused
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert_eq!(document["status"], 429, "{document}");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("rate_limit.exceeded.v1")),
        "{document}"
    );
    assert!(
        document["retry_after_seconds"].is_u64(),
        "the document says how long to wait: {document}"
    );
}

/// The refusal carries the budget the caller has left.
#[tokio::test]
async fn a_refusal_advertises_the_remaining_budget() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "advertised").await;

    let refused = exhaust(&app, &alias).await;
    let headers = refused.headers();
    let limit = headers
        .get("x-ratelimit-limit")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_eq!(limit.as_deref(), Some("3"), "{headers:?}");

    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_eq!(remaining.as_deref(), Some("0"));

    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    assert!(reset.is_some_and(|reset| reset > 0), "{headers:?}");
}

#[tokio::test]
async fn a_refusal_says_how_long_to_wait() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "retrying").await;

    let refused = exhaust(&app, &alias).await;
    let retry_after = refused
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    assert!(
        retry_after.is_some_and(|secs| secs >= 1),
        "the caller is told when to come back"
    );
}

/// A successful call inside the budget advertises the same headers.
#[tokio::test]
async fn a_successful_call_advertises_its_budget() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "inline").await;

    let response = call(&app, &alias, "/v1/ping").await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
}

/// An upstream without a limit is not throttled by its neighbour's bucket.
#[tokio::test]
async fn a_sibling_without_a_limit_is_unaffected() {
    let app = app().await;
    let limited = LocalUpstream::start().await;
    let free = LocalUpstream::start().await;

    let limited_alias = wired(&app, &limited, "sibling.limited").await;
    let free_spec = free.upstream_spec("sibling.free");
    let free_doc = app
        .create_upstream(json!({
            "alias": free_spec["alias"],
            "endpoints": free_spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let free_alias = free_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/open",
        "methods": ["GET"],
        "target_alias": free_alias,
        "strip_prefix": false
    }))
    .await;

    exhaust(&app, &limited_alias).await;
    for _ in 0..BURST + 3 {
        let response = call(&app, &free_alias, "/v1/open").await;
        assert_eq!(
            response.status(),
            http::StatusCode::OK,
            "the un-limited upstream keeps answering"
        );
    }
    assert_eq!(free.count(), BURST + 3);
}

/// The bucket refills: a refused call can be repeated after the window.
#[tokio::test]
async fn the_bucket_refills_after_the_window() {
    let app = common::app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("refilling");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit",
            "rate_limit": {
                "sustained": {"rate": 10, "window_secs": 1},
                "burst_capacity": 1
            }
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let first = call(&app, &alias, "/v1/ping").await;
    assert_eq!(first.status(), http::StatusCode::OK);
    let refused = call(&app, &alias, "/v1/ping").await;
    assert_eq!(refused.status(), http::StatusCode::TOO_MANY_REQUESTS);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let refilled = call(&app, &alias, "/v1/ping").await;
    assert_eq!(
        refilled.status(),
        http::StatusCode::OK,
        "the window passed and a token came back"
    );
}

/// A limit on the *route* holds its own bucket against the upstream's.
#[tokio::test]
async fn a_route_limit_is_enforced_on_its_own_path() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("routed");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/throttled",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "rate_limit": {
            "sustained": {"rate": 1, "window_secs": 1},
            "burst_capacity": 1
        }
    }))
    .await;

    let first = call(&app, &alias, "/v1/throttled").await;
    assert_eq!(first.status(), http::StatusCode::OK);
    let second = call(&app, &alias, "/v1/throttled").await;
    assert_eq!(second.status(), http::StatusCode::TOO_MANY_REQUESTS);
}

// --- the stricter-of merge -------------------------------------------------

/// A route limit looser than its upstream's does not loosen the pool: the
/// stricter of the two governs, so the upstream's own refusal still lands.
#[tokio::test]
async fn a_looser_route_limit_leaves_the_upstream_stricter() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("merge.upstream.strict");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit",
            "rate_limit": {
                "sustained": {"rate": 1, "window_secs": 60},
                "burst_capacity": 1
            }
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "rate_limit": {
            "sustained": {"rate": 100, "window_secs": 60},
            "burst_capacity": 100
        }
    }))
    .await;

    let first = call(&app, &alias, "/v1/ping").await;
    assert_eq!(first.status(), http::StatusCode::OK);
    let second = call(&app, &alias, "/v1/ping").await;
    assert_eq!(
        second.status(),
        http::StatusCode::TOO_MANY_REQUESTS,
        "the route's generous limit does not widen the pool's"
    );
}

/// A route limit stricter than its upstream's tightens the pool on that path
/// only, and the wider pool keeps answering beside it.
#[tokio::test]
async fn a_stricter_route_limit_tightens_its_own_path() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("merge.route.strict");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit",
            "rate_limit": {
                "sustained": {"rate": 100, "window_secs": 60},
                "burst_capacity": 100
            }
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/tight",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "rate_limit": {
            "sustained": {"rate": 1, "window_secs": 60},
            "burst_capacity": 1
        }
    }))
    .await;

    let first = call(&app, &alias, "/v1/tight").await;
    assert_eq!(first.status(), http::StatusCode::OK);
    let second = call(&app, &alias, "/v1/tight").await;
    assert_eq!(
        second.status(),
        http::StatusCode::TOO_MANY_REQUESTS,
        "the route's own tighter limit governs its path"
    );
}
