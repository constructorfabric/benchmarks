//! The data plane: what arrives at the upstream, and what comes back.

mod common;

use common::*;
use oagw::config::OagwConfig;

/// A route with an explicit query allowlist.
fn allowlisted_route(upstream_id: &str, path: &str, allow: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                "path": path,
                "query_allowlist": allow
            }
        }
    })
}

/// Registers the mock as an upstream, routes it and returns the alias.
async fn wire(
    app: &mut App,
    mock: &MockUpstream,
    route: impl FnOnce(&str) -> serde_json::Value,
) -> String {
    let upstream = app
        .create_upstream(loopback_upstream("vendor", mock.port()))
        .await;
    app.create_route(route(&upstream)).await;
    "vendor".to_owned()
}

#[tokio::test]
async fn proxies_a_get_with_the_routed_prefix_and_the_query() {
    let mut app = app();
    let mock = echo_upstream().await;
    wire(&mut app, &mock, |id| allowlisted_route(id, "/v1", &["q"])).await;

    let response = app
        .send_raw("GET", &proxy_path("vendor", "v1/echo?q=1"), None, &[])
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&App::read(response).await)
    );
    let headers = response.headers().clone();
    let echo: serde_json::Value = serde_json::from_slice(&App::read(response).await).expect("json");

    assert_eq!(echo["method"], "GET");
    // `path_suffix_mode` defaults to `append`, so the suffix rides along.
    assert_eq!(echo["path"], "/v1/echo");
    assert_eq!(echo["query"], "q=1");
    // The gateway names the endpoint, not the client's Host.
    assert!(
        echo["host"]
            .as_str()
            .expect("host")
            .starts_with("127.0.0.1")
    );
    // ADR-0007: a successful response names the upstream as its source too.
    assert!(
        mock.last()
            .header("host")
            .is_some_and(|h| h.starts_with("127.0.0.1:"))
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
}

#[tokio::test]
async fn a_catch_all_route_serves_the_whole_alias() {
    let mut app = app();
    let mock = echo_upstream().await;
    let alias = wire(&mut app, &mock, |id| route_body(id, "/")).await;

    let response = app
        .send_raw("GET", &proxy_path(&alias, "any/depth/here"), None, &[])
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&App::read(response).await)
    );
    let echo: serde_json::Value = serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(echo["path"], "/any/depth/here");

    // The bare alias is a request for `/` and matches the same route.
    let response = app
        .send_raw("GET", &proxy_path(&alias, ""), None, &[])
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&App::read(response).await)
    );
    let echo: serde_json::Value = serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(echo["path"], "/");
}

#[tokio::test]
async fn forwards_a_post_body_and_content_type() {
    let mut app = app();
    let mock = echo_upstream().await;
    let mut upstream = loopback_upstream("vendor", mock.port());
    upstream["headers"] = serde_json::json!({"request": {"passthrough": "all"}});
    let id = app.create_upstream(upstream).await;
    app.create_route(route_body(&id, "/v1")).await;
    let alias = "vendor".to_owned();

    let response = app
        .send_raw(
            "POST",
            &proxy_path(&alias, "v1/things"),
            Some(serde_json::json!({"name": "widget"})),
            &[],
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let echo: serde_json::Value = serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["path"], "/v1/things");
    assert_eq!(echo["content_type"], "application/json");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(echo["body"].as_str().expect("body"))
            .expect("echoed body"),
        serde_json::json!({"name": "widget"})
    );
}

#[tokio::test]
async fn a_query_parameter_outside_the_allowlist_is_rejected_at_the_gateway() {
    let mut app = app();
    let mock = echo_upstream().await;
    let alias = wire(&mut app, &mock, |id| allowlisted_route(id, "/v1", &["q"])).await;

    let response = app
        .send_raw("GET", &proxy_path(&alias, "v1?extra=1"), None, &[])
        .await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let problem: serde_json::Value =
        serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(problem["type"], oagw::gts::errors::VALIDATION_ERROR);
    // The request never reached the upstream.
    assert!(mock.captured().is_empty(), "{:?}", mock.captured());
}

#[tokio::test]
async fn a_path_suffix_is_rejected_when_the_route_disables_it() {
    let mut app = app();
    let mock = echo_upstream().await;
    let id = app
        .create_upstream(loopback_upstream("vendor", mock.port()))
        .await;
    let alias = "vendor".to_owned();
    app.create_route(serde_json::json!({
        "upstream_id": id,
        "match": {"http": {"methods": ["GET"], "path": "/v1", "path_suffix_mode": "disabled"}}
    }))
    .await;

    // Exactly the route path goes through.
    let response = app
        .send_raw("GET", &proxy_path(&alias, "v1"), None, &[])
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);

    // A suffix does not.
    let response = app
        .send_raw("GET", &proxy_path(&alias, "v1/deep"), None, &[])
        .await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn an_unknown_alias_is_a_route_not_found_problem() {
    let mut app = app();
    let (status, problem) = app.send("GET", &proxy_path("nowhere", "v1"), None).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["type"], oagw::gts::errors::ROUTE_NOT_FOUND);
    assert_eq!(problem["alias"], "nowhere");
}

