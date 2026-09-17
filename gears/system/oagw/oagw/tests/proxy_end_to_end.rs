//! Data-plane behaviour driven through the real handler stack.
//!
//! Every test mounts the OAGW routes on an axum `Router`, points an upstream at
//! a live `httpmock` server and asserts what actually crosses the wire in both
//! directions.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::{
    ERROR_SOURCE, FixedSecrets, Harness, assert_problem, json, request, tenant_context, text,
};
use httpmock::MockServer;
use serde_json::{Value, json};

const ALIAS: &str = "vendor.example";

fn plaintext_config() -> oagw::config::OagwConfig {
    oagw::config::OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicyConfig {
            enabled: false,
            denied_segments: Vec::new(),
        },
        ..oagw::config::OagwConfig::default()
    }
}

/// Spins up a mock upstream and registers it as a single-endpoint upstream with
/// a route whose path prefix is `prefix`.
struct Upstream {
    server: MockServer,
    upstream_id: String,
    harness: Harness,
    ctx: toolkit_security::SecurityContext,
}

impl Upstream {
    /// Builds the fixture with a plaintext `http` endpoint and one route.
    async fn start(prefix: &str) -> Self {
        Self::with_config(prefix, plaintext_config()).await
    }

    /// Builds the fixture over an explicit configuration.
    async fn with_config(prefix: &str, config: oagw::config::OagwConfig) -> Self {
        let harness = Harness::with_config(config);
        let server = MockServer::start();
        let ctx = tenant_context();
        let host = server.host().to_owned();
        let port = server.port();
        let upstream_id = {
            let harness = &harness;
            let ctx = ctx.clone();
            let created = async {
                let mut response = harness
                    .json(
                        ctx,
                        Method::POST,
                        "/oagw/v1/upstreams",
                        json!({
                            "enabled": true,
                            "alias": ALIAS,
                            "server": { "endpoints": [
                                { "scheme": "http", "host": host, "port": port }
                            ]}
                        }),
                    )
                    .await;
                assert_eq!(response.status(), StatusCode::CREATED, "upstream fixture");
                json(&mut response).await
            }
            .await;
            created["id"].as_str().expect("id").to_owned()
        };

        harness
            .json(
                ctx.clone(),
                Method::POST,
                "/oagw/v1/routes",
                json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": ["GET", "POST"], "path": prefix } }
                }),
            )
            .await;
        Self {
            server,
            upstream_id,
            harness,
            ctx,
        }
    }

    /// Replaces the upstream with the supplied extra configuration merged in.
    async fn reconfigure(&self, patch: Value) {
        let mut body = json!({
            "enabled": true,
            "alias": ALIAS,
            "server": { "endpoints": [
                { "scheme": "http", "host": self.server.host(), "port": self.server.port() }
            ]}
        });
        if let Some(object) = patch.as_object() {
            for (key, value) in object {
                body[key.clone()] = value.clone();
            }
        }
        let response = self
            .harness
            .json(
                self.ctx.clone(),
                Method::PUT,
                &format!("/oagw/v1/upstreams/{}", self.upstream_id),
                body,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "reconfigure upstream");
    }

    /// Sends a proxy request with the supplied headers.
    async fn proxy(&self, path: &str, headers: &[(&str, &str)]) -> axum::response::Response {
        let request = {
            let mut builder = axum::http::Request::builder()
                .method(Method::GET)
                .uri(format!("/oagw/v1/proxy/{ALIAS}{path}"));
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            builder.body(axum::body::Body::empty()).expect("request")
        };
        self.harness.send(self.ctx.clone(), request).await
    }
}

#[tokio::test]
async fn proxies_a_get_request_with_query_and_headers() {
    let upstream = Upstream::start("/v1").await;
    let mock = upstream.server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/pets")
            .query_param("breed", "pug")
            .header("accept", "application/json")
            .header_missing("connection")
            .header_missing("x-oagw-target-host")
            .header(
                "host",
                format!("{}:{}", upstream.server.host(), upstream.server.port()),
            );
        then.status(200)
            .header("content-type", "application/json")
            .header("connection", "close")
            .body(r#"{"name":"pug"}"#);
    });

    let mut response = upstream
        .proxy(
            "/v1/pets?breed=pug",
            &[("accept", "application/json"), ("connection", "keep-alive")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE)
            .and_then(|value| value.to_str().ok()),
        Some("upstream"),
        "successful passthrough responses are stamped with the upstream source"
    );
    let body = text(&mut response).await;
    assert_eq!(body, r#"{"name":"pug"}"#);
    mock.assert_calls(1);
}

