//! The proxy data plane (T029-T032): forwarding, target-host routing,
//! header transformation and inbound validation.
//!
//! Every request dials a `httpmock` stub, so what is asserted here is what
//! the gateway actually put on the wire: a mock that constrains a header only
//! matches when the gateway sent it, and `hits()` on a constrained mock is
//! the assertion of presence or absence.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use httpmock::prelude::*;
use serde_json::json;

/// A gear whose upstream dials `stub` and whose route matches `path`.
async fn gear_for(stub: &MockServer, alias: &str, path: &str, methods: &[&str]) -> Harness {
    let harness = Harness::plaintext_gear();
    let upstream_id = create_upstream(&harness, alias, "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, path, methods).await;
    harness
}

/// The proxy URL for an upstream path: the alias is the pivot and the path is
/// forwarded verbatim (`/proxy/{alias}{path}`), per ADR 0001's
/// `GET /proxy/openai/v1/chat/completions`.
fn proxied(alias: &str, path: &str) -> String {
    format!("/oagw/v1/proxy/{alias}{path}")
}

/// The `X-OAGW-Error-Source` header of a response.
fn source(response: &axum::http::Response<axum::body::Body>) -> String {
    response
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

// ── T029: the happy path ─────────────────────────────────────────────────

#[tokio::test]
async fn an_upstream_response_is_returned_verbatim() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .header("x-upstream-note", "hello")
            .body("{\"object\":\"list\"}");
    });

    let harness = gear_for(&stub, "openai", "/v1/models", &["GET"]).await;
    let response = harness
        .send(harness.proxy_request("GET", &proxied("openai", "/v1/models"), &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("x-upstream-note").and_then(|v| v.to_str().ok()),
        Some("hello"),
        "an upstream header is passed through"
    );
    assert_eq!(source(&response), "upstream", "a passthrough names its origin");
    assert_eq!(read_body(response).await, b"{\"object\":\"list\"}".to_vec());
}

#[tokio::test]
async fn a_post_body_reaches_the_upstream() {
    let stub = MockServer::start();
    let recorded = stub.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .body("{\"model\":\"x\"}");
        then.status(201).body("{\"id\":\"c1\"}");
    });

    let harness = gear_for(&stub, "chat", "/v1/chat", &["POST"]).await;
    let response = harness
        .send(harness.proxy_request(
            "POST",
            &proxied("chat", "/v1/chat"),
            &[("content-type", "application/json")],
            Some(b"{\"model\":\"x\"}".to_vec()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(read_body(response).await, b"{\"id\":\"c1\"}".to_vec());
    assert_eq!(recorded.calls(), 1, "the body the caller sent was forwarded");
}

#[tokio::test]
async fn the_query_string_is_forwarded() {
    let stub = MockServer::start();
    let recorded = stub.mock(|when, then| {
        when.method(GET).path("/v1/models").query_param_exists("limit");
        then.status(200).body("ok");
    });

    let harness = Harness::plaintext_gear();
    let upstream_id = create_upstream(&harness, "query", "127.0.0.1", stub.port(), "http").await;
    // An allowlist naming the parameter the route admits.
    let response = harness
        .send(harness.request("POST", "/oagw/v1/routes", Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/models",
                                 "query_allowlist": ["limit"], "path_suffix_mode": "append" } }
        }))))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let proxied = harness
        .send(harness.proxy_request("GET", &proxied("query", "/v1/models?limit=5"), &[], None))
        .await;
    assert_eq!(proxied.status(), StatusCode::OK);
    assert_eq!(recorded.calls(), 1, "the query parameter reached the stub");
}

#[tokio::test]
async fn an_unknown_alias_is_not_found() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/nobody/v1", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(source(&response), "gateway", "a gateway rejection names itself");
    let problem = read_json(response).await;
    assert_eq!(problem["status"], 404);
    assert!(problem["type"].as_str().unwrap_or_default().contains(".v1"), "{problem}");
}

