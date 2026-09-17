//! Data-plane guards, rate limiting, CORS and credential injection (DESIGN
//! "Guard Rules", ADR 0003, ADR 0004, ADR 0008).
//!
//! Every guard is exercised through the wire: a real upstream answers, so the
//! assertion is always about the response the client sees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::common::{APIKEY_PLUGIN, Harness, catch_all_route, create_upstream};

const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
const RATE_LIMITED: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
const CORS_ORIGIN: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
const METHOD_NOT_ALLOWED: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.method_not_allowed.v1";

/// Create an upstream + catch-all route, returning the alias.
async fn setup(h: &Harness, upstream: Value, alias: &str) -> String {
    let (status, body) = create_upstream(h, upstream).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let upstream_id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = common::create_route(h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    alias.to_owned()
}

/// An upstream answering on `port` under `alias`, with `extra` merged in.
fn upstream_at(host: &str, port: u16, alias: &str, mut extra: Value) -> Value {
    let mut body = common::http_upstream(host, port, Some(alias));
    if let (Some(extra_object), Some(body_object)) = (extra.as_object_mut(), body.as_object_mut()) {
        for (key, value) in extra_object {
            body_object.insert(key.clone(), value.clone());
        }
    }
    body
}

// ------------------------------------------------------------------- guards

#[tokio::test]
async fn a_method_the_route_does_not_serve_is_a_405() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, upstream_at(&host, port, "methods", json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    // The route serves GET only; the *router* serves POST too, so the request
    // reaches the pipeline and the route matcher has to refuse it.
    let (status, route) = common::create_route(
        &h,
        json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET"], "path": "/",
                               "path_suffix_mode": "append"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");

    let (status, problem, _) = h
        .json(Method::POST, "/oagw/v1/proxy/methods/things", &[], None)
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{problem}");
    assert_eq!(problem["type"], METHOD_NOT_ALLOWED);
    assert_eq!(problem["status"], json!(405));
    // RFC 9110 §10.2.2: the served methods are named in the problem document.
    let allow = problem["alias"].as_str().unwrap_or_default();
    assert_eq!(allow, "GET", "the served methods travel with the problem");
}

#[tokio::test]
async fn an_oversized_body_is_a_413() {
    let (host, port) = common::echo_server().await;
    let h = common::harness_with(common::harness_with_small_body());
    let alias = setup(&h, upstream_at(&host, port, "small", json!({})), "small").await;

    // The declared body exceeds `body_limit_bytes`, so the guard refuses it
    // before anything is dialled.
    let (status, problem, _) = h
        .json(
            Method::POST,
            &format!("/oagw/v1/proxy/{alias}/upload"),
            &[("content-length", "999999")],
            Some(json!({"blob": "x".repeat(2048)})),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
    assert_eq!(problem["status"], json!(413));
}

#[tokio::test]
async fn a_transfer_encoding_other_than_chunked_is_refused() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let alias = setup(&h, upstream_at(&host, port, "framed", json!({})), "framed").await;

    // `raw` lets the test control the framing headers exactly.
    let (status, body, _) = h
        .raw(
            Method::POST,
            &format!("/oagw/v1/proxy/{alias}/upload"),
            &[("transfer-encoding", "identity"), ("content-length", "2")],
            b"{}".to_vec(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let problem: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    assert_eq!(problem["type"], VALIDATION);
}

#[tokio::test]
async fn an_unknown_query_parameter_is_refused_by_the_allowlist() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, upstream_at(&host, port, "queried", json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, route) = common::create_route(
        &h,
        json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET", "POST"], "path": "/",
                               "path_suffix_mode": "append",
                               "query_allowlist": ["model"]}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");

    // `model` is allowed…
    let (status, _, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/queried/v1?model=gpt",
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // …`temperature` is not.
    let (status, problem, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/queried/v1?temperature=0.7",
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], VALIDATION);
}

// --------------------------------------------------------------- rate limits

/// A rate limit of one request per hour with a burst of one: the second request
/// is refused deterministically, with no wall-clock waiting.
fn hourly_burst_of_one() -> Value {
    common::token_bucket(1, 1, "tenant")
        .as_object_mut()
        .map(|object| {
            object.insert("sustained".to_owned(), json!({"rate": 1, "window": "hour"}));
            json!(object)
        })
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_rate_limit_beyond_the_burst_is_a_429() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    setup(
        &h,
        upstream_at(
            &host,
            port,
            "capped",
            json!({"rate_limit": hourly_burst_of_one()}),
        ),
        "capped",
    )
    .await;

    let (status, _, _) = h
        .json(Method::GET, "/oagw/v1/proxy/capped/ping", &[], None)
        .await;
    assert_eq!(status, StatusCode::OK, "the first request is allowed");

    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/capped/ping", &[], None)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{problem}");
    assert_eq!(problem["type"], RATE_LIMITED);
    assert_eq!(problem["status"], json!(429));
    let retry = problem["retry_after_seconds"]
        .as_u64()
        .expect("retry_after_seconds");
    assert!(
        retry >= 1,
        "retry_after_seconds must be positive: {problem}"
    );
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some(retry.to_string().as_str()),
        "the header and the problem member agree"
    );
}

#[tokio::test]
async fn a_rate_limit_is_scoped_per_upstream() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let limited = setup(
        &h,
        upstream_at(
            &host,
            port,
            "limited",
            json!({"rate_limit": hourly_burst_of_one()}),
        ),
        "limited",
    )
    .await;
    let other = setup(&h, upstream_at(&host, port, "open", json!({})), "open").await;

    let (status, _, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{limited}/ping"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{limited}/ping"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A different upstream has its own allowance.
    let (status, _, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{other}/ping"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "another upstream is not throttled");
}

// ---------------------------------------------------------------------- CORS

#[tokio::test]
async fn a_preflight_is_answered_without_reaching_the_upstream() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "cors",
            json!({"cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET", "POST"],
                "allow_credentials": true
            }}),
        ),
        "cors",
    )
    .await;

    let (status, _, headers) = h
        .json(
            Method::OPTIONS,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "content-type,x-request-id",
                ),
            ],
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a preflight is answered locally"
    );
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|v| v.to_str().ok()),
        Some("86400")
    );
}