#[tokio::test]
async fn strips_hop_by_hop_and_routing_headers_from_the_response() {
    let upstream = Upstream::start("/v1").await;
    let mock = upstream.server.mock(|when, then| {
        when.path("/v1/echo");
        then.status(200)
            .header("connection", "keep-alive")
            .header("transfer-encoding", "chunked")
            .header("server", "upstream-1")
            .header("x-oagw-target-host", "someone-else")
            .header("x-oagw-error-source", "upstream")
            .body("ok");
    });

    let response = upstream.proxy("/v1/echo", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert!(
        headers.get("connection").is_none(),
        "hop-by-hop headers are stripped"
    );
    assert!(headers.get("transfer-encoding").is_none());
    assert!(
        headers.get("x-oagw-target-host").is_none(),
        "routing headers consumed by the gateway are stripped"
    );
    assert!(
        headers.get("server").is_some(),
        "ordinary headers pass through"
    );
    mock.assert();
}

#[tokio::test]
async fn unknown_alias_returns_a_404_problem() {
    let harness = Harness::with_config(plaintext_config());
    let mut response = harness
        .send(
            tenant_context(),
            request(Method::GET, "/oagw/v1/proxy/missing.example/v1", &[]),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::NOT_FOUND).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert!(
        body["instance"]
            .as_str()
            .is_some_and(|i| i.contains("/oagw/v1/proxy"))
    );
}

#[tokio::test]
async fn no_matching_route_returns_a_404_problem() {
    let upstream = Upstream::start("/v1").await;
    let mut response = upstream.proxy("/v2/other", &[]).await;
    let body = assert_problem(&mut response, StatusCode::NOT_FOUND).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn disabled_upstream_returns_503_and_disabled_route_returns_404() {
    let upstream = Upstream::start("/v1").await;

    // Disable the upstream and observe 503.
    let mut body = json!({
        "enabled": false,
        "alias": ALIAS,
        "server": { "endpoints": [
            { "scheme": "http", "host": upstream.server.host(), "port": upstream.server.port() }
        ]}
    });
    let response = upstream
        .harness
        .json(
            upstream.ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/upstreams/{}", upstream.upstream_id),
            body.clone(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut response = upstream.proxy("/v1/pets", &[]).await;
    let problem = assert_problem(&mut response, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );

    // Re-enable the upstream and disable the route instead: 404.
    body["enabled"] = json!(true);
    let response = upstream
        .harness
        .json(
            upstream.ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/upstreams/{}", upstream.upstream_id),
            body,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut response = upstream
        .harness
        .json(
            upstream.ctx.clone(),
            Method::GET,
            "/oagw/v1/routes",
            json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let routes = json(&mut response).await;
    let route_id = routes[0]["id"].as_str().expect("route id").to_owned();
    let response = upstream
        .harness
        .json(
            upstream.ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/routes/{route_id}"),
            json!({
                "upstream_id": upstream.upstream_id,
                "enabled": false,
                "match": { "http": { "methods": ["GET", "POST"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut response = upstream.proxy("/v1/pets", &[]).await;
    assert_problem(&mut response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn plaintext_upstream_is_refused_when_allow_http_upstream_is_disabled() {
    let config = oagw::config::OagwConfig {
        allow_http_upstream: false,
        ..plaintext_config()
    };
    let upstream = Upstream::with_config("/v1", config).await;
    let mut response = upstream.proxy("/v1/pets", &[]).await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("allow_http_upstream"))
    );
}

#[tokio::test]
async fn body_ceiling_and_framing_are_enforced_before_dialling() {
    let upstream = Upstream::start("/v1").await;

    // An unsupported transfer encoding is rejected before any upstream call.
    let request = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .header("transfer-encoding", "gzip")
        .body(axum::body::Body::from("payload"))
        .expect("request");
    let mut response = upstream.harness.send(upstream.ctx.clone(), request).await;
    assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn oversized_declared_bodies_return_413() {
    let config = oagw::config::OagwConfig {
        max_body_bytes: 8,
        ..plaintext_config()
    };
    let upstream = Upstream::with_config("/v1", config).await;
    let request = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .header("content-length", "4096")
        .body(axum::body::Body::from(vec![b'x'; 4096]))
        .expect("request");
    let mut response = upstream.harness.send(upstream.ctx.clone(), request).await;
    let body = assert_problem(&mut response, StatusCode::PAYLOAD_TOO_LARGE).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[tokio::test]
async fn rate_limit_exhaustion_returns_429_with_retry_after_and_headers() {
    let upstream = Upstream::start("/v1").await;
    upstream
        .reconfigure(json!({
            "rate_limit": {
                "sustained": { "rate": 1, "window": "minute" },
                "burst": 1,
                "scope": "tenant",
                "cost": 1,
                "response_headers": true,
                "enabled": true
            }
        }))
        .await;
    upstream.server.mock(|when, then| {
        when.path("/v1/pets");
        then.status(200).body("ok");
    });

    let first = upstream.proxy("/v1/pets", &[]).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        first
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok()),
        Some("0")
    );

    let mut second = upstream.proxy("/v1/pets", &[]).await;
    let body = assert_problem(&mut second, StatusCode::TOO_MANY_REQUESTS).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert!(
        second
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .is_some(),
        "429 carries a Retry-After hint"
    );
}

#[tokio::test]
async fn required_headers_guard_rejects_on_request_and_response_phases() {
    let upstream = Upstream::start("/v1").await;
    upstream
        .reconfigure(json!({
            "plugins": { "items": [ {
                "id": oagw::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                "config": {
                    "required_request_headers": "x-partner-id",
                    "required_response_headers": "x-upstream-contract"
                }
            } ] }
        }))
        .await;

    let mut response = upstream.proxy("/v1/pets", &[]).await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("x-partner-id"))
    );

    // A compliant request passes the guard, but the upstream response is
    // missing the required header, so the gateway answers 502.
    let mock = upstream.server.mock(|when, then| {
        when.path("/v1/pets");
        then.status(200).body("ok");
    });
    let mut response = upstream
        .proxy("/v1/pets", &[("x-partner-id", "partner-1")])
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_GATEWAY).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
    );
    mock.assert();
}

#[tokio::test]
async fn api_key_is_injected_into_the_outbound_header() {
    let config = plaintext_config();
    let harness = Harness::build(
        config,
        Arc::new(oagw::infra::state::StaticTenantChain),
        Arc::new(FixedSecrets(vec![(
            "partner-key".to_owned(),
            "s3cr3t".to_owned(),
        )])),
    );
    let server = MockServer::start();
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": server.host(), "port": server.port() }
                ]},
                "auth": {
                    "type": oagw::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID,
                    "config": {
                        "api_key_ref": "cred://partner-key",
                        "api_key_name": "x-api-key"
                    }
                }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mock = server.mock(|when, then| {
        when.path("/v1/pets").header("x-api-key", "s3cr3t");
        then.status(200).body("ok");
    });

    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .body(axum::body::Body::empty())
        .expect("request");
    let response = harness.send(ctx.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();

    // The credential never reaches the caller, even in a failure path.
    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v2/missing"))
        .body(axum::body::Body::empty())
        .expect("request");
    let mut response = harness.send(ctx.clone(), request).await;
    let body = assert_problem(&mut response, StatusCode::NOT_FOUND).await;
    assert!(
        !serde_json::to_string(&body)
            .expect("json")
            .contains("s3cr3t"),
        "credential material must never appear in an error response"
    );
}

#[tokio::test]
async fn api_key_is_injected_into_the_query_with_a_prefix() {
    let config = plaintext_config();
    let harness = Harness::build(
        config,
        Arc::new(oagw::infra::state::StaticTenantChain),
        Arc::new(FixedSecrets(vec![(
            "partner-key".to_owned(),
            "s3cr3t".to_owned(),
        )])),
    );
    let server = MockServer::start();
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": server.host(), "port": server.port() }
                ]},
                "auth": {
                    "type": oagw::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID,
                    "config": {
                        "api_key_ref": "partner-key",
                        "api_key_in": "query",
                        "api_key_name": "api_key",
                        "api_key_prefix": "Bearer "
                    }
                }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mock = server.mock(|when, then| {
        when.path("/v1/pets")
            .query_param("api_key", "Bearer s3cr3t")
            .header_missing("x-api-key");
        then.status(200).body("ok");
    });

    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .body(axum::body::Body::empty())
        .expect("request");
    let mut response = harness.send(ctx.clone(), request).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        text(&mut response).await
    );
    mock.assert();
}

#[tokio::test]
async fn unresolved_credential_reference_fails_closed_without_leaking_the_value() {
    let config = plaintext_config();
    let harness = Harness::build(
        config,
        Arc::new(oagw::infra::state::StaticTenantChain),
        Arc::new(FixedSecrets(Vec::new())),
    );
    let server = MockServer::start();
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": server.host(), "port": server.port() }
                ]},
                "auth": {
                    "type": oagw::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID,
                    "config": { "api_key_ref": "cred://absent" }
                }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .body(axum::body::Body::empty())
        .expect("request");
    let mut response = harness.send(ctx.clone(), request).await;
    let body = assert_problem(&mut response, StatusCode::UNAUTHORIZED).await;
    let rendered = serde_json::to_string(&body).expect("json");
    assert!(
        !rendered.contains("cred://absent"),
        "the failing reference must not be echoed back"
    );
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
}

#[tokio::test]
async fn cors_actual_requests_are_enforced_after_upstream_resolution() {
    let upstream = Upstream::start("/v1").await;
    upstream
        .reconfigure(json!({
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://console.example"],
                "allowed_methods": ["GET", "POST"]
            }
        }))
        .await;
    upstream.server.mock(|when, then| {
        when.path("/v1/pets");
        then.status(200).body("ok");
    });

    let response = upstream
        .proxy("/v1/pets", &[("origin", "https://console.example")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://console.example")
    );

    let mut response = upstream
        .proxy("/v1/pets", &[("origin", "https://evil.example")])
        .await;
    let body = assert_problem(&mut response, StatusCode::FORBIDDEN).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
}

#[tokio::test]
async fn preflight_is_answered_at_the_handler_level() {
    let upstream = Upstream::start("/v1").await;
    let request = axum::http::Request::builder()
        .method(Method::OPTIONS)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .header("origin", "https://console.example")
        .header("access-control-request-method", "GET")
        .header("access-control-request-headers", "content-type")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = upstream.harness.send(upstream.ctx.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let headers = response.headers().clone();
    // ADR 0004: the handler-level preflight is permissive — it echoes the
    // requested origin, method and headers without resolving an upstream.
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://console.example")
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("GET")
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some("content-type")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|value| value.to_str().ok()),
        Some("86400")
    );
    assert!(
        headers
            .get("vary")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|vary| vary.contains("Origin")),
        "the preflight varies on the CORS request headers"
    );
}

