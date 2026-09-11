//! Error-contract integration tests: every condition the gateway can raise
//! through its router surfaces as an RFC 9457 problem document with the
//! documented type identifier, status, the error-source marker and — where the
//! error is retriable — a `Retry-After`.
//!
//! The error table lives in `docs/DESIGN.md` ("Gateway error responses"); each
//! test names the row it pins down.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::{StatusCode, header};
use common::Harness;
use httpmock::MockServer;
use serde_json::{Value, json};

/// The problem document a response carries, plus its headers.
struct Problem {
    status: StatusCode,
    content_type: Option<String>,
    body: Value,
    headers: axum::http::HeaderMap,
}

/// Sends a request and reads the problem document out of the answer.
async fn ask(harness: &Harness, method: &str, uri: &str, body: Option<Value>) -> Problem {
    let (status, body, headers) = harness.json_with_headers(method, uri, body).await;
    Problem {
        status,
        content_type: headers
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap_or_default().to_owned()),
        body,
        headers,
    }
}

/// The marker the response carries, or `None` when it carries none.
fn source_of(problem: &Problem) -> Option<&str> {
    problem
        .headers
        .get("x-oagw-error-source")
        .and_then(|value| value.to_str().ok())
}

/// Asserts the shape every problem document shares.
fn assert_document(problem: &Problem, expected_type: &str, expected_status: u16) {
    assert_eq!(
        problem.content_type.as_deref(),
        Some("application/problem+json"),
        "{:?}",
        problem.body
    );
    assert_eq!(
        problem.body["type"],
        json!(oagw::gts_helpers::error_type_id(
            expected_type
                .rsplit_once('.')
                .map_or(expected_type, |_| expected_type)
        )),
        "{:?}",
        problem.body
    );
    assert_eq!(problem.body["status"], json!(expected_status));
    assert_eq!(problem.status.as_u16(), expected_status);
    assert!(
        problem.body["title"].is_string(),
        "a title names the condition: {:?}",
        problem.body
    );
    assert!(
        problem.body["detail"].is_string(),
        "a detail explains it: {:?}",
        problem.body
    );
    assert!(
        problem.body["instance"].is_string(),
        "an instance locates it: {:?}",
        problem.body
    );
    assert!(
        source_of(problem).is_some(),
        "every answer names its source: {:?}",
        problem.headers
    );
}

/// A fully typed identifier for a family, spelled out to keep the test readable.
fn family(name: &str) -> String {
    oagw::gts_helpers::error_type_id(name)
}

#[tokio::test]
async fn an_unknown_alias_is_route_not_found() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let problem = ask(
        &harness,
        "GET",
        "/oagw/v1/proxy/no-such-alias/v1/things",
        None,
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("route.not_found")));
    assert_document(&problem, "route.not_found", 404);
    assert_eq!(source_of(&problem), Some("gateway"));
    assert_eq!(
        problem.body["instance"],
        json!("/oagw/v1/proxy/no-such-alias/v1/things")
    );
}

#[tokio::test]
async fn a_request_without_a_matching_route_is_route_not_found() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

    let problem = ask(
        &harness,
        "DELETE",
        "/oagw/v1/proxy/api.example.com/v1/chat",
        None,
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("route.not_found")));
    assert_document(&problem, "route.not_found", 404);
    assert_eq!(source_of(&problem), Some("gateway"));
}

#[tokio::test]
async fn a_malformed_upstream_is_a_validation_error() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let problem = ask(
        &harness,
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({"alias": "api.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                    "server": {"endpoints": [{"scheme": "ftp", "host": "api.example.com", "port": 21}]}})),
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("validation.error")));
    assert_document(&problem, "validation.error", 400);
    assert_eq!(source_of(&problem), Some("gateway"));
}

#[tokio::test]
async fn a_duplicate_alias_is_a_conflict() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    harness.seed_upstream("api.example.com", &server).await;

    let problem = ask(
        &harness,
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({
            "alias": "api.example.com",
            "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
            "server": {"endpoints": [
                {"scheme": "http", "host": server.host(), "port": server.port()}
            ]}
        })),
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("conflict")));
    assert_document(&problem, "conflict", 409);
}