#[tokio::test]
async fn an_unmatched_path_on_a_known_upstream_is_a_404() {
    let mut app = app();
    let mock = echo_upstream().await;
    let alias = wire(&mut app, &mock, |id| route_body(id, "/v1")).await;

    let response = app
        .send_raw("GET", &proxy_path(&alias, "v2"), None, &[])
        .await;
    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_disabled_upstream_answers_503_link_unavailable() {
    let mut app = app();
    let mock = echo_upstream().await;
    let id = app
        .create_upstream(loopback_upstream("vendor", mock.port()))
        .await;
    app.create_route(route_body(&id, "/v1")).await;
    let alias = "vendor".to_owned();

    let mut disabled = loopback_upstream("vendor", mock.port());
    disabled["enabled"] = serde_json::json!(false);
    let (status, _) = app
        .send("PUT", &format!("/oagw/v1/upstreams/{id}"), Some(disabled))
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let response = app
        .send_raw("GET", &proxy_path(&alias, "v1"), None, &[])
        .await;
    assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let problem: serde_json::Value =
        serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(problem["type"], oagw::gts::errors::LINK_UNAVAILABLE);
}

#[tokio::test]
async fn upstream_failures_are_passed_through_with_the_upstream_error_source() {
    let mut app = app();
    let mock = MockUpstream::start(|_| MockResponse {
        status: 503,
        headers: vec![("content-type", "application/json")],
        body: MockBody::Bytes(r#"{"message":"overloaded"}"#),
    })
    .await;
    let alias = wire(&mut app, &mock, |id| route_body(id, "/v1")).await;

    let response = app
        .send_raw("GET", &proxy_path(&alias, "v1"), None, &[])
        .await;
    assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    let body = App::read(response).await;
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"message":"overloaded"}"#
    );
    assert!(!mock.captured().is_empty());
}

#[tokio::test]
async fn a_target_host_outside_the_endpoint_pool_is_rejected() {
    let mut app = app();
    let mock = echo_upstream().await;
    let alias = wire(&mut app, &mock, |id| route_body(id, "/v1")).await;

    let response = app
        .send_raw(
            "GET",
            &proxy_path(&alias, "v1"),
            None,
            &[("x-oagw-target-host", "not-in-the-pool.example")],
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let problem: serde_json::Value =
        serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(
        problem["type"],
        oagw::gts::errors::ROUTING_UNKNOWN_TARGET_HOST
    );
}

#[tokio::test]
async fn rate_limit_exhaustion_answers_429_with_the_rate_limit_error() {
    let mut app = app();
    let mock = echo_upstream().await;
    let id = app
        .create_upstream(loopback_upstream("vendor", mock.port()))
        .await;
    let alias = "vendor".to_owned();
    let mut route = route_body(&id, "/v1");
    route["rate_limit"] = serde_json::json!({
        "sustained": {"rate": 2, "window": "minute"},
        "burst": {"capacity": 2}
    });
    app.create_route(route).await;

    for expected in [
        http::StatusCode::OK,
        http::StatusCode::OK,
        http::StatusCode::TOO_MANY_REQUESTS,
    ] {
        let response = app
            .send_raw("GET", &proxy_path(&alias, "v1"), None, &[])
            .await;
        assert_eq!(
            response.status(),
            expected,
            "{}",
            String::from_utf8_lossy(&App::read(response).await)
        );
    }

    let problem: serde_json::Value = app.send("GET", &proxy_path(&alias, "v1"), None).await.1;
    assert_eq!(problem["type"], oagw::gts::errors::RATE_LIMIT_EXCEEDED);
}

#[tokio::test]
async fn cors_preflight_is_answered_without_touching_the_upstream() {
    let mut app = app();
    let mock = echo_upstream().await;
    let alias = wire(&mut app, &mock, |id| route_body(id, "/v1")).await;

    let response = app
        .send_raw(
            "OPTIONS",
            &proxy_path(&alias, "v1"),
            None,
            &[
                ("origin", "https://console.example.com"),
                ("access-control-request-method", "GET"),
                ("access-control-request-headers", "content-type"),
            ],
        )
        .await;
    // ADR-0004: a permissive 204, echoed back by the gateway alone.
    assert_eq!(
        response.status(),
        http::StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&App::read(response).await)
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://console.example.com")
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("GET")
    );
    assert!(mock.captured().is_empty(), "{:?}", mock.captured());
}

#[tokio::test]
async fn an_origin_outside_the_allowlist_is_rejected() {
    let mut app = app();
    let mock = echo_upstream().await;
    let id = app
        .create_upstream(loopback_upstream("vendor", mock.port()))
        .await;
    let alias = "vendor".to_owned();
    let mut route = route_body(&id, "/v1");
    route["cors"] = serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://console.example.com"]
    });
    app.create_route(route).await;

    let response = app
        .send_raw(
            "GET",
            &proxy_path(&alias, "v1"),
            None,
            &[("origin", "https://evil.example.com")],
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::FORBIDDEN);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_request_body_over_the_limit_is_rejected() {
    let mut app = App::new(
        MockResolver::default_hierarchy(),
        context_for(TENANT_L1A),
        OagwConfig {
            max_request_body_bytes: 8,
            ..test_config()
        },
    );
    let mock = echo_upstream().await;
    let alias = wire(&mut app, &mock, |id| route_body(id, "/v1")).await;

    let response = app
        .send_raw(
            "POST",
            &proxy_path(&alias, "v1"),
            Some(serde_json::json!({"payload": "0123456789".repeat(4)})),
            &[],
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
    let problem: serde_json::Value =
        serde_json::from_slice(&App::read(response).await).expect("json");
    assert_eq!(problem["type"], oagw::gts::errors::PAYLOAD_TOO_LARGE);
}
