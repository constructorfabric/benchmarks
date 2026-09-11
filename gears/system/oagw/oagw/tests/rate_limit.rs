//! AT-6: rate limiting (`contracts/proxy-api.md` § 4, `ADR/0003`).
//!
//! An exhausted bucket answers `429` with `Retry-After` and the
//! `X-RateLimit-*` headers; the headers also ride along on every accepted
//! request that is subject to a limit.

mod common;

use axum::http::StatusCode;
use common::{Caller, app, create_route, create_upstream, route_body};

const PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// An upstream body carrying a sustained rate of `rate` per `window`.
fn limited_upstream(alias: &str, port: u16, rate: u32, window: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": port }
        ]},
        "protocol": common::PROTOCOL_HTTP,
        "rate_limit": {
            "sustained": { "rate": rate, "window": window },
            "burst": { "capacity": rate },
            "scope": "tenant"
        }
    })
    .to_string()
}

#[tokio::test]
async fn the_second_immediate_request_is_rejected_with_429() {
    let addr = common::free_port().await;
    let alias = format!("limited-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &limited_upstream(&alias, addr, 1, "hour")).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");

    let (status, headers, body) = common::send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    // The upstream is not listening, so the accepted request fails at the
    // dial; the point is that it *was* accepted and the budget was charged.
    assert_eq!(
        headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("0")
    );

    let (status, headers, body) = common::send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.rate_limit.exceeded.v1")
    );
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("3600")
    );
    assert_eq!(
        headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("0")
    );
    assert!(headers.get("x-ratelimit-reset").is_some());
    assert_eq!(body["retry_after_seconds"], 3600);
}

#[tokio::test]
async fn a_route_limit_is_stricter_than_the_upstream_limit() {
    let addr = common::free_port().await;
    let alias = format!("nested-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream =
        create_upstream(&app, &caller, &limited_upstream(&alias, addr, 100, "hour")).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let route = serde_json::json!({
        "enabled": true,
        "upstream_id": id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": [], "path_suffix_mode": "append"
        }},
        "rate_limit": {
            "sustained": { "rate": 2, "window": "minute" },
            "burst": { "capacity": 2 }
        }
    })
    .to_string();
    let _ = create_route(&app, &caller, &route).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    // The upstream allows 100/hour; the route caps the same window at 2/minute.
    let (_, headers, _) = common::send(&app, &caller, "GET", &path, None).await;
    assert_eq!(
        headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );

    let (status, _, body) = common::send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let (status, _, body) = common::send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    // Two tokens per minute: the next one accrues in half the window.
    assert_eq!(body["retry_after_seconds"], 30);
}

#[tokio::test]
async fn an_unlimited_upstream_is_never_rejected() {
    let addr = common::free_port().await;
    let alias = format!("open-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &common::upstream_body(&alias, addr)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    for _ in 0..5 {
        let (status, headers, _) = common::send(&app, &caller, "GET", &path, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(headers.get("x-ratelimit-limit").is_none());
        assert!(headers.get("retry-after").is_none());
    }
}

#[tokio::test]
async fn user_scoped_buckets_are_independent_per_subject() {
    let addr = common::free_port().await;
    let alias = format!("users-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(
        &app,
        &caller,
        &serde_json::json!({
            "enabled": true,
            "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": addr }
            ]},
            "protocol": common::PROTOCOL_HTTP,
            "rate_limit": {
                "sustained": { "rate": 1, "window": "hour" },
                "burst": { "capacity": 1 },
                "scope": "user"
            }
        })
        .to_string(),
    )
    .await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    // Each subject gets its own bucket, so both subjects are accepted once.
    let (first, _, _) = common::send(&app, &caller, "GET", &path, None).await;
    let other = Caller {
        tenant_id: caller.tenant_id,
        subject_id: uuid::Uuid::new_v4(),
    };
    let (second, _, _) = common::send(&app, &other, "GET", &path, None).await;
    assert_eq!(first, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(second, StatusCode::SERVICE_UNAVAILABLE);

    // The first subject's bucket is now empty.
    let (third, _, body) = common::send(&app, &caller, "GET", &path, None).await;
    assert_eq!(third, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.rate_limit.exceeded.v1")
    );
}