#[tokio::test]
async fn an_unmatched_method_or_path_is_not_found() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_for(&stub, "openai", "/v1/models", &["GET"]).await;

    for (method, path) in [
        ("POST", &proxied("openai", "/v1/models")),
        ("GET", &proxied("openai", "/v1/other")),
    ] {
        let response = harness.send(harness.proxy_request(method, path, &[], None)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(source(&response), "gateway");
    }
}

// ── T030: the target-host hint ───────────────────────────────────────────

#[tokio::test]
async fn an_accurate_target_host_is_accepted() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("ok");
    });
    let harness = gear_for(&stub, "solo", "/v1", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/solo/v1",
            &[("x-oagw-target-host", "127.0.0.1")],
            None,
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&read_body(response).await)
    );
}

#[tokio::test]
async fn a_malformed_target_host_is_rejected() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("ok");
    });
    let harness = gear_for(&stub, "solo", "/v1", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/solo/v1",
            &[("x-oagw-target-host", "not a host")],
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(source(&response), "gateway");
    let problem = read_json(response).await;
    assert_eq!(problem["invalid_value"], "not a host", "{problem}");
}

#[tokio::test]
async fn an_unknown_target_host_is_rejected_with_the_valid_set() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("ok");
    });
    let harness = gear_for(&stub, "solo", "/v1", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/solo/v1",
            &[("x-oagw-target-host", "203.0.113.9")],
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = read_json(response).await;
    assert_eq!(problem["invalid_value"], "203.0.113.9", "{problem}");
    let valid = problem["valid_hosts"].as_array().cloned().unwrap_or_default();
    assert!(
        valid.iter().any(|h| h == "127.0.0.1"),
        "the configured endpoint is named: {problem}"
    );
}

#[tokio::test]
async fn the_target_host_header_is_never_forwarded() {
    // A mock that matches only when the hint is absent is the proof: if the
    // gateway forwarded it, this mock would never be hit and the request
    // would fall through to no mock at all.
    let stub = MockServer::start();
    let clean = stub.mock(|when, then| {
        when.method(GET).path("/v1").header_missing("x-oagw-target-host");
        then.status(200).body("ok");
    });

    let harness = gear_for(&stub, "solo", "/v1", &["GET"]).await;
    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/solo/v1",
            &[("x-oagw-target-host", "127.0.0.1")],
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(clean.calls(), 1, "the hint stayed at the gateway");
}

// ── T031: header transformation ──────────────────────────────────────────

#[tokio::test]
async fn hop_by_hop_headers_are_stripped() {
    let stub = MockServer::start();
    let clean = stub.mock(|when, then| {
        when.method(GET).path("/v1")
            .header_missing("proxy-authorization")
            .header_missing("connection")
            .header_missing("te")
            .header_missing("keep-alive")
            .header_missing("trailer");
        then.status(200).body("ok");
    });
    let leaky = stub.mock(|when, then| {
        when.header_exists("proxy-authorization");
        then.status(500).body("leaked");
    });

    let harness = gear_for(&stub, "hop", "/v1", &["GET"]).await;
    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/hop/v1",
            &[
                ("connection", "keep-alive"),
                ("keep-alive", "timeout=5"),
                ("proxy-authorization", "Basic zzz"),
                ("te", "trailers"),
                ("trailer", "x-checksum"),
                ("x-kept", "yes"),
            ],
            None,
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&read_body(response).await)
    );
    assert_eq!(clean.calls(), 1, "the hop-by-hop set was stripped");
    assert_eq!(leaky.calls(), 0, "no proxy credential reached the stub");
}

#[tokio::test]
async fn the_host_header_names_the_upstream() {
    let stub = MockServer::start();
    let clean = stub.mock(|when, then| {
        when.method(GET).path("/v1").header("host", "127.0.0.1");
        then.status(200).body("ok");
    });

    let harness = gear_for(&stub, "hop", "/v1", &["GET"]).await;
    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/hop/v1", &[("host", "gateway.local")], None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(clean.calls(), 1, "the caller's Host was replaced");
}

