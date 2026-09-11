//! Black-box, external-crate tests for DECOMPOSITION entry 2.8
//! (rate-limiting), exercising the publicly reachable surface of
//! `oagw::*`.
//!
//! `oagw::policy::ratelimit` (the token-bucket engine, counter-key
//! selection, hierarchical composition, and header computation this
//! FEATURE implements) is **not** reachable from an external test crate:
//! `src/policy/mod.rs` (owned by DECOMPOSITION entry 2.5's call-site wiring,
//! out of this entry's file-ownership list) declares `mod ratelimit;` as
//! crate-private, exactly the boundary `tests/proxy_core.rs` and
//! `tests/upstream_management.rs` document for their own entries. The
//! thorough, direct unit tests this FEATURE's algorithms need (bucket
//! accounting/replenishment, scope-key selection, hierarchical `min()`
//! composition, header computation, and the reject/queue/degrade
//! strategies under concurrency) live as inline `#[cfg(test)] mod tests`
//! inside each of `src/policy/ratelimit/{bucket,key,limit,headers,engine}.rs`,
//! which -- being part of the `oagw` crate itself -- has the access this
//! file cannot.
//!
//! This file instead exercises the parts of the FEATURE that genuinely are
//! visible from `oagw`'s public API as a black-box consumer would: the
//! `rate_limit` sub-schema's documented defaults on `oagw::model::upstream`
//! (persisted by entries 2.2/2.3, read by this feature) and the
//! `RateLimitExceeded` row of `oagw::error`'s RFC 9457 catalog this
//! feature's `429` contract renders through
//! (`cpt-cf-oagw-dod-ratelimit-rejection-contract`).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use httpmock::prelude::*;
use oagw::OagwGear;
use oagw::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use oagw::model::upstream::{
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow,
    Sharing,
};
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx, RestApiCapability};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// `cpt-cf-oagw-dod-ratelimit-token-bucket`: a `rate_limit` object parses
/// with every documented default (`sharing: private`, `algorithm:
/// token_bucket`, `sustained.window: second`, `scope: tenant`, `strategy:
/// reject`, `cost: 1`) when only the required `sustained.rate` is given.
#[test]
fn rate_limit_config_applies_every_documented_default() {
    let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
        "sustained": { "rate": 100 }
    }))
    .unwrap();

    assert_eq!(config.sharing, Sharing::Private);
    assert_eq!(config.algorithm, RateLimitAlgorithm::TokenBucket);
    assert_eq!(config.sustained.rate, 100);
    assert_eq!(config.sustained.window, RateLimitWindow::Second);
    assert!(config.burst.is_none());
    assert_eq!(config.scope, RateLimitScope::Tenant);
    assert_eq!(config.strategy, RateLimitStrategy::Reject);
    assert_eq!(config.cost, 1);
}

/// `algorithm: sliding_window` is accepted as a legal persisted value, per
/// `cpt-cf-oagw-dod-ratelimit-token-bucket`'s explicit "accepted as a
/// configuration value" requirement.
#[test]
fn sliding_window_algorithm_is_a_legal_persisted_value() {
    let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
        "algorithm": "sliding_window",
        "sustained": { "rate": 10, "window": "minute" },
        "burst": { "capacity": 20 },
        "scope": "ip",
        "strategy": "queue",
        "cost": 3
    }))
    .unwrap();

    assert_eq!(config.algorithm, RateLimitAlgorithm::SlidingWindow);
    assert_eq!(config.sustained.window, RateLimitWindow::Minute);
    assert_eq!(config.burst.unwrap().capacity, Some(20));
    assert_eq!(config.scope, RateLimitScope::Ip);
    assert_eq!(config.strategy, RateLimitStrategy::Queue);
    assert_eq!(config.cost, 3);
}

/// `cpt-cf-oagw-dod-ratelimit-token-bucket`: all four documented
/// `sustained.window` units (`second`|`minute`|`hour`|`day`) are legal
/// persisted values -- the existing defaults/sliding-window tests above
/// only exercise `second` (the default) and `minute`; this closes the
/// `hour`/`day` gap in the externally-visible model contract that
/// `cpt-cf-oagw-algo-ratelimit-effective-limit` step `inst-ratelimit-effective-limit-09`'s
/// per-second normalization depends on every window unit being accepted.
#[test]
fn every_documented_sustained_window_unit_round_trips() {
    for window in ["second", "minute", "hour", "day"] {
        let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 5, "window": window },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(config.sustained.window).unwrap(),
            window
        );
    }
}

/// Every `scope`/`strategy`/`sharing` enum value round-trips through
/// serde, matching `cpt-cf-oagw-dod-ratelimit-scope-selection`'s five
/// scopes and `cpt-cf-oagw-dod-ratelimit-rejection-contract`'s three
/// strategies.
#[test]
fn every_documented_scope_and_strategy_value_round_trips() {
    for scope in ["global", "tenant", "user", "ip", "route"] {
        let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 1 },
            "scope": scope,
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(config.scope).unwrap(), scope);
    }
    for strategy in ["reject", "queue", "degrade"] {
        let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 1 },
            "strategy": strategy,
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(config.strategy).unwrap(), strategy);
    }
    for sharing in ["private", "inherit", "enforce"] {
        let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 1 },
            "sharing": sharing,
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(config.sharing).unwrap(), sharing);
    }
}

