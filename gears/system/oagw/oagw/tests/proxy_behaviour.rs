//! Router-level tests for the proxy (data-plane) surface.
//!
//! Every test drives the gear's own `Router` against an in-process upstream, so
//! what is asserted here is what a caller sees: status codes, the headers the
//! upstream actually received, and the error semantics the gateway applies.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{GATEWAY_SOURCE, Harness, JsonConfig, PROTOCOL_HTTP, record, request, request_with};
use serde_json::Value;
use uuid::Uuid;

const UPSTREAM_SOURCE: &str = "upstream";

/// An upstream with one route matching everything, and nothing else configured.
async fn gateway() -> Harness {
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await;
    harness.simple_upstream("echo", None).await;
    harness
}

/// A gateway whose stand-in upstream is registered under `alias` with a
/// catch-all route, so a test can address it at any path.
async fn harness_with_upstream(alias: &str) -> Harness {
    let harness = gateway_named(alias).await;
    harness
}

/// A gateway with an upstream named `alias` and no route yet, for the tests
/// that register their own.
async fn harness_without_route(alias: &str) -> Harness {
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await;
    let document = serde_json::json!({
        "alias": alias,
        "protocol": common::PROTOCOL_HTTP,
        "server": {
            "endpoints": [{
                "scheme": "http",
                "host": harness.upstream().host(),
                "port": harness.upstream_port(),
            }]
        },
    });
    harness.register_upstream(alias, document).await;
    harness
}

/// A gateway with one upstream named `alias` and a catch-all route.
async fn gateway_named(alias: &str) -> Harness {
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await;
    harness.simple_upstream(alias, None).await;
    harness
}

/// An upstream carrying extra upstream configuration, for the policy tests.
async fn gateway_with(extra: Value, secrets: Vec<(String, String)>) -> Harness {
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), secrets).await;
    harness.simple_upstream("echo", Some(extra)).await;
    harness
}

#[tokio::test]
async fn an_unresolvable_alias_is_a_404_from_the_gateway() {
    let harness = gateway().await;
    let response = record(harness.serve(request("GET", "/oagw/v1/proxy/nosuch", None)).await).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(response.body["type"], "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    assert_eq!(response.body["status"], 404);
    assert_eq!(response.body["title"], "Route Not Found");
    assert!(response.detail().contains("nosuch"), "{}", response.detail());
}

#[tokio::test]
async fn a_tenant_without_the_alias_sees_a_404_not_another_tenants_upstream() {
    let harness = gateway().await;
    // Tenant A registered `echo`; another tenant has no upstream by that alias.
    let response = record(
        harness
            .serve_as(
                uuid::Uuid::from_u128(0x9999),
                request("GET", "/oagw/v1/proxy/echo/echo", None),
            )
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
}

#[tokio::test]
async fn a_get_is_forwarded_with_the_path_appended() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.source(), Some(UPSTREAM_SOURCE));
    assert_eq!(response.body["method"], "GET");
    assert_eq!(response.body["path"], "/echo");
}

#[tokio::test]
async fn a_query_string_is_forwarded_verbatim() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo?a=1&b=two", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["query"], "a=1&b=two");
}

#[tokio::test]
async fn a_post_body_is_forwarded() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/proxy/echo/echo",
                Some(serde_json::json!({ "hello": "world" })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["method"], "POST");
}

#[tokio::test]
async fn an_upstream_status_is_forwarded_untouched() {
    let harness = gateway().await;
    for code in [201, 204, 404, 500, 503] {
        let response = record(
            harness
                .serve(request("GET", &format!("/oagw/v1/proxy/echo/status/{code}"), None))
                .await,
        )
        .await;
        assert_eq!(response.status.as_u16(), code, "the upstream's status is not rewritten");
        assert_eq!(response.source(), Some(UPSTREAM_SOURCE));
    }
}

