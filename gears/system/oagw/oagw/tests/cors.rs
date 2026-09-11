//! CORS is answered where the gateway owns the request.
//!
//! A browser preflight must be answered immediately, without waking an
//! upstream that may be asleep, and the actual request that follows is the one
//! that gets checked against the operator's origins and methods. These tests
//! count how often the upstream answered: a preflight is the gateway's own
//! business, and a refused cross-origin call must never leave the building.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use http_body_util::BodyExt;
use serde_json::json;
use toolkit_security::SecurityContext;

/// The proxy path for `alias` on the wired `/v1/thing` route.
fn proxy_path(alias: &str) -> String {
    format!("/oagw/v1/proxy/{alias}/v1/thing")
}

const APP_ORIGIN: &str = "https://app.example.com";
const ADMIN_ORIGIN: &str = "https://admin.example.com";
const EVIL_ORIGIN: &str = "https://evil.example.org";

/// A route on `/v1/thing` with an explicit CORS configuration.
async fn wired(
    app: &common::TestApp,
    upstream: &LocalUpstream,
    cors: serde_json::Value,
    methods: &[&str],
) -> String {
    let spec = upstream.upstream_spec("cross.origin");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let methods: Vec<String> = methods.iter().map(|m| (*m).to_owned()).collect();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": methods,
        "target_alias": alias,
        "strip_prefix": false,
        "cors": cors
    }))
    .await;
    alias
}

/// A CORS configuration allowing `APP_ORIGIN`, credentials off.
fn cors_config(methods: &[&str]) -> serde_json::Value {
    json!({
        "allow_origins": [APP_ORIGIN, ADMIN_ORIGIN],
        "allow_methods": methods,
        "allow_headers": ["content-type", "authorization"],
        "allow_credentials": false,
        "max_age_secs": 600
    })
}

/// An `OPTIONS` preflight for `origin` wanting `method`.
fn preflight(
    app: &common::TestApp,
    alias: &str,
    origin: &str,
    method: &str,
    headers: Option<&str>,
) -> http::Request<axum::body::Body> {
    let mut named: Vec<(String, String)> = vec![
        ("origin".to_owned(), origin.to_owned()),
        (
            "access-control-request-method".to_owned(),
            method.to_owned(),
        ),
    ];
    if let Some(requested) = headers {
        named.push((
            "access-control-request-headers".to_owned(),
            requested.to_owned(),
        ));
    }
    let named: Vec<(&str, &str)> = named
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    app.request(http::Method::OPTIONS, &proxy_path(alias), None, &named)
}

/// A cross-origin `GET` for `origin`.
fn actual(
    app: &common::TestApp,
    alias: &str,
    origin: &str,
    method: &http::Method,
) -> http::Request<axum::body::Body> {
    app.request(
        method.clone(),
        &proxy_path(alias),
        None,
        &[("origin", origin)],
    )
}

#[tokio::test]
async fn a_preflight_is_answered_without_touching_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        cors_config(&["GET", "POST"]),
        &["GET", "OPTIONS"],
    )
    .await;

    let response = app
        .send(preflight(
            &app,
            &alias,
            APP_ORIGIN,
            "POST",
            Some("content-type"),
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::NO_CONTENT,
        "{response:?}"
    );
    let headers = response.headers();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(APP_ORIGIN),
        "the requested origin is echoed"
    );
    assert!(
        headers
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|methods| methods.contains("POST")),
        "the requested method is echoed"
    );
    assert!(
        headers
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|allowed| allowed.contains("content-type")),
        "the requested headers are echoed"
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|v| v.to_str().ok()),
        Some("600")
    );
    assert!(
        headers
            .get("vary")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|vary| vary.to_ascii_lowercase().contains("origin")),
        "the answer varies on Origin: {headers:?}"
    );
    assert_eq!(
        upstream.count(),
        0,
        "no upstream round-trip for a preflight"
    );
}

/// A preflight is not a lease on the route: the actual request is checked.
#[tokio::test]
async fn an_actual_request_from_a_disallowed_origin_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, cors_config(&["GET"]), &["GET", "OPTIONS"]).await;

    let response = app
        .send(actual(&app, &alias, EVIL_ORIGIN, &http::Method::GET))
        .await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(common::error_source_header())
            .and_then(|v| v.to_str().ok()),
        Some(common::error_source_gateway()),
        "the refusal is the gateway's own"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("cors.origin_not_allowed.v1")),
        "{document}"
    );
    assert_eq!(
        upstream.count(),
        0,
        "a disallowed origin never reaches the upstream"
    );
}