#[tokio::test]
async fn target_host_is_required_for_a_common_suffix_alias() {
    // The alias `vendor.example` is the common suffix of both endpoints, so the
    // caller has to disambiguate with `X-OAGW-Target-Host`. The endpoints are
    // named hosts that no DNS in the sandbox resolves: the two rejections below
    // happen before any dial, and the pinned-selection matrix is covered by the
    // `resolve_target_host` unit tests in the data plane.
    let harness = Harness::with_config(plaintext_config());
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": "vendor.example",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "us.vendor.example", "port": 443 },
                    { "scheme": "https", "host": "eu.vendor.example", "port": 443 }
                ]}
            }),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(&mut response).await
    );
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mut response = harness
        .send(
            ctx.clone(),
            request(Method::GET, "/oagw/v1/proxy/vendor.example/v1/pets", &[]),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(
        body["valid_hosts"],
        json!(["eu.vendor.example", "us.vendor.example"]),
        "the problem lists the valid hosts"
    );

    let mut response = harness
        .send(
            ctx.clone(),
            request(
                Method::GET,
                "/oagw/v1/proxy/vendor.example/v1/pets",
                &[("x-oagw-target-host", "not-a-pool-member.example")],
            ),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );

    let mut response = harness
        .send(
            ctx.clone(),
            request(
                Method::GET,
                "/oagw/v1/proxy/vendor.example/v1/pets",
                &[("x-oagw-target-host", "us.vendor.example:443")],
            ),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        body["type"], "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
        "a target host with a port is not a bare hostname"
    );
}