#[tokio::test]
async fn an_upstream_5xx_is_marked_upstream_not_gateway() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/status/503", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.source(),
        Some(UPSTREAM_SOURCE),
        "a 5xx the upstream sent is the upstream's, not the gateway's"
    );
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_from_the_gateway() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    // Nothing listens on port 1.
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 1 }] }
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_GATEWAY);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(
        response.body["upstream_id"],
        harness.upstream_id("echo").to_string(),
        "the problem names the upstream that could not be reached"
    );
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_allow_http_is_false() {
    let harness = Harness::build(&JsonConfig::new(false, 1024 * 1024), Vec::new())
        .await
        .0;
    harness.simple_upstream("echo", None).await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the scheme is accepted at create time and refused at dial time"
    );
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
}

#[tokio::test]
async fn a_plaintext_upstream_is_dialled_when_allow_http_is_true() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the same document the false case refuses is dialled here"
    );
}

#[tokio::test]
async fn hop_by_hop_headers_do_not_reach_the_upstream() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[
                    ("connection", "keep-alive"),
                    ("keep-alive", "timeout=5"),
                    ("proxy-connection", "keep-alive"),
                    ("x-caller-header", "value"),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let headers = response.body["headers"].clone();
    for hop in ["connection", "keep-alive", "proxy-connection"] {
        assert!(headers.get(hop).is_none(), "`{hop}` must not reach the upstream");
    }
    assert!(
        headers.get("x-caller-header").is_none(),
        "the default passthrough is `none`: nothing the caller sent is forwarded"
    );
}

#[tokio::test]
async fn the_upstream_receives_a_host_header_for_itself() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    let host = response.body["headers"]["host"].as_str().unwrap_or_default();
    assert!(!host.is_empty(), "the upstream must receive a Host header");
}

#[tokio::test]
async fn a_response_header_rule_reaches_the_caller() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "headers": { "response": { "set": { "x-gateway-mark": "present" } } },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/status/200", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header("x-gateway-mark"), Some("present"));
}

#[tokio::test]
async fn a_response_header_rule_can_remove_an_upstream_header() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "headers": { "response": { "remove": ["x-upstream-mark"] } },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/sse", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        response.header("x-upstream-mark").is_none(),
        "a removed header must not reach the caller"
    );
}

#[tokio::test]
async fn a_request_header_allowlist_selects_what_the_upstream_sees() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "headers": {
                    "request": {
                        "passthrough": "allowlist",
                        "passthrough_allowlist": ["x-keep-me"],
                        "set": { "x-gateway-set": "yes" },
                    },
                },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("x-keep-me", "kept"), ("x-drop-me", "dropped")],
            ))
            .await,
    )
    .await;
    let headers = response.body["headers"].clone();
    assert!(
        headers.get("x-drop-me").is_none(),
        "a header outside the allowlist is not forwarded"
    );
    assert_eq!(headers.get("x-keep-me").and_then(Value::as_str), Some("kept"));
    assert_eq!(headers.get("x-gateway-set").and_then(Value::as_str), Some("yes"));
}

#[tokio::test]
async fn a_request_header_rule_can_remove_a_header() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "headers": { "request": { "remove": ["x-secret"] } },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("x-secret", "value")],
            ))
            .await,
    )
    .await;
    let headers = response.body["headers"].clone();
    assert!(headers.get("x-secret").is_none(), "a removed header must not be forwarded");
}

#[tokio::test]
async fn an_api_key_plugin_injects_the_credential_upstream() {
    let harness = gateway_with(
        serde_json::json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "sharing": "private",
                "config": { "secret_ref": "cred://openai-key" },
            },
        }),
        vec![("openai-key".to_owned(), "sk-test-value".to_owned())],
    )
    .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(
        response.body["headers"].get("x-api-key").and_then(Value::as_str),
        Some("sk-test-value"),
        "the resolved secret is presented upstream"
    );
}

