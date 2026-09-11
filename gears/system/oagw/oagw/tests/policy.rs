//! Integration tests for the policy layer: auth plugins, guards, rate
//! limiting and CORS.

mod common;

use common::{
    base_config, build_router, build_router_with_cred_store, create, empty_request, request, send,
};
use credstore_sdk::test_util::MockCredStoreClient;
use http::{Method, StatusCode};
use httpmock::MockServer;
use oagw::domain::model::PROTOCOL_HTTP;
use serde_json::json;
use std::sync::Arc;

fn upstream_body(alias: &str, port: u16) -> serde_json::Value {
    json!({
        "alias": alias,
        "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
        "protocol": PROTOCOL_HTTP,
    })
}

fn route_body(upstream_id: &serde_json::Value, path: &str) -> serde_json::Value {
    json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": path}},
    })
}

#[tokio::test]
async fn the_apikey_plugin_injects_its_header_and_the_secret_never_leaks_into_the_response() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/call")
            .header("x-api-key", "sk-supersecret");
        then.status(200).body("ok-no-secret-here");
    });

    let cred_store = MockCredStoreClient::with_secrets(vec![(
        "my-secret".to_owned(),
        "sk-supersecret".to_owned(),
    )]);
    let (router, _state) = build_router_with_cred_store(base_config(), Some(Arc::new(cred_store)));

    let mut body = upstream_body("svc", server.port());
    body["auth"] = json!({
        "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        "config": {"header": "x-api-key", "secret_ref": "cred://my-secret"},
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &body).await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/call"),
    )
    .await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc/call"),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "the mock only matches when the header carries the resolved secret: {}",
        resp.text()
    );
    assert_eq!(mock.calls(), 1);
    assert!(
        !resp.text().contains("sk-supersecret"),
        "the secret value must never appear in the response body: {}",
        resp.text()
    );
}

#[tokio::test]
async fn unknown_and_catalogue_only_auth_plugins_fail_with_503_plugin_not_found() {
    let server = MockServer::start();
    let (router, _state) = build_router(base_config());

    for (case, instance) in [
        ("unknown identifier", "cf.core.oagw.no-such-plugin.v1"),
        ("catalogue-only basic.v1", "cf.core.oagw.basic.v1"),
        ("catalogue-only bearer.v1", "cf.core.oagw.bearer.v1"),
    ] {
        let alias = format!("svc-{}", instance.replace(['.', '_'], "-"));
        let mut body = upstream_body(&alias, server.port());
        body["auth"] = json!({
            "type": format!("gts.cf.core.oagw.auth_plugin.v1~{instance}"),
            "config": {"value": "irrelevant"},
        });
        let upstream = create(&router, "/oagw/v1/upstreams", &body).await;
        create(
            &router,
            "/oagw/v1/routes",
            &route_body(&upstream["uuid"], "/call"),
        )
        .await;

        let resp = send(
            &router,
            empty_request(Method::GET, &format!("/oagw/v1/proxy/{alias}/call")),
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "case `{case}`: {}",
            resp.text()
        );
        let problem_type = resp.json()["type"]
            .as_str()
            .expect("type present")
            .to_owned();
        assert!(
            problem_type.ends_with("plugin.not_found.v1"),
            "case `{case}`: unexpected problem type {problem_type}"
        );
    }
}