#[tokio::test]
async fn server_sent_events_stream_through_with_their_content_type() {
    let upstream = Upstream::start("/v1").await;
    let mock = upstream.server.mock(|when, then| {
        when.path("/v1/stream");
        then.status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body("event: start\ndata: one\n\n\nevent: end\ndata: two\n\n");
    });

    let mut response = upstream.proxy("/v1/stream", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream"),
        "the SSE media type survives the proxy hop"
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    let body = text(&mut response).await;
    assert!(body.contains("data: one"));
    assert!(body.contains("data: two"));
    mock.assert();
}

#[tokio::test]
async fn upstream_failure_maps_onto_a_gateway_problem() {
    // A listener that is immediately closed: the endpoint address is well
    // formed but nothing is listening, so the dial is refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    let harness = Harness::with_config(plaintext_config());
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": "dead.example",
                "server": { "endpoints": [
                    { "scheme": "http", "host": "127.0.0.1", "port": port }
                ]}
            }),
        )
        .await;
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();
    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mut response = harness
        .send(
            ctx.clone(),
            request(Method::GET, "/oagw/v1/proxy/dead.example/v1/pets", &[]),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_GATEWAY).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
}

#[tokio::test]
async fn longest_prefix_route_wins_and_suffix_rules_apply() {
    let harness = Harness::with_config(plaintext_config());
    let server = MockServer::start();
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": server.host(), "port": server.port() }
                ]}
            }),
        )
        .await;
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    for path in ["/v1", "/v1/pets"] {
        let response = harness
            .json(
                ctx.clone(),
                Method::POST,
                "/oagw/v1/routes",
                json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": ["GET"], "path": path } }
                }),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let broad = server.mock(|when, then| {
        when.path("/v1/pets");
        then.status(200).body("narrow");
    });
    let mut response = harness
        .send(
            ctx.clone(),
            request(Method::GET, &format!("/oagw/v1/proxy/{ALIAS}/v1/pets"), &[]),
        )
        .await;
    assert_eq!(text(&mut response).await, "narrow");
    broad.assert();

    let fallback = server.mock(|when, then| {
        when.path("/v1/other");
        then.status(200).body("broad");
    });
    let mut response = harness
        .send(
            ctx.clone(),
            request(
                Method::GET,
                &format!("/oagw/v1/proxy/{ALIAS}/v1/other"),
                &[],
            ),
        )
        .await;
    assert_eq!(text(&mut response).await, "broad");
    fallback.assert();
}