#[tokio::test]
async fn an_injected_credential_is_absent_from_a_response_that_does_not_echo_it() {
    let harness = gateway_with(
        serde_json::json!({
            "auth": {
                "type": common::APIKEY_PLUGIN,
                "sharing": "private",
                "config": { "secret_ref": "cred://openai-key" },
            },
        }),
        vec![("openai-key".to_owned(), "sk-test-value".to_owned())],
    )
    .await;
    let response = record(
        harness
            .serve(request("POST", "/oagw/v1/proxy/echo/post", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.raw);
    assert!(
        !response.raw.contains("sk-test-value"),
        "the credential travels upstream only: {}",
        response.raw
    );
}

#[tokio::test]
async fn an_unresolvable_credential_is_a_401() {
    let harness = gateway_with(
        serde_json::json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "sharing": "private",
                "config": { "secret_ref": "cred://missing-key" },
            },
        }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(response.body["type"], "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
    assert_eq!(response.body["status"], 401);
}

#[tokio::test]
async fn an_unknown_auth_plugin_type_is_a_401() {
    // A UUID reference passes create-time validation (the CP mints identifiers
    // for custom plugins) and is only discovered missing when a request runs.
    let harness = gateway_with(
        serde_json::json!({
            "auth": { "type": "0b7f5a3e-6f5c-4b8e-9a2d-1c3e5f7a9b01" },
        }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "an unresolvable plugin cannot authenticate the request: {}",
        response.raw
    );
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
}

#[tokio::test]
async fn a_required_headers_guard_refuses_a_request_without_them() {
    let harness = gateway_with(
        serde_json::json!({
            "plugins": {
                "items": [{
                    "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "x-correlation-id" },
                }],
            },
        }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(response.detail().contains("x-correlation-id"), "{}", response.detail());
}

#[tokio::test]
async fn a_required_headers_guard_passes_when_the_header_is_present() {
    let harness = gateway_with(
        serde_json::json!({
            "plugins": {
                "items": [{
                    "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "x-correlation-id" },
                }],
            },
        }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("x-correlation-id", "abc")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
}

#[tokio::test]
async fn a_guard_judges_the_callers_headers_not_the_rules_output() {
    // The rule set drops `x-correlation-id` before the upstream sees it; the
    // guard must still accept the request, because it judges what arrived.
    let harness = gateway_with(
        serde_json::json!({
            "headers": { "request": { "remove": ["x-correlation-id"] } },
            "plugins": {
                "items": [{
                    "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_request_headers": "x-correlation-id" },
                }],
            },
        }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("x-correlation-id", "abc")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the guard ran before the header rules");
    assert!(
        response.body["headers"].get("x-correlation-id").is_none(),
        "the rule still removed it upstream"
    );
}

#[tokio::test]
async fn a_guard_that_rejects_the_upstream_response_replaces_it_with_a_502() {
    let harness = gateway_with(
        serde_json::json!({
            "plugins": {
                "items": [{
                    "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_response_headers": "x-upstream-mark" },
                }],
            },
        }),
        Vec::new(),
    )
    .await;
    // `/status/200` on the upstream sends no `x-upstream-mark`.
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/status/200", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_GATEWAY);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
}

#[tokio::test]
async fn a_guard_that_accepts_the_upstream_response_leaves_it_alone() {
    let harness = gateway_with(
        serde_json::json!({
            "plugins": {
                "items": [{
                    "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                    "config": { "required_response_headers": "x-upstream-mark" },
                }],
            },
        }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/sse", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the upstream marks this response itself");
}

#[tokio::test]
async fn a_request_id_is_minted_when_the_caller_supplies_none() {
    let harness = gateway_with(
        serde_json::json!({ "plugins": { "items": [common::REQUEST_ID_PLUGIN] } }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let minted = response.header("x-request-id").map(str::to_owned);
    assert!(
        minted.as_ref().is_some_and(|value| !value.is_empty()),
        "a correlation id is minted and echoed to the caller"
    );
    assert_eq!(
        response.body["headers"].get("x-request-id").and_then(Value::as_str),
        minted.as_deref(),
        "the same id is presented upstream"
    );
}

#[tokio::test]
async fn a_supplied_request_id_is_propagated_and_echoed() {
    let harness = gateway_with(
        serde_json::json!({ "plugins": { "items": [common::REQUEST_ID_PLUGIN] } }),
        Vec::new(),
    )
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("x-request-id", "caller-supplied-id")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header("x-request-id"),
        Some("caller-supplied-id"),
        "the caller's id is echoed, not replaced"
    );
    assert_eq!(
        response.body["headers"].get("x-request-id").and_then(Value::as_str),
        Some("caller-supplied-id")
    );
}

#[tokio::test]
async fn an_exhausted_token_bucket_is_a_429_with_a_retry_after() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "rate_limit": {
                    "sustained": { "rate": 2, "window": "second" },
                    "burst": { "capacity": 2 },
                    "scope": "tenant",
                    "strategy": "reject",
                },
            })),
        )
        .await;
    for _ in 0..2 {
        let response = record(
            harness
                .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
                .await,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "the first two fit the bucket");
    }
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert!(
        response.header("retry-after").is_some(),
        "a rejected request is told when it may retry"
    );
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert!(
        response.body["retry_after_seconds"].as_u64().is_some(),
        "the problem carries the same budget as the header"
    );
}

#[tokio::test]
async fn a_disallowed_origin_is_a_403() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "cors": { "enabled": true, "allowed_origins": ["https://allowed.test"] },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://disallowed.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
}

#[tokio::test]
async fn an_allowed_origin_is_proxied_with_cors_headers() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .simple_upstream(
            "echo",
            Some(serde_json::json!({
                "cors": { "enabled": true, "allowed_origins": ["https://allowed.test"] },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://allowed.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header("access-control-allow-origin"), Some("https://allowed.test"));
}

#[tokio::test]
async fn a_preflight_is_answered_permissively_without_reaching_the_upstream() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request_with(
                "OPTIONS",
                "/oagw/v1/proxy/echo/echo",
                &[
                    ("origin", "https://anywhere.test"),
                    ("access-control-request-method", "GET"),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("https://anywhere.test"),
        "the preflight echo is permissive; enforcement happens on the actual request"
    );
    assert_eq!(response.header("access-control-allow-methods"), Some("GET"));
}

#[tokio::test]
async fn a_method_outside_the_route_allowlist_is_refused() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .upstream_with_route(
            "echo",
            None,
            Some(serde_json::json!({
                "match": { "http": { "methods": ["GET"], "path": "/" } },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request("PATCH", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "PATCH is not in the route's method list"
    );
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a listed method passes");
}

#[tokio::test]
async fn a_query_parameter_outside_the_allowlist_is_refused() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness
        .upstream_with_route(
            "echo",
            None,
            Some(serde_json::json!({
                "match": {
                    "http": {
                        "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                        "path": "/",
                        "query_allowlist": ["allowed"],
                    }
                },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo?unlisted=1", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{}", response.raw);
    assert!(
        response.detail().contains("unlisted"),
        "the problem names the offending parameter: {}",
        response.detail()
    );
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo?allowed=1", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the listed parameter passes: {}", response.raw);
}

#[tokio::test]
async fn a_body_over_the_configured_limit_is_a_413() {
    let harness = Harness::build(&JsonConfig::new(true, 64), Vec::new()).await.0;
    harness.simple_upstream("echo", None).await;
    let oversized = "x".repeat(128);
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/proxy/echo/echo",
                Some(serde_json::json!({ "body": oversized })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[tokio::test]
async fn a_declared_body_over_the_limit_is_refused_before_it_is_read() {
    let harness = Harness::build(&JsonConfig::new(true, 16), Vec::new()).await.0;
    harness.simple_upstream("echo", None).await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/echo/echo")
        .header("content-length", "1024")
        .body(axum::body::Body::from("short body, long declaration"))
        .unwrap();
    let response = record(harness.serve(request).await).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn the_proxy_requires_a_security_context() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve_unauthenticated(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "a proxied request needs a security context: {}",
        response.raw
    );
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
        "the refusal is the gateway's own problem document"
    );
}

#[tokio::test]
async fn a_route_bound_to_one_upstream_does_not_shadow_another() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "second",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 1 }]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let upstream_id = response.body["id"].as_str().unwrap().to_owned();
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": ["GET"], "path": "/second-only" } },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);

    // `second` matches only `/second-only`, so `/echo` still resolves to `echo`.
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn the_host_header_names_the_endpoint_the_request_is_dialled_at() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    // Two endpoints that resolve to the same loopback under different names,
    // so the `Host` the upstream saw says which one was picked.
    let port = harness.upstream_port();
    let harness = harness
        .register_upstream(
            "pool",
            serde_json::json!({
                "alias": "pool.test",
                "protocol": common::PROTOCOL_HTTP,
                "server": {
                    "endpoints": [
                        { "scheme": "http", "host": "localhost", "port": port },
                        { "scheme": "http", "host": "127.0.0.1", "port": port },
                    ]
                },
            }),
        )
        .await;
    harness.route_for("pool", "/host", None).await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/pool.test/host",
                &[("x-oagw-target-host", "127.0.0.1")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    let host = response.body["host"].as_str().unwrap_or_default();
    assert!(
        host.starts_with("127.0.0.1:"),
        "`host` names the endpoint that answered, not the pool's first member: {host}"
    );
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_from_the_gateway() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    let harness = harness
        .register_upstream(
            "shuttered",
            serde_json::json!({
                "alias": "shuttered",
                "protocol": common::PROTOCOL_HTTP,
                "enabled": false,
                "server": {
                    "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": harness.upstream_port() }]
                },
            }),
        )
        .await;
    harness.route_for("shuttered", "/", None).await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/shuttered/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        response.raw
    );
    assert_eq!(response.source(), Some(common::GATEWAY_SOURCE));
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn a_disabled_route_is_not_matched() {
    let harness = harness_without_route("dark").await;
    harness.route_for("dark", "/open", None).await;
    // Same upstream, same path band, but switched off.
    let route = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": harness.upstream_id("dark"),
                    "match": { "http": { "methods": ["GET"], "path": "/closed", "query_allowlist": [] } },
                    "enabled": false,
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(route.status, StatusCode::CREATED, "{}", route.raw);

    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/dark/closed/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "a disabled route is invisible to matching: {}",
        response.raw
    );
    assert_eq!(response.source(), Some(common::GATEWAY_SOURCE));
}

#[tokio::test]
async fn a_resolved_upstream_without_a_matching_route_is_a_404() {
    let harness = harness_without_route("routed").await;
    harness.route_for("routed", "/known", None).await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/routed/elsewhere", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the alias resolved, so this is a routing miss: {}",
        response.raw
    );
    assert_eq!(response.source(), Some(common::GATEWAY_SOURCE));
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn two_routes_with_the_same_match_conflict() {
    let harness = harness_with_upstream("clash").await;
    harness.route_for("clash", "/only", None).await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": harness.upstream_id("clash"),
                    "match": { "http": { "methods": ["GET"], "path": "/only", "query_allowlist": [] } },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CONFLICT,
        "the same path, method and priority twice is a conflict: {}",
        response.raw
    );
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.conflict.v1"
    );
}

#[tokio::test]
async fn the_longest_path_prefix_wins() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    let harness = harness
        .register_upstream(
            "deep",
            serde_json::json!({
                "alias": "deep",
                "protocol": common::PROTOCOL_HTTP,
                "server": {
                    "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": harness.upstream_port() }]
                },
            }),
        )
        .await;
    // `/` matches everything and lets any query through, so only the `/v1/chat`
    // route - which admits no query at all - can refuse one.
    harness.route_for("deep", "/", None).await;
    harness
        .route_for("deep", "/v1/chat", Some(serde_json::json!({
            "match": {
                "http": {
                    "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                    "path": "/v1/chat",
                    "query_allowlist": [],
                }
            },
        })))
        .await;

    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/deep/v1/chat/echo?q=1", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "the `/v1/chat` route is what matched, and it admits no query: {}",
        response.raw
    );
    assert_eq!(response.source(), Some(common::GATEWAY_SOURCE));
}