#[tokio::test]
async fn an_actual_request_from_a_disallowed_method_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    // The route itself carries DELETE; it is the CORS configuration that
    // refuses it, which is why the answer is a 400 and not a 404.
    let mut cors = cors_config(&["GET"]);
    cors["allow_methods"] = json!(["GET"]);
    let spec = upstream.upstream_spec("cross.method");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": ["GET", "OPTIONS", "DELETE"],
        "target_alias": alias,
        "strip_prefix": false,
        "cors": cors
    }))
    .await;

    let response = app
        .send(actual(&app, &alias, APP_ORIGIN, &http::Method::DELETE))
        .await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("cors.method_not_allowed.v1")),
        "{document}"
    );
    assert_eq!(upstream.count(), 0, "a refused method never travels");
}

#[tokio::test]
async fn an_allowed_origin_gets_the_cors_headers_on_the_answer() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, cors_config(&["GET"]), &["GET", "OPTIONS"]).await;

    let response = app
        .send(actual(&app, &alias, APP_ORIGIN, &http::Method::GET))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(APP_ORIGIN)
    );
    assert_eq!(upstream.count(), 1, "the request did travel");
}

/// A same-origin request — one the browser will not preflight — is untouched.
#[tokio::test]
async fn a_request_without_an_origin_is_not_cors_checked() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, cors_config(&["GET"]), &["GET", "OPTIONS"]).await;

    let response = app
        .send(app.request(http::Method::GET, &proxy_path(&alias), None, &[]))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "no Origin header means no CORS rules apply"
    );
}

/// `allow_credentials` answers with the credentials header, never with a wildcard.
#[tokio::test]
async fn credentials_are_advertised_only_with_explicit_origins() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let mut cors = cors_config(&["GET"]);
    cors["allow_credentials"] = json!(true);
    let alias = wired(&app, &upstream, cors, &["GET", "OPTIONS"]).await;

    let response = app
        .send(actual(&app, &alias, APP_ORIGIN, &http::Method::GET))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
}

#[tokio::test]
async fn credentials_with_a_wildcard_origin_are_refused_at_create_time() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("wildcard");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/thing",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "cors": {
                "allow_origins": ["*"],
                "allow_methods": ["GET"],
                "allow_credentials": true
            }
        }))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("wildcard")),
        "{document}"
    );
}

/// A route without CORS does not answer preflights or check origins.
#[tokio::test]
async fn a_route_without_cors_lets_the_upstream_answer() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("no.cors");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": ["GET", "OPTIONS"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let response = app
        .send(actual(&app, &alias, EVIL_ORIGIN, &http::Method::GET))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "no cors configuration means no origin enforcement"
    );
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "the gateway invents no allow-origin"
    );
}

/// The second configured origin is honoured too, not just the first.
#[tokio::test]
async fn every_configured_origin_is_allowed() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, cors_config(&["GET"]), &["GET", "OPTIONS"]).await;

    for origin in [APP_ORIGIN, ADMIN_ORIGIN] {
        let response = app
            .send(actual(&app, &alias, origin, &http::Method::GET))
            .await;
        assert_eq!(response.status(), http::StatusCode::OK, "{origin}");
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(origin)
        );
    }
}

#[tokio::test]
async fn the_preflight_answer_is_bodyless() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, cors_config(&["GET"]), &["GET", "OPTIONS"]).await;

    let response = app
        .send(preflight(&app, &alias, APP_ORIGIN, "GET", None))
        .await;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert!(
        bytes.is_empty(),
        "a preflight carries no body: {}",
        String::from_utf8_lossy(&bytes)
    );
}

/// A preflight is answered with no credential presented at all.
///
/// A browser sends none, so a preflight that had to be authenticated would
/// strand every cross-origin client at `401` before the gateway could say
/// anything about origins (ADR 0004: no tenant context, no upstream
/// resolution).
#[tokio::test]
async fn a_preflight_needs_no_credential() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": ["GET", "OPTIONS"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    // The barest preflight there is: no subject extension, no bearer header.
    let request = common::request_with_subject(
        SecurityContext::anonymous(),
        http::Method::OPTIONS,
        &proxy_path(&alias),
        None,
        &[
            ("origin", EVIL_ORIGIN),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ],
    );
    let response = app.send(request).await;
    assert_eq!(
        response.status(),
        http::StatusCode::NO_CONTENT,
        "a preflight is answered permissively without a tenant"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some(EVIL_ORIGIN),
        "the requested origin is echoed back"
    );
    assert_eq!(
        upstream.count(),
        0,
        "no upstream round-trip for an unauthenticated preflight"
    );
}

/// The same preflight against an alias nobody configured still gets the
/// permissive answer, because resolving a route needs the tenant a preflight
/// does not have.
#[tokio::test]
async fn a_preflight_for_an_unknown_alias_is_answered_all_the_same() {
    let app = app().await;

    let request = common::request_with_subject(
        SecurityContext::anonymous(),
        http::Method::OPTIONS,
        &proxy_path("never-configured"),
        None,
        &[
            ("origin", APP_ORIGIN),
            ("access-control-request-method", "GET"),
        ],
    );
    let response = app.send(request).await;
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
}