/// `cpt-cf-oagw-dod-ratelimit-rejection-contract`: the `RateLimitExceeded`
/// catalog row this feature's `429` renders through carries the documented
/// status, GTS type, and `X-OAGW-Error-Source: gateway` header, exactly as
/// `tests/proxy_core.rs` asserts for entry 2.5's own error kinds.
#[tokio::test]
async fn rate_limit_exceeded_renders_the_documented_429_envelope() {
    let response = OagwError::new(OagwErrorKind::RateLimitExceeded, "budget exhausted")
        .with_instance("/oagw/v1/proxy/vendor.com")
        .with_retry_after_seconds(30)
        .into_response();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    assert_eq!(
        response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("30")
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(json["status"], 429);
}

/// `cpt-cf-oagw-algo-ratelimit-headers` step `inst-ratelimit-headers-06`:
/// `Retry-After` is attached only when the caller opts in via
/// `with_retry_after_seconds` (the `strategy: reject` denial path); a
/// `429` rendered without it -- as engine callers that never reach that
/// builder call would produce -- carries no `Retry-After` header at all,
/// complementing the "with retry-after" assertion above rather than
/// assuming the header is always present on this error kind.
#[tokio::test]
async fn rate_limit_exceeded_without_retry_after_omits_the_header() {
    let response = OagwError::new(OagwErrorKind::RateLimitExceeded, "budget exhausted")
        .with_instance("/oagw/v1/proxy/vendor.com")
        .into_response();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get(header::RETRY_AFTER).is_none());
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
}

// ---------------------------------------------------------------------
// RF-003 regression: full-router, black-box proof that a `scope: ip`
// budget is genuinely per-resource, not silently collapsed to one shared
// `tenant`-keyed bucket. Driven through the real `oagw::OagwGear`, exactly
// the pattern `tests/cors_handling.rs` documents and uses.
// ---------------------------------------------------------------------

struct FixedConfig(Value);

impl ConfigProvider for FixedConfig {
    fn get_gear_config(&self, gear: &str) -> Option<&Value> {
        (gear == OagwGear::MODULE_NAME).then_some(&self.0)
    }
}

async fn build_router() -> Router {
    let gear = OagwGear::default();
    let ctx = GearCtx::new(
        OagwGear::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(FixedConfig(json!({
            "config": { "proxy_timeout_secs": 5, "allow_http_upstream": true }
        }))) as Arc<dyn ConfigProvider>,
        Arc::new(ClientHub::new()),
        Default::default(),
    );
    gear.init(&ctx)
        .await
        .expect("gear init must succeed with a well-formed fixed config");
    let openapi = OpenApiRegistryImpl::new();
    gear.register_rest(&ctx, Router::new(), &openapi)
        .expect("register_rest must succeed")
}

fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

fn json_request(method: &str, uri: &str, tenant_id: Uuid, body: Value) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

fn get_request_with_ip(uri: &str, tenant_id: Uuid, client_ip: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-forwarded-for", client_ip)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

async fn response_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Create an Upstream with a `scope: ip`, `sustained.rate: 1` rate limit
/// pointing at a mocked upstream, plus a matching `GET {path}` Route.
/// Returns the alias.
async fn create_rate_limited_upstream(
    router: &Router,
    tenant_id: Uuid,
    alias: &str,
    port: u16,
    path: &str,
) -> String {
    let body = json!({
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "rate_limit": {
            "sharing": "private",
            "sustained": { "rate": 1, "window": "second" },
            "scope": "ip",
            "strategy": "reject",
        },
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/upstreams", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "upstream creation must succeed"
    );
    let json = response_json(response).await;
    let upstream_id: Uuid = json["id"].as_str().unwrap().parse().unwrap();

    let route_body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
        "priority": 1,
    });
    let response = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/routes",
            tenant_id,
            route_body,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "route creation must succeed"
    );

    alias.to_owned()
}

/// RF-003 required test: two Upstreams under one tenant, each with a
/// `scope: ip` rate limit, must NOT share a bucket -- exhausting one
/// Upstream's budget for a client IP must not reject the other Upstream's
/// budget for that same IP. Also proves the "attach headers to an allowed
/// response" half of the contract: a successful response carries
/// `X-RateLimit-*`.
#[tokio::test]
async fn ip_scoped_rate_limits_do_not_bleed_across_upstreams() {
    let server_a = MockServer::start();
    let _m_a = server_a.mock(|when, then| {
        when.method(GET).path("/a");
        then.status(200).body("a-ok");
    });
    let server_b = MockServer::start();
    let _m_b = server_b.mock(|when, then| {
        when.method(GET).path("/b");
        then.status(200).body("b-ok");
    });

    let router = build_router().await;
    let tenant_id = Uuid::new_v4();
    let client_ip = "203.0.113.42";

    let alias_a =
        create_rate_limited_upstream(&router, tenant_id, "rl-svc-a", server_a.port(), "/a").await;
    let alias_b =
        create_rate_limited_upstream(&router, tenant_id, "rl-svc-b", server_b.port(), "/b").await;

    // First request to Upstream A succeeds and carries X-RateLimit-* on the
    // *allowed* response (RF-003's other required half of the contract).
    let response = router
        .clone()
        .oneshot(get_request_with_ip(
            &format!("/oagw/v1/proxy/{alias_a}/a"),
            tenant_id,
            client_ip,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-ratelimit-limit").is_some());
    assert!(response.headers().get("x-ratelimit-remaining").is_some());

    // Second request to the SAME Upstream A, same IP: budget of 1/sec is
    // exhausted, so this is rejected 429.
    let response = router
        .clone()
        .oneshot(get_request_with_ip(
            &format!("/oagw/v1/proxy/{alias_a}/a"),
            tenant_id,
            client_ip,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get(header::RETRY_AFTER).is_some());

    // Upstream B, same tenant, same client IP: must still succeed -- its
    // own independent bucket was never touched by Upstream A's exhaustion.
    let response = router
        .oneshot(get_request_with_ip(
            &format!("/oagw/v1/proxy/{alias_b}/b"),
            tenant_id,
            client_ip,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a different upstream's scope:ip budget must not be exhausted by upstream A's traffic"
    );
}