#[tokio::test]
async fn configured_request_headers_are_applied() {
    let stub = MockServer::start();
    let clean = stub.mock(|when, then| {
        when.method(GET).path("/v1")
            .header("x-oagw-set", "1")
            .header("x-oagw-add", "2")
            .header_missing("x-oagw-remove");
        then.status(200).body("ok");
    });

    let harness = Harness::plaintext_gear();
    let created = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(json!({
            "alias": "hdrs",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": stub.port() } ] },
            "protocol": "http",
            "headers": {
                "request": {
                    "set": { "x-oagw-set": "1" },
                    "add": { "x-oagw-add": "2" },
                    "remove": [ "x-oagw-remove" ]
                }
            }
        }))))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream_id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    create_route(&harness, &upstream_id, "/v1", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/hdrs/v1",
            &[("x-oagw-remove", "gone")],
            None,
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&read_body(response).await)
    );
    assert_eq!(clean.calls(), 1, "set/add/remove all took effect");
}

#[tokio::test]
async fn configured_response_headers_are_applied() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("ok");
    });

    let harness = Harness::plaintext_gear();
    let created = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(json!({
            "alias": "resp",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": stub.port() } ] },
            "protocol": "http",
            "headers": {
                "response": {
                    "set": { "x-gateway-note": "oagw" },
                    "remove": [ "x-upstream-note" ]
                }
            }
        }))))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream_id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    create_route(&harness, &upstream_id, "/v1", &["GET"]).await;

    // The stub's own header is added by a second mock so the removal has a
    // target.
    stub.mock(|when, then| {
        when.method(GET).path("/v1").header_exists("x-unused");
        then.status(200).header("x-upstream-note", "drop me").body("ok");
    });

    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/resp/v1", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("x-gateway-note").and_then(|v| v.to_str().ok()),
        Some("oagw"),
        "a response header is set"
    );
    assert!(
        headers.get("x-upstream-note").is_none(),
        "a removed upstream header is gone"
    );
}

#[tokio::test]
async fn a_passthrough_header_policy_forwards_only_the_allowlist() {
    let stub = MockServer::start();
    let clean = stub.mock(|when, then| {
        when.method(GET).path("/v1").header_exists("x-allowed");
        then.status(200).body("ok");
    });
    let leaky = stub.mock(|when, then| {
        when.header_exists("x-secret");
        then.status(500).body("leaked");
    });

    let harness = Harness::plaintext_gear();
    let created = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(json!({
            "alias": "allow",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": stub.port() } ] },
            "protocol": "http",
            "headers": { "request": { "passthrough": "allowlist",
                                       "passthrough_allowlist": ["x-allowed"] } }
        }))))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream_id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    create_route(&harness, &upstream_id, "/v1", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/allow/v1",
            &[("x-allowed", "yes"), ("x-forbidden", "no")],
            None,
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&read_body(response).await)
    );
    assert_eq!(clean.calls(), 1, "the allowed header was forwarded");
    assert_eq!(leaky.calls(), 0, "the forbidden header was not");
}

// ── T032: inbound validation ─────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_query_parameter_is_rejected_with_an_empty_allowlist() {
    let stub = MockServer::start();
    let recorded = stub.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("ok");
    });
    let harness = gear_for(&stub, "q", "/v1", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/q/v1?debug=1", &[], None))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an empty allowlist admits nothing"
    );
    assert_eq!(recorded.calls(), 0, "nothing was dialled");
}

#[tokio::test]
async fn a_suffix_is_rejected_when_the_route_disables_it() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/exact");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let upstream_id = create_upstream(&harness, "exact", "127.0.0.1", stub.port(), "http").await;
    let created = harness
        .send(harness.request("POST", "/oagw/v1/routes", Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/exact", "path_suffix_mode": "disabled" } }
        }))))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let proxy = harness
        .send(harness.proxy_request(
            "GET",
            &proxied("exact", "/v1/exact/deeper"),
            &[],
            None,
        ))
        .await;
    assert_eq!(proxy.status(), StatusCode::BAD_REQUEST, "a suffix is not allowed here");

    let exact = harness
        .send(harness.proxy_request("GET", &proxied("exact", "/v1/exact"), &[], None))
        .await;
    assert_eq!(exact.status(), StatusCode::OK, "the route path itself is allowed");
}