#[tokio::test]
async fn a_request_header_set_rule_overwrites_the_callers_value() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    let harness = harness
        .register_upstream(
            "overwritten",
            serde_json::json!({
                "alias": "overwritten",
                "protocol": common::PROTOCOL_HTTP,
                "headers": {
                    "request": {
                        "passthrough": "all",
                        "set": { "x-env": "prod" },
                    }
                },
                "server": {
                    "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": harness.upstream_port() }]
                },
            }),
        )
        .await;
    harness.route_for("overwritten", "/", None).await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/overwritten/echo",
                &[("x-env", "dev")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(
        response.body["headers"]["x-env"], "prod",
        "`set` wins over what the caller sent"
    );
}

#[tokio::test]
async fn the_target_host_header_never_reaches_the_upstream() {
    let harness = harness_with_upstream("pooltwo").await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/pooltwo/echo",
                &[("x-oagw-target-host", "127.0.0.1")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert!(
        response.body["headers"].get("x-oagw-target-host").is_none(),
        "a routing header is consumed, not forwarded: {}",
        response.raw
    );
}

#[tokio::test]
async fn an_alias_matches_regardless_of_its_case() {
    let harness = harness_with_upstream("mixedcase").await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/MixedCase/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "an alias is a case-insensitive routing key: {}",
        response.raw
    );
}