#[tokio::test]
async fn a_cross_origin_request_from_a_disallowed_origin_is_refused() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "strict",
            json!({"cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET", "POST"]
            }}),
        ),
        "strict",
    )
    .await;

    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[("origin", "https://evil.example.net")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert_eq!(problem["type"], CORS_ORIGIN);
    assert_eq!(problem["status"], json!(403));

    // The allowed origin is proxied and answered with CORS headers.
    let (status, _, headers) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[("origin", "https://app.example.com")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
}

#[tokio::test]
async fn a_cross_origin_method_outside_the_allowlist_is_refused() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "methodless",
            json!({"cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET"]
            }}),
        ),
        "methodless",
    )
    .await;

    let (status, problem, _) = h
        .json(
            Method::DELETE,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[("origin", "https://app.example.com")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

// -------------------------------------------------------------------- plugins

#[tokio::test]
async fn a_guard_plugin_can_reject_a_request() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();

    // ADR 0009: the built-in required-headers guard, bound with its config.
    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "guarded",
            json!({"plugins": {
                "sharing": "private",
                "items": [{
                    "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                    "config": {"required_request_headers": "x-correlation-id"}
                }]
            }}),
        ),
        "guarded",
    )
    .await;

    // Without the required header the guard refuses the request.
    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], VALIDATION);

    // With it, the request is proxied.
    let (status, _, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[("x-correlation-id", "abc")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// Create a `GET` route on `upstream_id` with `extra` merged into the route body.
async fn route_with(h: &Harness, upstream_id: &str, path: &str, extra: Value) {
    let mut route = json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": path,
                           "path_suffix_mode": "append"}}
    });
    if let (Some(extra), Some(target)) = (extra.as_object(), route.as_object_mut()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    let (status, body) = common::create_route(h, route).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// A route-level guard is scoped to the route that binds it: the sibling route
/// of the same upstream runs without it.
#[tokio::test]
async fn a_route_level_guard_applies_only_to_its_own_route() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, upstream_at(&host, port, "scoped", json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    // ADR 0002: a route may bind its own guard chain. Only `/guarded` carries it.
    route_with(
        &h,
        &upstream_id,
        "/guarded",
        json!({"plugins": {"sharing": "private", "items": [{
            "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
            "config": {"required_request_headers": "x-correlation-id"}
        }]}}),
    )
    .await;
    route_with(&h, &upstream_id, "/open", json!({})).await;

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/scoped/guarded/v1", &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], VALIDATION);

    let (status, _, _) = h
        .json(Method::GET, "/oagw/v1/proxy/scoped/open/v1", &[], None)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the upstream-level chain is empty, so the other route is not guarded"
    );
}