#[tokio::test]
async fn query_allowlist_rejects_unknown_parameters() {
    let harness = Harness::with_config(plaintext_config());
    let server = MockServer::start();
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": server.host(), "port": server.port() }
                ]}
            }),
        )
        .await;
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();
    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": {
                    "http": { "methods": ["GET"], "path": "/v1", "query_allowlist": ["breed"] }
                }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    server.mock(|when, then| {
        when.path("/v1/pets");
        then.status(200).body("ok");
    });

    let response = harness
        .send(
            ctx.clone(),
            request(
                Method::GET,
                &format!("/oagw/v1/proxy/{ALIAS}/v1/pets?breed=pug"),
                &[],
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut response = harness
        .send(
            ctx.clone(),
            request(
                Method::GET,
                &format!("/oagw/v1/proxy/{ALIAS}/v1/pets?leak=1"),
                &[],
            ),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("'leak'"))
    );
}

#[tokio::test]
async fn tenant_chain_ancestors_cannot_shadow_a_descendant_alias() {
    // The caller's tenant owns the upstream; a parent tenant in the chain does
    // not exist in the store, so resolution must still succeed.
    let harness = Harness::build(
        plaintext_config(),
        Arc::new(common::FixedChain(vec![
            common::TENANT,
            uuid::Uuid::from_u128(0xfff),
        ])),
        Arc::new(common::FailSecrets),
    );
    let server = MockServer::start();
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": server.host(), "port": server.port() }
                ]}
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mock = server.mock(|when, then| {
        when.path("/v1/pets");
        then.status(200).body("owned");
    });
    let mut response = harness
        .send(
            ctx.clone(),
            request(Method::GET, &format!("/oagw/v1/proxy/{ALIAS}/v1/pets"), &[]),
        )
        .await;
    assert_eq!(text(&mut response).await, "owned");
    mock.assert();
}