#[tokio::test]
async fn a_refused_request_never_reaches_the_upstream() {
    // One request's worth of budget: the second caller is refused.
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await;
    harness
        .upstream_with_route(
            "metered",
            Some(serde_json::json!({
                "rate_limit": {
                    "sustained": { "rate": 1, "window": "second" },
                    "burst": { "capacity": 1 },
                    "scope": "tenant",
                }
            })),
            None,
        )
        .await;

    let before = common::counted_hits();
    let admitted = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/metered/counted", None))
            .await,
    )
    .await;
    assert_eq!(admitted.status, StatusCode::OK, "{}", admitted.raw);

    // A query the route's allowlist refuses, and a caller the budget refuses:
    // neither may so much as open a connection to the upstream.
    let unknown_query = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/metered/counted?unlisted=1", None))
            .await,
    )
    .await;
    assert_eq!(unknown_query.status, StatusCode::BAD_REQUEST);
    let over_budget = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/metered/counted", None))
            .await,
    )
    .await;
    assert_eq!(over_budget.status, StatusCode::TOO_MANY_REQUESTS);

    assert_eq!(
        common::counted_hits(),
        before + 1,
        "only the admitted request was dialled"
    );
}

#[tokio::test]
async fn an_upstream_plugin_runs_before_the_route_owns_the_request() {
    // The upstream requires a header, the route carries a transform of its
    // own: both levels contribute, and the upstream's chain is what stands
    // between the caller and the route's.
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await;
    harness
        .upstream_with_route(
            "chained",
            Some(serde_json::json!({
                "plugins": {
                    "items": [{
                        "plugin_ref": common::REQUIRED_HEADERS_GUARD,
                        "config": { "required_request_headers": "x-upstream-level" },
                    }]
                }
            })),
            Some(serde_json::json!({
                "plugins": { "items": [common::REQUEST_ID_PLUGIN] }
            })),
        )
        .await;

    let without = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/chained/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        without.status,
        StatusCode::BAD_REQUEST,
        "the upstream's own guard ran even though the route binds a plugin: {}",
        without.raw
    );

    let with = record(
        harness.serve(request_with(
            "GET",
            "/oagw/v1/proxy/chained/echo",
            &[("x-upstream-level", "yes")],
        ))
        .await,
    )
    .await;
    assert_eq!(with.status, StatusCode::OK, "{}", with.raw);
    assert!(
        with.body["headers"].get("x-request-id").is_some(),
        "the route's transform ran after the upstream's guard passed it: {}",
        with.raw
    );
}