#[tokio::test]
async fn a_plugin_still_in_use_is_a_conflict_naming_the_plugin() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

    let (status, plugin) = harness
        .json(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "name": "shaper",
                "plugin_type": "transform",
                "source_code": "def transform(ctx): pass"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    let plugin_id = plugin["id"].as_str().unwrap().to_owned();

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "plugins": {"items": [plugin_id]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let problem = ask(
        &harness,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("plugin.in_use")));
    assert_document(&problem, "plugin.in_use", 409);
}

#[tokio::test]
async fn a_disabled_upstream_is_link_unavailable_and_retriable() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "enabled": false,
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let problem = ask(
        &harness,
        "POST",
        "/oagw/v1/proxy/api.example.com/v1/chat",
        Some(json!({})),
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("link.unavailable")));
    assert_document(&problem, "link.unavailable", 503);
    assert_eq!(source_of(&problem), Some("gateway"));
    let retry_after = problem
        .headers
        .get(header::RETRY_AFTER)
        .map(|value| value.to_str().unwrap_or_default().to_owned());
    assert!(
        retry_after.is_some(),
        "the caller is told to come back: {retry_after:?}"
    );
}

#[tokio::test]
async fn a_target_host_outside_the_pool_is_unknown() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

    let (status, body, _) = harness
        .send_raw(common::raw_request(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            &[("x-oagw-target-host", "stranger.example.com")],
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], json!(family("routing.unknown_target_host")));
    assert_eq!(body["status"], json!(400));
    assert!(body["detail"].is_string(), "{body}");
    assert_eq!(
        body["instance"],
        json!("/oagw/v1/proxy/api.example.com/v1/chat")
    );
}

#[tokio::test]
async fn an_empty_rate_bucket_is_rejected_with_retry_after() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "rate_limit": {
                    "sustained": {"rate": 1, "window": "second"},
                    "burst": {"capacity": 1}
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let _answer = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/chat");
        then.status(200);
    });

    let (first, problem, _) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(first, StatusCode::OK, "{problem}");

    let problem = ask(
        &harness,
        "POST",
        "/oagw/v1/proxy/api.example.com/v1/chat",
        Some(json!({})),
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("rate_limit.exceeded")));
    assert_document(&problem, "rate_limit.exceeded", 429);
    assert_eq!(source_of(&problem), Some("gateway"));
    let retry = problem
        .headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default();
    assert!(retry >= 1, "the caller is told when to come back: {retry}");
    assert_eq!(
        problem.body["retry_after_seconds"],
        json!(retry),
        "the body repeats the header's guidance"
    );
}

#[tokio::test]
async fn a_missing_credential_is_reported_without_its_material() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

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

    // The store knows the key, so the credential is resolved and injected: the
    // request goes out, and no part of the answer names the secret's value.
    let answer = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/chat")
            .header("authorization", "sk-secret-value");
        then.status(200);
    });
    let (status, body, _) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(answer.calls(), 1);
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains("sk-secret-value"),
        "no secret material in the answer body: {body}"
    );
}

#[tokio::test]
async fn an_unresolvable_credential_is_secret_not_found() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("api.example.com", &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

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
                        "secret_ref": "cred://acme/absent-key"
                    }
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let problem = ask(
        &harness,
        "POST",
        "/oagw/v1/proxy/api.example.com/v1/chat",
        Some(json!({})),
    )
    .await;
    assert_eq!(problem.body["type"], json!(family("secret.not_found")));
    assert_document(&problem, "secret.not_found", 500);
    assert_eq!(source_of(&problem), Some("gateway"));
    assert!(
        !problem.body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("sk-secret-value"),
        "the stored value never appears: {:?}",
        problem.body["detail"]
    );
}

#[tokio::test]
async fn an_unreachable_upstream_is_reported_as_a_gateway_failure() {
    let harness = Harness::without_mock();
    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "dark.example.com",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "127.0.0.1", "port": 9}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{id}/routes"),
            Some(json!({
                "match": {"http": {"path": "/v1/chat", "methods": ["POST"]}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, problem, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/dark.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status.as_u16(), 502, "{problem}");
    assert!(
        problem["type"].is_string(),
        "a transport failure names its family: {problem}"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway"),
        "nothing was relayed, so the failure is the gateway's"
    );
}