#[tokio::test]
async fn unauthenticated_callers_cannot_proxy() {
    let upstream = Upstream::start("/v1").await;
    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/v1/pets"))
        .body(axum::body::Body::empty())
        .expect("request");
    let router = upstream.harness.router(common::anonymous_context());
    let response = tower::ServiceExt::oneshot(router, request)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Minimal WebSocket echo upstream: completes the handshake, then echoes every
/// frame back to the peer.
///
/// `Sec-WebSocket-Accept` is a fixed token: the gateway must relay it verbatim,
/// and neither side in this test re-derives the digest.
async fn spawn_websocket_upstream() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream listener");
    let address = listener.local_addr().expect("upstream address");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                loop {
                    match socket.read(&mut byte).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
                            head.push(byte[0]);
                            if head.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let request = String::from_utf8_lossy(&head);
                if !request.to_ascii_lowercase().contains("upgrade: websocket") {
                    let _ = socket
                        .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let _ = socket
                    .write_all(
                        concat!(
                            "HTTP/1.1 101 Switching Protocols\r\n",
                            "upgrade: websocket\r\n",
                            "connection: Upgrade\r\n",
                            "sec-websocket-accept: mock-accept-token\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;

                // Echo every client frame back, unmasked, as a server frame.
                let mut header = [0_u8; 2];
                loop {
                    if socket.read_exact(&mut header).await.is_err() {
                        return;
                    }
                    let opcode = header[0] & 0x0f;
                    let masked = header[1] & 0x80 != 0;
                    let length = u64::from(header[1] & 0x7f);
                    let payload_len = usize::try_from(length).unwrap_or(0);
                    let mut mask = [0_u8; 4];
                    if masked && socket.read_exact(&mut mask).await.is_err() {
                        return;
                    }
                    let mut payload = vec![0_u8; payload_len];
                    if socket.read_exact(&mut payload).await.is_err() {
                        return;
                    }
                    if masked {
                        for (index, byte) in payload.iter_mut().enumerate() {
                            *byte ^= mask[index % 4];
                        }
                    }
                    let mut frame = vec![0x80 | opcode, payload.len().min(126) as u8];
                    frame.extend_from_slice(&payload);
                    if socket.write_all(&frame).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}

/// Sends one masked client frame and reads the echoed server frame.
async fn websocket_round_trip(socket: &mut tokio::net::TcpStream, message: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mask = [0x2a_u8, 0x4b, 0x11, 0x7f];
    let payload = message.as_bytes();
    let mut frame = vec![0x81_u8, 0x80 | payload.len().min(126) as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    socket.write_all(&frame).await.expect("client frame");

    let mut header = [0_u8; 2];
    socket.read_exact(&mut header).await.expect("echo header");
    let length = usize::from(header[1] & 0x7f);
    let mut echoed = vec![0_u8; length];
    socket.read_exact(&mut echoed).await.expect("echo body");
    String::from_utf8(echoed).expect("utf-8 echo")
}

#[tokio::test]
async fn websocket_upgrades_splice_the_two_sockets() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let upstream_address = spawn_websocket_upstream().await;

    let harness = Harness::with_config(plaintext_config());
    let ctx = tenant_context();
    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "enabled": true,
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": upstream_address.ip().to_string(), "port": upstream_address.port() }
                ]}
            }),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(&mut response).await
    );
    let created = json(&mut response).await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();
    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/socket" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // `Router::oneshot` has no socket, so the upgrade needs a real server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("gateway listener");
    let gateway_address = listener.local_addr().expect("gateway address");
    let router = harness.router(ctx.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let mut socket = tokio::net::TcpStream::connect(gateway_address)
        .await
        .expect("client connection");
    let handshake = format!(
        "GET /oagw/v1/proxy/{ALIAS}/socket HTTP/1.1\r\n\
         host: {gateway_address}\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    socket
        .write_all(handshake.as_bytes())
        .await
        .expect("handshake");

    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        socket.read_exact(&mut byte).await.expect("response head");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let response = String::from_utf8_lossy(&head).to_ascii_lowercase();

    assert!(
        response.starts_with("http/1.1 101"),
        "the gateway switches protocols: {response}"
    );
    assert!(
        response.contains("upgrade: websocket"),
        "the upgrade header is relayed: {response}"
    );
    assert!(
        response.contains("sec-websocket-accept: mock-accept-token"),
        "the upstream accept token is relayed: {response}"
    );

    let echoed = websocket_round_trip(&mut socket, "ping-through-oagw").await;
    assert_eq!(echoed, "ping-through-oagw");
    server.abort();
}