#[tokio::test]
async fn a_descendant_shadows_its_ancestors_alias() {
    // Both tenants publish the same alias, each marking its own response; the
    // closest match in the walk is the one that answers.
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let harness = Harness::build_with_hierarchy(
        &JsonConfig::new(true, 1024 * 1024),
        Vec::new(),
        &[(child, parent)],
    )
    .await
    .0;
    // Only the descendant marks its upstream: a response that carries the mark
    // was served by the descendant's own definition, one without it by the
    // ancestor's.
    for (tenant, mark) in [(parent, None), (child, Some("child"))] {
        let mut document = serde_json::json!({
            "alias": "shadow",
            "protocol": PROTOCOL_HTTP,
            "server": {
                "endpoints": [{
                    "scheme": "http",
                    "host": harness.upstream().host(),
                    "port": harness.upstream_port(),
                }]
            },
        });
        if let Some(owner) = mark {
            document["headers"] = serde_json::json!({ "response": { "set": { "x-owner": owner } } });
        }
        let created = record(
            harness
                .serve_as(
                    tenant,
                    request("POST", "/oagw/v1/upstreams", Some(document)),
                )
                .await,
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.raw);

        let route = record(
            harness
                .serve_as(
                    tenant,
                    request(
                        "POST",
                        "/oagw/v1/routes",
                        Some(serde_json::json!({
                            "upstream_id": created.body["id"],
                            "match": { "http": { "methods": ["GET"], "path": "/" } },
                        })),
                    ),
                )
                .await,
        )
        .await;
        assert_eq!(route.status, StatusCode::CREATED, "{}", route.raw);
    }

    let from_child = record(
        harness
            .serve_as(child, request("GET", "/oagw/v1/proxy/shadow/status/200", None))
            .await,
    )
    .await;
    assert_eq!(from_child.status, StatusCode::OK, "{}", from_child.raw);
    assert_eq!(
        from_child.header("x-owner"),
        Some("child"),
        "the descendant's own upstream is the closest match"
    );

    let from_parent = record(
        harness
            .serve_as(
                parent,
                request("GET", "/oagw/v1/proxy/shadow/status/200", None),
            )
            .await,
    )
    .await;
    assert_eq!(from_parent.status, StatusCode::OK, "{}", from_parent.raw);
    assert_eq!(
        from_parent.header("x-owner"),
        None,
        "the parent is served by its own unmarked upstream"
    );
}