/// A route-level binding for a reference the upstream also binds replaces the
/// upstream-level one (route level wins on conflict).
#[tokio::test]
async fn a_route_level_plugin_overrides_the_same_upstream_level_plugin() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) = create_upstream(
        &h,
        upstream_at(
            &host,
            port,
            "override",
            json!({"plugins": {"sharing": "private", "items": [{
                "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                "config": {"required_request_headers": "x-upstream-only"}
            }]}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    route_with(
        &h,
        &upstream_id,
        "/",
        json!({"plugins": {"sharing": "private", "items": [{
            "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
            "config": {"required_request_headers": "x-route-only"}
        }]}}),
    )
    .await;

    // The route's requirement is in force …
    let (status, problem, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/override/v1",
            &[("x-upstream-only", "1")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    // …and the upstream-level requirement is not: the same plugin ran once, with
    // the route's configuration.
    let (status, _, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/override/v1",
            &[("x-route-only", "1")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the upstream rule was overridden");
}

/// Upstream-level and route-level chains compose: both run, upstream first
/// (DESIGN "Plugin System", ADR 0002 "Execution Order").
#[tokio::test]
async fn upstream_level_and_route_level_plugins_both_execute() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) = create_upstream(
        &h,
        upstream_at(
            &host,
            port,
            "stacked",
            json!({"plugins": {"sharing": "private",
                               "items": [common::REQUEST_ID_PLUGIN]}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    route_with(
        &h,
        &upstream_id,
        "/",
        json!({"plugins": {"sharing": "private", "items": [{
            "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
            "config": {"required_request_headers": "x-correlation-id"}
        }]}}),
    )
    .await;

    // The route-level guard rejects …
    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/stacked/v1/chat", &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], VALIDATION);

    // …and once it allows, the upstream-level transform has run too.
    let (status, echoed, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/stacked/v1/chat",
            &[("x-correlation-id", "abc")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    let headers = echoed["headers"].as_object().expect("echoed headers");
    assert!(
        headers.contains_key("x-request-id"),
        "the upstream-level transform ran after the route-level guard: {headers:?}"
    );
}

#[tokio::test]
async fn a_transform_plugin_adds_a_request_id_the_upstream_can_see() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();

    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "identified",
            json!({"plugins": {
                "sharing": "private",
                "items": [common::REQUEST_ID_PLUGIN]
            }}),
        ),
        "identified",
    )
    .await;

    let (_, echoed) = common::post_json(&h, &alias, "/v1/chat", &[], json!({})).await;
    let headers = echoed["headers"].as_object().expect("echoed headers");
    assert!(
        headers.contains_key("x-request-id"),
        "the transform plugin injects an id the upstream can see: {headers:?}"
    );
}

#[tokio::test]
async fn an_api_key_is_injected_into_the_proxied_request() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    h.secrets.insert("openai-key", b"sk-test-e2e-fake-key");

    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "keyed",
            json!({"auth": {
                "type": APIKEY_PLUGIN,
                "config": {"key_ref": "openai-key"}
            }}),
        ),
        "keyed",
    )
    .await;

    let (status, echoed) = common::post_json(&h, &alias, "/v1/chat", &[], json!({})).await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    assert_eq!(
        echoed["headers"]["x-api-key"], "sk-test-e2e-fake-key",
        "the credential plugin injects the key upstream"
    );
    // The gateway never forwards the client's own credentials alongside it.
    let headers = echoed["headers"].as_object().unwrap();
    assert!(!headers.contains_key("authorization"), "{headers:?}");
}

#[tokio::test]
async fn a_missing_credential_never_proxies_an_unauthenticated_request() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();

    let alias = setup(
        &h,
        upstream_at(
            &host,
            port,
            "keyless",
            json!({"auth": {
                "type": APIKEY_PLUGIN,
                "config": {"key_ref": "not-there"}
            }}),
        ),
        "keyless",
    )
    .await;

    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/chat"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
}