#[tokio::test]
async fn the_required_headers_guard_is_400_on_request_and_502_on_response() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/guarded");
        then.status(200).header("x-other", "1").body("body");
    });

    let (router, _state) = build_router(base_config());
    let mut body = upstream_body("svc-guard", server.port());
    // The required-headers guard reads its configuration from the upstream's
    // `auth.config` map even though it is bound as a guard, not an auth
    // plugin — see the final report.
    body["auth"] = json!({
        "config": {
            "required_request_headers": "x-needed",
            "required_response_headers": "x-resp-needed",
        },
    });
    body["plugins"] = json!({
        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &body).await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/guarded"),
    )
    .await;

    let missing_request_header = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-guard/guarded"),
    )
    .await;
    assert_eq!(
        missing_request_header.status,
        StatusCode::BAD_REQUEST,
        "{}",
        missing_request_header.text()
    );
    assert_eq!(
        missing_request_header.json()["context"]["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(
        mock.calls(),
        0,
        "the guard must reject before the upstream is ever called"
    );

    let req = request(Method::GET, "/oagw/v1/proxy/svc-guard/guarded")
        .header("x-needed", "1")
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let missing_response_header = send(&router, req).await;
    assert_eq!(
        missing_response_header.status,
        StatusCode::BAD_GATEWAY,
        "{}",
        missing_response_header.text()
    );
    assert_eq!(
        missing_response_header.json()["context"]["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_blank_after_trim_guard_configuration_is_a_no_op() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/noop-guard");
        then.status(200).body("passed-through");
    });

    let (router, _state) = build_router(base_config());
    let mut body = upstream_body("svc-noop-guard", server.port());
    body["auth"] = json!({"config": {"required_request_headers": " , , "}});
    body["plugins"] = json!({
        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &body).await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/noop-guard"),
    )
    .await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-noop-guard/noop-guard"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.text(), "passed-through");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn exceeding_the_rate_limit_returns_429_with_the_expected_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/limited");
        then.status(200).body("ok");
    });

    let (router, _state) = build_router(base_config());
    let mut body = upstream_body("svc-limited", server.port());
    body["rate_limit"] = json!({
        "sustained": {"rate": 1, "window": "minute"},
        "burst": {"capacity": 1},
        "scope": "tenant",
        "strategy": "reject",
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &body).await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/limited"),
    )
    .await;

    let first = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-limited/limited"),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());

    let second = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-limited/limited"),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        second.text()
    );
    assert!(
        second.header("retry-after").is_some(),
        "retry-after header must be present"
    );
    assert!(second.header("x-ratelimit-limit").is_some());
    assert!(second.header("x-ratelimit-remaining").is_some());
    assert!(second.header("x-ratelimit-reset").is_some());
}

// DEFECT: `register_proxy_methods` in `src/api/rest/routes.rs` only wires
// get/post/put/patch/delete onto `/oagw/v1/proxy/{alias}` and
// `/oagw/v1/proxy/{alias}/{*rest}` — `OPTIONS` is never registered. That
// makes the preflight-handling branch in `forward()`
// (`src/api/rest/handlers/proxy.rs`, `is_preflight`/`preflight_response`)
// unreachable dead code: axum answers an OPTIONS request to a matched path
// with its own 405 Method Not Allowed before the handler ever runs, so a real
#[tokio::test]
async fn a_cors_preflight_succeeds_without_a_configured_upstream() {
    let (router, _state) = build_router(base_config());
    let req = request(Method::OPTIONS, "/oagw/v1/proxy/no-such-upstream/anything")
        .header("origin", "https://example.test")
        .header("access-control-request-method", "GET")
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let resp = send(&router, req).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("https://example.test")
    );
    assert_eq!(resp.header("access-control-max-age"), Some("86400"));
}

#[tokio::test]
async fn a_disallowed_origin_on_an_actual_request_is_rejected() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/cors");
        then.status(200).body("should-not-be-reached");
    });

    let (router, _state) = build_router(base_config());
    let mut body = upstream_body("svc-cors", server.port());
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["https://allowed.test"],
        "allowed_methods": ["GET"],
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &body).await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/cors"),
    )
    .await;

    // Not a preflight: only Origin is present, no
    // Access-Control-Request-Method, so this is an actual cross-origin GET.
    let req = request(Method::GET, "/oagw/v1/proxy/svc-cors/cors")
        .header("origin", "https://evil.test")
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let resp = send(&router, req).await;
    // `check_cors_actual` (src/api/rest/handlers/proxy.rs) maps a disallowed
    // origin onto `ErrorKind::ValidationError`, which the catalogue always
    // renders as 400 — not 403 — regardless of the `cors.origin_not_allowed`
    // context tag. See the final report.
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    assert_eq!(
        resp.json()["context"]["error_code"],
        json!("cors.origin_not_allowed")
    );
    assert_eq!(
        mock.calls(),
        0,
        "a disallowed origin must be rejected before forwarding"
    );
}