#[tokio::test]
async fn an_ancestor_rate_limit_bounds_its_descendant() {
    // The parent publishes an enforced budget and the alias the child routes
    // to; the child's own looser figure may not loosen it.
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let harness = Harness::build_with_hierarchy(
        &JsonConfig::new(true, 1024 * 1024),
        Vec::new(),
        &[(child, parent)],
    )
    .await
    .0;
    let created = record(
        harness
            .serve_as(
                parent,
                request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(serde_json::json!({
                        "alias": "family",
                        "protocol": PROTOCOL_HTTP,
                        "server": {
                            "endpoints": [{
                                "scheme": "http",
                                "host": harness.upstream().host(),
                                "port": harness.upstream_port(),
                            }]
                        },
                        "rate_limit": {
                            "sustained": { "rate": 1, "window": "second" },
                            "burst": { "capacity": 1 },
                            "scope": "tenant",
                            "sharing": "enforce",
                        },
                    })),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.raw);
    let route = record(
        harness
            .serve_as(
                parent,
                request(
                    "POST",
                    "/oagw/v1/routes",
                    Some(serde_json::json!({
                        "upstream_id": created.body["id"],
                        "match": { "http": { "methods": ["GET"], "path": "/" } },
                    })),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(route.status, StatusCode::CREATED, "{}", route.raw);

    let first = record(
        harness
            .serve_as(child, request("GET", "/oagw/v1/proxy/family/status/200", None))
            .await,
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.raw);
    let second = record(
        harness
            .serve_as(child, request("GET", "/oagw/v1/proxy/family/status/200", None))
            .await,
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the ancestor's enforced budget is what the descendant spends: {}",
        second.raw
    );
}