#[tokio::test]
async fn an_unroutable_alias_without_routes_is_not_found() {
    let stub = MockServer::start();
    let recorded = stub.mock(|when, then| {
        when.method(GET);
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    create_upstream(&harness, "noroutes", "127.0.0.1", stub.port(), "http").await;

    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/noroutes/v1", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "no route matches");
    assert_eq!(recorded.calls(), 0);
}


// ── ADR 0009: configured plugin bindings (T075-T077) ─────────────────────

/// The GTS id of the built-in required-headers guard.
const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// A catalog-only guard identifier: known to the types registry, not bindable.
const CATALOG_ONLY: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";

/// Creates the upstream + route pair the binding tests proxy through.
async fn bound_gear(stub: &MockServer, alias: &str, plugins: serde_json::Value) -> Harness {
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body(alias, "127.0.0.1", stub.port(), "http");
    body["plugins"] = plugins;
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "upstream accepted");
    let upstream_id = read_json(response).await["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;
    harness
}

#[tokio::test]
async fn a_binding_object_configures_the_guard() {
    // ADR 0009 binds `{plugin_ref, config}`: the response half of the guard
    // must see the configuration the binding carries and refuse an upstream
    // response that omits it.
    let stub = MockServer::start();
    let missing = stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("{}");
    });
    let harness = bound_gear(
        &stub,
        "bound",
        json!({ "items": [
            { "plugin_ref": REQUIRED_HEADERS, "config": { "required_response_headers": "x-upstream-note" } }
        ]}),
    )
    .await;

    let response = harness
        .send(harness.proxy_request("GET", &proxied("bound", "/v1/models"), &[], None))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "the upstream omitted the required response header"
    );
    assert_eq!(source(&response), "gateway");
    assert_eq!(missing.calls(), 1, "the response was refused after the call");
}

#[tokio::test]
async fn a_configured_response_guard_allows_a_compliant_response() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).header("x-upstream-note", "present").body("{}");
    });
    let harness = bound_gear(
        &stub,
        "compliant",
        json!({ "items": [
            { "plugin_ref": REQUIRED_HEADERS, "config": { "required_response_headers": "x-upstream-note" } }
        ]}),
    )
    .await;

    let response = harness
        .send(harness.proxy_request("GET", &proxied("compliant", "/v1/models"), &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "the header is present");
}

#[tokio::test]
async fn the_request_half_of_the_bound_guard_is_enforced() {
    let stub = MockServer::start();
    let models = stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("{}");
    });
    let harness = bound_gear(
        &stub,
        "correlated",
        json!({ "items": [
            { "plugin_ref": REQUIRED_HEADERS, "config": { "required_request_headers": "x-correlation-id" } }
        ]}),
    )
    .await;

    let refused = harness
        .send(harness.proxy_request("GET", &proxied("correlated", "/v1/models"), &[], None))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "the header is missing");
    assert_eq!(models.calls(), 0, "a refused request never reaches the upstream");

    let allowed = harness
        .send(harness.proxy_request(
            "GET",
            &proxied("correlated", "/v1/models"),
            &[("x-correlation-id", "abc")],
            None,
        ))
        .await;
    assert_eq!(allowed.status(), StatusCode::OK, "the header is present");
}

#[tokio::test]
async fn a_catalog_only_identifier_is_not_bindable() {
    let stub = MockServer::start();
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("captive", "127.0.0.1", stub.port(), "http");
    body["plugins"] = json!({ "items": [CATALOG_ONLY] });
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a catalog-only guard exists for types-registry cataloging only"
    );
}

