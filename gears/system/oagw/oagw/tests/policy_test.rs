//! Policy integration tests: rate limiting, credential injection and CORS,
//! each configured through the management API and observed on the proxy.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::Harness;
use httpmock::{Mock, MockServer};
use serde_json::json;

/// Seeds an upstream with a POST route, returning its identifier.
async fn seeded(harness: &Harness, server: &MockServer) -> String {
    let upstream = harness.seed_upstream("api.example.com", server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;
    id
}

/// A mock that answers the chat route with `200`.
fn echo(server: &MockServer) -> Mock<'_> {
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/chat");
        then.status(200).header("content-type", "application/json");
    })
}

#[tokio::test]
async fn a_rate_limited_request_carries_the_bucket_headers() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let id = seeded(&harness, &server).await;

    let (status, _) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "rate_limit": {
                    "sustained": {"rate": 2, "window": "second"},
                    "burst": {"capacity": 2}
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let answer = echo(&server);
    let (first, _, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(first, StatusCode::OK, "the first call is inside the bucket");
    assert_eq!(answer.calls(), 1);
    let limit = headers
        .get("x-ratelimit-limit")
        .map(|value| value.to_str().unwrap_or_default().to_owned());
    assert_eq!(limit.as_deref(), Some("2"), "the bucket size is advertised");

    // The second call exhausts the bucket; the third is refused.
    let (second, _, _) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(second, StatusCode::OK, "{second}");
    let (third, problem, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(third, StatusCode::TOO_MANY_REQUESTS, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("rate_limit.exceeded"))
    );
    let retry_after = headers
        .get("retry-after")
        .map(|value| value.to_str().unwrap_or_default().to_owned());
    let retry = retry_after
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default();
    assert!(
        retry >= 1,
        "the client is told when to come back: {retry_after:?}"
    );
    assert_eq!(answer.calls(), 2, "the refused call never left the gateway");
}

#[tokio::test]
async fn an_api_key_credential_is_injected_from_the_store() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let id = seeded(&harness, &server).await;

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "auth": {
                    "type": oagw::gts_helpers::AUTH_APIKEY,
                    "config": {
                        "header_name": "authorization",
                        "secret_ref": "cred://acme/openai-key"
                    }
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let answer = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/chat")
            .header("authorization", "sk-secret-value");
        then.status(200);
    });
    let refused = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/chat");
        then.status(500);
    });

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{problem}");
    assert_eq!(answer.calls(), 1, "the store's secret reached the upstream");
    assert_eq!(refused.calls(), 0, "no other credential was sent");
}

#[tokio::test]
async fn a_missing_credential_is_a_gateway_secret_not_found() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let id = seeded(&harness, &server).await;

    let (status, _) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "auth": {
                    "type": oagw::gts_helpers::AUTH_APIKEY,
                    "config": {
                        "header_name": "authorization",
                        "secret_ref": "cred://acme/absent-key"
                    }
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let answer = echo(&server);
    let (status, problem, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("secret.not_found"))
    );
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        !detail.contains("sk-secret-value"),
        "no stored secret value is echoed back: {detail}"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("gateway")
    );
    assert_eq!(answer.calls(), 0, "an unauthenticated call is not proxied");
}

#[tokio::test]
async fn a_required_header_guard_rejects_a_request_without_it() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let id = seeded(&harness, &server).await;

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "plugins": {"items": [{
                    "plugin_ref": oagw::gts_helpers::GUARD_REQUIRED_HEADERS,
                    "config": {"required_request_headers": "x-trace-id"}
                }]},
                "headers": {"request": {
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["x-trace-id"]
                }}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let answer = echo(&server);
    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(answer.calls(), 0, "a rejected request is not proxied");

    let (status, problem, _) = harness
        .send_raw(common::raw_request(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            &[("x-trace-id", "trace-1")],
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{problem}");
    assert_eq!(answer.calls(), 1, "a compliant request is proxied");
}

#[tokio::test]
async fn cors_preflight_is_answered_by_the_gateway() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let id = seeded(&harness, &server).await;

    let (status, _) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "cors": {
                    "enabled": true,
                    "allowed_origins": ["https://console.example.com"],
                    "allowed_methods": ["POST"]
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let preflight = |origin: &str| {
        common::raw_request(
            "OPTIONS",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            &[
                ("origin", origin),
                ("access-control-request-method", "POST"),
            ],
            None,
        )
    };

    let (status, _, headers) = harness
        .send_raw(preflight("https://console.example.com"))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "the gateway answers itself");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("https://console.example.com"),
        "a preflight from the configured origin is allowed"
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("POST"),
        "the configured method is advertised"
    );

    // A preflight from an origin the configuration does not name is answered
    // without an allow-origin header, so the browser refuses to send the
    // actual request.
    let (_, _, headers) = harness
        .send_raw(preflight("https://stranger.example"))
        .await;
    assert!(headers.get("access-control-allow-origin").is_none());
}