#[tokio::test]
async fn an_unknown_plugin_uuid_is_not_bindable() {
    let stub = MockServer::start();
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("ghosted", "127.0.0.1", stub.port(), "http");
    body["plugins"] = json!({ "items": [uuid::Uuid::new_v4().to_string()] });
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the plugin the binding names does not exist"
    );
}

// ---------------------------------------------------------------------------
// Body validation (DESIGN §"Body Validation Rules")
// ---------------------------------------------------------------------------

/// A plaintext gear whose request-body cap is small enough to test cheaply.
fn capped_gear(bytes: usize) -> Harness {
    let mut config = Harness::plaintext_config();
    config.max_body_bytes = bytes;
    Harness::new(config)
}

#[tokio::test]
async fn a_body_over_the_hard_limit_is_a_413_not_a_protocol_error() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = capped_gear(16);
    let upstream_id = create_upstream(&harness, "capped", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["POST"]).await;

    let response = harness
        .send(harness.proxy_request(
            "POST",
            &proxied("capped", "/v1/models"),
            &[("content-length", "32")],
            Some(vec![b'x'; 32]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(source(&response), "gateway");
    let body = read_json(response).await;
    assert_eq!(
        body["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1")
    );
    assert_eq!(body["status"], json!(413));
}

#[tokio::test]
async fn a_content_length_that_disagrees_with_the_body_is_a_400() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = capped_gear(1024);
    let upstream_id = create_upstream(&harness, "sized", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["POST"]).await;

    let response = harness
        .send(harness.proxy_request(
            "POST",
            &proxied("sized", "/v1/models"),
            &[("content-length", "9")],
            Some(b"0123456789".to_vec()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(source(&response), "gateway");
    let body = read_json(response).await;
    assert_eq!(
        body["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
    );
}

#[tokio::test]
async fn an_unparseable_content_length_is_a_400_before_the_body_is_read() {
    let stub = MockServer::start();
    let harness = capped_gear(64);
    let upstream_id = create_upstream(&harness, "garbled", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["POST"]).await;

    let response = harness
        .send(harness.proxy_request(
            "POST",
            &proxied("garbled", "/v1/models"),
            &[("content-length", "not-a-number")],
            Some(b"0123456789".to_vec()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_json(response).await["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
    );
}

#[tokio::test]
async fn an_unsupported_transfer_encoding_is_a_400() {
    let stub = MockServer::start();
    let harness = capped_gear(64);
    let upstream_id = create_upstream(&harness, "encoded", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["POST"]).await;

    let response = harness
        .send(harness.proxy_request(
            "POST",
            &proxied("encoded", "/v1/models"),
            &[("transfer-encoding", "gzip, chunked")],
            Some(b"4\r\nwiki\r\n0\r\n\r\n".to_vec()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_json(response).await["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
    );
}

#[tokio::test]
async fn a_chunked_body_is_accepted() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(POST).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = capped_gear(64);
    let upstream_id = create_upstream(&harness, "chunked", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["POST"]).await;

    let response = harness
        .send(harness.proxy_request(
            "POST",
            &proxied("chunked", "/v1/models"),
            &[("transfer-encoding", "chunked")],
            Some(b"4\r\nwiki\r\n0\r\n\r\n".to_vec()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_plaintext_dial_is_refused_when_the_posture_forbids_it() {
    // FR-005: `http` is a legal scheme value, but whether a plaintext
    // connection is actually made is governed by the same permission. The
    // default posture is HTTPS-only, so the gateway refuses the dial instead
    // of opening an unencrypted connection.
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::default_gear();
    let upstream_id = create_upstream(&harness, "cleartext", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request("GET", &proxied("cleartext", "/v1/models"), &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(source(&response), "gateway");
    let body = read_json(response).await;
    assert_eq!(
        body["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
    );
    assert!(
        !serde_json::to_string(&body).unwrap_or_default().contains("ok"),
        "the upstream is never dialled, so its response body never appears"
    );
}
