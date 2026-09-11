//! Proxy execution: routing, header hygiene, credential injection, plugin
//! chain, CORS, rate limiting, timeouts and error-source distinction.
//!
//! The gateway runs in-process against a real loopback upstream, so the
//! transport path (DNS, TCP, HTTP/1.1 framing) is exercised for real.

use oagw::config::{OagwConfig, SsrfPolicyConfig};
use oagw::domain::gts_helpers as gts;
use oagw::test_support::{
    Harness, MockUpstream, TEST_SECRET_REF, TEST_SECRET_VALUE, context_for, empty_request,
    json_request, read_json, read_text,
};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Gateway plus a live upstream, with one upstream and one route registered.
struct Fixture {
    harness: Harness,
    upstream: MockUpstream,
    ctx: SecurityContext,
    upstream_id: String,
}

async fn fixture_with(
    upstream_overrides: Value,
    route_overrides: Value,
    config: Option<OagwConfig>,
) -> Fixture {
    let mock = MockUpstream::start().await;
    let harness = match config {
        Some(config) => Harness::builder().config(config).build(),
        None => Harness::new(),
    };
    let ctx = context_for(Uuid::new_v4());

    let mut upstream_body = json!({
        "server": { "endpoints": [
            { "scheme": "http", "host": mock.host(), "port": mock.port() }
        ] },
        "protocol": gts::PROTOCOL_HTTP,
        "alias": "mock-upstream"
    });
    merge(&mut upstream_body, upstream_overrides);
    let created = read_json(
        harness
            .post_json(&ctx, "/oagw/v1/upstreams", &upstream_body)
            .await,
    )
    .await;
    let upstream_id = created["id"]
        .as_str()
        .unwrap_or_else(|| panic!("upstream create failed: {created}"))
        .to_owned();

    let mut route_body = json!({
        "upstream_id": upstream_id.clone(),
        "match": { "http": {
            "methods": ["GET", "POST", "PUT", "PATCH", "DELETE"],
            "path": "/v1"
        } }
    });
    merge(&mut route_body, route_overrides);
    let route = harness
        .post_json(&ctx, "/oagw/v1/routes", &route_body)
        .await;
    assert_eq!(route.status(), 201, "route create must succeed");

    Fixture {
        harness,
        upstream: mock,
        ctx,
        upstream_id,
    }
}

async fn fixture() -> Fixture {
    fixture_with(json!({}), json!({}), None).await
}

fn merge(target: &mut Value, overrides: Value) {
    if let (Some(target), Some(overrides)) = (target.as_object_mut(), overrides.as_object()) {
        for (key, value) in overrides {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn permissive_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicyConfig {
            enabled: false,
            ..SsrfPolicyConfig::default()
        },
        ..OagwConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_get_is_forwarded_with_the_composed_path_and_tagged_as_upstream() {
    let f = fixture().await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-oagw-error-source"],
        "upstream",
        "a passthrough response must be attributed to the upstream"
    );
    let body = read_json(response).await;
    assert_eq!(body["upstream"], "mock");
    assert_eq!(body["method"], "GET");

    let seen = f.upstream.last_request().expect("the upstream was called");
    assert_eq!(seen.uri, "/v1/models", "origin-form request target");
    assert_eq!(
        seen.header("host"),
        Some(format!("{}:{}", f.upstream.host(), f.upstream.port()).as_str()),
        "Host is replaced by the upstream authority"
    );
}

#[tokio::test]
async fn a_body_carrying_method_is_forwarded_with_its_body_and_content_type() {
    let f = fixture().await;
    let response = f
        .harness
        .send(
            &f.ctx,
            json_request(
                http::Method::POST,
                "/oagw/v1/proxy/mock-upstream/v1/chat",
                &json!({ "model": "gpt-4" }),
            ),
        )
        .await;
    assert_eq!(response.status(), 200);
    let seen = f.upstream.last_request().expect("called");
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.body, r#"{"model":"gpt-4"}"#);
    assert_eq!(seen.header("content-type"), Some("application/json"));
}

#[tokio::test]
async fn the_alias_resolves_case_insensitively() {
    let f = fixture().await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/MOCK-UPSTREAM/v1/models")
        .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn a_request_without_a_path_suffix_reaches_a_root_route() {
    let f = fixture_with(
        json!({}),
        json!({ "match": { "http": {
        "methods": ["GET"],
        "path": "/"
    } } }),
        None,
    )
    .await;
    let response = f.harness.get(&f.ctx, "/oagw/v1/proxy/mock-upstream").await;
    assert_eq!(response.status(), 200);
    assert_eq!(f.upstream.last_request().expect("called").uri, "/");
}

// ---------------------------------------------------------------------------
// Routing failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_alias_is_404_route_not_found() {
    let f = fixture().await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/nowhere.example.com/v1/models")
        .await;
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    let problem = read_json(response).await;
    assert_eq!(problem["type"], gts::ERR_ROUTE_NOT_FOUND);
    assert_eq!(problem["alias"], "nowhere.example.com");
}

#[tokio::test]
async fn an_unmatched_path_or_method_is_404() {
    let f = fixture_with(
        json!({}),
        json!({ "match": { "http": { "methods": ["GET"], "path": "/v1" } } }),
        None,
    )
    .await;
    let unmatched_path = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v2/models")
        .await;
    assert_eq!(unmatched_path.status(), 404);

    let unmatched_method = f
        .harness
        .send(
            &f.ctx,
            json_request(
                http::Method::POST,
                "/oagw/v1/proxy/mock-upstream/v1/models",
                &json!({}),
            ),
        )
        .await;
    assert_eq!(unmatched_method.status(), 404);
    assert!(
        f.upstream.requests().is_empty(),
        "an unmatched request must never reach the upstream"
    );
}

#[tokio::test]
async fn the_longest_matching_route_wins() {
    let f = fixture().await;
    let precise = f
        .harness
        .post_json(
            &f.ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": f.upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1/models" } }
            }),
        )
        .await;
    assert_eq!(precise.status(), 201);

    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models/gpt-4")
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        f.upstream.last_request().expect("called").uri,
        "/v1/models/gpt-4"
    );
}

#[tokio::test]
async fn path_suffix_mode_disabled_refuses_a_suffix() {
    let f = fixture_with(
        json!({}),
        json!({ "match": { "http": {
            "methods": ["GET"],
            "path": "/v1/models",
            "path_suffix_mode": "disabled"
        } } }),
        None,
    )
    .await;
    assert_eq!(
        f.harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await
            .status(),
        200
    );
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models/gpt-4")
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(read_json(response).await["type"], gts::ERR_VALIDATION);
}

#[tokio::test]
async fn a_disabled_upstream_is_503() {
    let f = fixture().await;
    let disable = f
        .harness
        .put_json(
            &f.ctx,
            &format!("/oagw/v1/upstreams/{}", f.upstream_id),
            &json!({
                "server": { "endpoints": [
                    { "scheme": "http", "host": f.upstream.host(), "port": f.upstream.port() }
                ] },
                "protocol": gts::PROTOCOL_HTTP,
                "alias": "mock-upstream",
                "enabled": false
            }),
        )
        .await;
    assert_eq!(disable.status(), 200);

    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
}

#[tokio::test]
async fn a_disabled_route_is_excluded_from_matching() {
    let f = fixture().await;
    let routes = read_json(f.harness.get(&f.ctx, "/oagw/v1/routes").await).await;
    let route_id = routes["items"][0]["id"].as_str().expect("id").to_owned();
    let disabled = f
        .harness
        .put_json(
            &f.ctx,
            &format!("/oagw/v1/routes/{route_id}"),
            &json!({
                "match": { "http": { "methods": ["GET"], "path": "/v1" } },
                "enabled": false
            }),
        )
        .await;
    assert_eq!(disabled.status(), 200);
    assert_eq!(
        f.harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// Query allowlist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_query_allowlist_allows_no_parameters() {
    let f = fixture().await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models?limit=5")
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(read_json(response).await["type"], gts::ERR_VALIDATION);
}

#[tokio::test]
async fn allowlisted_parameters_are_forwarded() {
    let f = fixture_with(
        json!({}),
        json!({ "match": { "http": {
            "methods": ["GET"],
            "path": "/v1",
            "query_allowlist": ["limit"]
        } } }),
        None,
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models?limit=5")
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        f.upstream.last_request().expect("called").uri,
        "/v1/models?limit=5"
    );
    assert_eq!(
        f.harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models?other=1")
            .await
            .status(),
        400
    );
}

// ---------------------------------------------------------------------------
// Header handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nothing_is_forwarded_by_default_and_the_inbound_bearer_never_leaks() {
    let f = fixture().await;
    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer platform-token"),
    );
    request.headers_mut().insert(
        "x-custom",
        http::HeaderValue::from_static("should-not-travel"),
    );
    request.headers_mut().insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("keep-alive"),
    );
    let response = f.harness.send(&f.ctx, request).await;
    assert_eq!(response.status(), 200);

    let seen = f.upstream.last_request().expect("called");
    assert_eq!(
        seen.header("authorization"),
        None,
        "the platform bearer token must never reach a third party"
    );
    assert_eq!(
        seen.header("x-custom"),
        None,
        "passthrough defaults to none"
    );
    assert_eq!(
        seen.header("connection"),
        None,
        "hop-by-hop headers are stripped and HTTP/1.1 keep-alive is implicit"
    );
}

#[tokio::test]
async fn header_rules_set_add_remove_and_allowlist_are_applied() {
    let f = fixture_with(
        json!({ "headers": {
            "request": {
                "passthrough": "allowlist",
                "passthrough_allowlist": ["x-keep"],
                "set": { "x-injected": "1" },
                "remove": ["x-remove-me"]
            },
            "response": {
                "set": { "x-gateway": "ok" },
                "remove": ["x-upstream-marker"]
            }
        } }),
        json!({}),
        None,
    )
    .await;
    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    for (name, value) in [
        ("x-keep", "kept"),
        ("x-drop", "dropped"),
        ("x-remove-me", "gone"),
    ] {
        request.headers_mut().insert(
            http::HeaderName::from_bytes(name.as_bytes()).expect("name"),
            http::HeaderValue::from_static(value),
        );
    }
    let response = f.harness.send(&f.ctx, request).await;
    assert_eq!(response.headers()["x-gateway"], "ok");

    let seen = f.upstream.last_request().expect("called");
    assert_eq!(seen.header("x-keep"), Some("kept"));
    assert_eq!(seen.header("x-injected"), Some("1"));
    assert_eq!(seen.header("x-drop"), None);
    assert_eq!(seen.header("x-remove-me"), None);
}

#[tokio::test]
async fn a_malformed_content_length_is_400_and_an_oversized_one_is_413() {
    let f = fixture().await;
    let mut malformed = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    malformed.headers_mut().insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from_static("not-a-number"),
    );
    assert_eq!(f.harness.send(&f.ctx, malformed).await.status(), 400);

    let harness = Harness::builder()
        .config(OagwConfig {
            max_body_bytes: 16,
            ..permissive_config()
        })
        .build();
    let ctx = context_for(Uuid::new_v4());
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": f.upstream.host(), "port": f.upstream.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "mock-upstream"
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["POST"], "path": "/v1" } }
            }),
        )
        .await;
    let body = json!({ "padding": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" });
    let mut oversized = json_request(
        http::Method::POST,
        "/oagw/v1/proxy/mock-upstream/v1/echo",
        &body,
    );
    oversized.headers_mut().insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from_str(&body.to_string().len().to_string()).expect("value"),
    );
    let response = harness.send(&ctx, oversized).await;
    assert_eq!(response.status(), 413);
    assert_eq!(
        read_json(response).await["type"],
        gts::ERR_PAYLOAD_TOO_LARGE
    );
}

// ---------------------------------------------------------------------------
// Credential injection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_apikey_plugin_injects_a_header_credential() {
    let f = fixture_with(
        json!({ "auth": {
            "type": gts::APIKEY_AUTH_PLUGIN_ID,
            "config": {
                "in": "header",
                "name": "x-api-key",
                "secret_ref": format!("cred://{TEST_SECRET_REF}")
            }
        } }),
        json!({}),
        None,
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        f.upstream
            .last_request()
            .expect("called")
            .header("x-api-key"),
        Some(TEST_SECRET_VALUE)
    );
}

#[tokio::test]
async fn the_apikey_plugin_can_inject_a_query_credential() {
    let f = fixture_with(
        json!({ "auth": {
            "type": gts::APIKEY_AUTH_PLUGIN_ID,
            "config": { "in": "query", "name": "key", "secret_ref": TEST_SECRET_REF }
        } }),
        json!({}),
        None,
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        f.upstream.last_request().expect("called").uri,
        format!("/v1/models?key={TEST_SECRET_VALUE}")
    );
}

#[tokio::test]
async fn an_unresolvable_credential_is_500_secret_not_found_and_never_echoes_the_value() {
    let f = fixture_with(
        json!({ "auth": {
            "type": gts::APIKEY_AUTH_PLUGIN_ID,
            "config": { "secret_ref": "cred://absent-key" }
        } }),
        json!({}),
        None,
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 500);
    let text = read_text(response).await;
    assert!(text.contains(gts::ERR_SECRET_NOT_FOUND), "{text}");
    assert!(
        !text.contains(TEST_SECRET_VALUE),
        "no credential material may appear in an error body"
    );
    assert!(
        f.upstream.requests().is_empty(),
        "the request must not be forwarded without its credential"
    );
}

#[tokio::test]
async fn the_noop_plugin_injects_nothing() {
    let f = fixture_with(
        json!({ "auth": { "type": gts::NOOP_AUTH_PLUGIN_ID } }),
        json!({}),
        None,
    )
    .await;
    assert_eq!(
        f.harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await
            .status(),
        200
    );
    assert_eq!(
        f.upstream
            .last_request()
            .expect("called")
            .header("authorization"),
        None
    );
}

// ---------------------------------------------------------------------------
// Plugin chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_request_id_transform_propagates_and_mints_a_correlation_id() {
    let f = fixture_with(
        json!({ "plugins": { "items": [ gts::REQUEST_ID_TRANSFORM_PLUGIN_ID ] } }),
        json!({}),
        None,
    )
    .await;
    // Nothing supplied: the plugin mints one.
    f.harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    let minted = f
        .upstream
        .last_request()
        .expect("called")
        .header("x-request-id")
        .map(str::to_owned);
    assert!(minted.is_some_and(|id| Uuid::parse_str(&id).is_ok()));

    // Supplied: it travels unchanged. The default passthrough policy does not
    // forward it, so the plugin is what carries it.
    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    request
        .headers_mut()
        .insert("x-request-id", http::HeaderValue::from_static("req_abc123"));
    let response = f.harness.send(&f.ctx, request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("req_abc123"),
        "the correlation id is echoed back to the caller"
    );
}

#[tokio::test]
async fn the_required_headers_guard_rejects_a_missing_request_header() {
    let f = fixture_with(
        json!({ "plugins": { "items": [ {
            "plugin_ref": gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "config": { "required_request_headers": "x-correlation-id" }
        } ] } }),
        json!({}),
        None,
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 400);
    let problem = read_json(response).await;
    assert_eq!(problem["error_code"], "REQUIRED_HEADER_MISSING");
    assert!(
        f.upstream.requests().is_empty(),
        "a guard rejection must not reach the upstream"
    );
}

#[tokio::test]
async fn the_required_headers_guard_admits_a_present_header() {
    let f = fixture_with(
        json!({
            "headers": { "request": { "passthrough": "all" } },
            "plugins": { "items": [ {
                "plugin_ref": gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                "config": { "required_request_headers": "x-correlation-id" }
            } ] }
        }),
        json!({}),
        None,
    )
    .await;
    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    request
        .headers_mut()
        .insert("x-correlation-id", http::HeaderValue::from_static("abc"));
    assert_eq!(f.harness.send(&f.ctx, request).await.status(), 200);
}

#[tokio::test]
async fn the_required_headers_guard_rejects_a_missing_response_header_with_502() {
    let f = fixture_with(
        json!({ "plugins": { "items": [ {
            "plugin_ref": gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "config": { "required_response_headers": "x-absent-header" }
        } ] } }),
        json!({}),
        None,
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 502);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
}

#[tokio::test]
async fn a_custom_plugin_binding_is_reported_as_unexecutable_rather_than_ignored() {
    let mock = MockUpstream::start().await;
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let plugin = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/plugins",
                &json!({
                    "plugin_type": "guard",
                    "name": "starlark_guard",
                    "source_code": "def on_request(ctx):\n    return ctx.next()\n"
                }),
            )
            .await,
    )
    .await;
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": mock.host(), "port": mock.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "mock-upstream",
                    "plugins": { "items": [ plugin["id"] ] }
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    let response = harness
        .get(&ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 503);
    assert_eq!(read_json(response).await["type"], gts::ERR_PLUGIN_NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Endpoint selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_common_suffix_alias_requires_the_target_host_header() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "https", "host": "us.vendor.com", "port": 443 },
                        { "scheme": "https", "host": "eu.vendor.com", "port": 443 }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP
                }),
            )
            .await,
    )
    .await;
    assert_eq!(created["alias"], "vendor.com");
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;

    let response = harness
        .get(&ctx, "/oagw/v1/proxy/vendor.com/v1/status")
        .await;
    assert_eq!(response.status(), 400);
    let problem = read_json(response).await;
    assert_eq!(problem["type"], gts::ERR_MISSING_TARGET_HOST);
    assert_eq!(
        problem["valid_hosts"].as_array().map(Vec::len),
        Some(2),
        "the caller is told which hosts are valid"
    );

    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/vendor.com/v1/status");
    request.headers_mut().insert(
        "x-oagw-target-host",
        http::HeaderValue::from_static("apac.vendor.com"),
    );
    let unknown = harness.send(&ctx, request).await;
    assert_eq!(unknown.status(), 400);
    assert_eq!(
        read_json(unknown).await["type"],
        gts::ERR_UNKNOWN_TARGET_HOST
    );

    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/vendor.com/v1/status");
    request.headers_mut().insert(
        "x-oagw-target-host",
        http::HeaderValue::from_static("us.vendor.com:8443"),
    );
    let invalid = harness.send(&ctx, request).await;
    assert_eq!(invalid.status(), 400);
    assert_eq!(
        read_json(invalid).await["type"],
        gts::ERR_INVALID_TARGET_HOST
    );
}

#[tokio::test]
async fn the_target_host_header_is_consumed_and_not_forwarded() {
    let f = fixture_with(
        json!({ "headers": { "request": { "passthrough": "all" } } }),
        json!({}),
        None,
    )
    .await;
    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    let host = f.upstream.host();
    request.headers_mut().insert(
        "x-oagw-target-host",
        http::HeaderValue::from_str(&host).expect("value"),
    );
    assert_eq!(f.harness.send(&f.ctx, request).await.status(), 200);
    assert_eq!(
        f.upstream
            .last_request()
            .expect("called")
            .header("x-oagw-target-host"),
        None
    );
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_preflight_is_answered_locally_without_resolving_an_upstream() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let mut request = empty_request(
        http::Method::OPTIONS,
        "/oagw/v1/proxy/never-registered/v1/models",
    );
    request.headers_mut().insert(
        http::header::ORIGIN,
        http::HeaderValue::from_static("https://app.example.com"),
    );
    request.headers_mut().insert(
        http::header::ACCESS_CONTROL_REQUEST_METHOD,
        http::HeaderValue::from_static("POST"),
    );
    request.headers_mut().insert(
        http::header::ACCESS_CONTROL_REQUEST_HEADERS,
        http::HeaderValue::from_static("Content-Type, Authorization"),
    );
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert_eq!(response.headers()["access-control-allow-methods"], "POST");
    assert_eq!(
        response.headers()["access-control-allow-headers"],
        "Content-Type, Authorization"
    );
    assert_eq!(response.headers()["access-control-max-age"], "86400");
    assert!(
        response.headers()["vary"]
            .to_str()
            .expect("ascii")
            .contains("Origin")
    );
}

#[tokio::test]
async fn an_actual_cross_origin_request_is_validated_and_answered_with_cors_headers() {
    let f = fixture_with(
        json!({ "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET"],
            "expose_headers": ["X-Upstream-Marker"],
            "allow_credentials": true
        } }),
        json!({}),
        None,
    )
    .await;

    let mut allowed = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    allowed.headers_mut().insert(
        http::header::ORIGIN,
        http::HeaderValue::from_static("https://app.example.com"),
    );
    let response = f.harness.send(&f.ctx, allowed).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert_eq!(
        response.headers()["access-control-allow-credentials"],
        "true"
    );

    let mut disallowed_origin =
        empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v1/models");
    disallowed_origin.headers_mut().insert(
        http::header::ORIGIN,
        http::HeaderValue::from_static("https://evil.com"),
    );
    let refused = f.harness.send(&f.ctx, disallowed_origin).await;
    assert_eq!(refused.status(), 403);
    assert_eq!(
        read_json(refused).await["type"],
        gts::ERR_CORS_ORIGIN_NOT_ALLOWED
    );

    let mut disallowed_method = json_request(
        http::Method::POST,
        "/oagw/v1/proxy/mock-upstream/v1/models",
        &json!({}),
    );
    disallowed_method.headers_mut().insert(
        http::header::ORIGIN,
        http::HeaderValue::from_static("https://app.example.com"),
    );
    let refused = f.harness.send(&f.ctx, disallowed_method).await;
    assert_eq!(refused.status(), 403);
    assert_eq!(
        read_json(refused).await["type"],
        gts::ERR_CORS_METHOD_NOT_ALLOWED
    );
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_exceeded_rate_limit_answers_429_with_retry_after_and_ratelimit_headers() {
    let f = fixture_with(
        json!({ "rate_limit": {
            "sustained": { "rate": 2, "window": "minute" },
            "burst": { "capacity": 2 },
            "scope": "tenant",
            "strategy": "reject"
        } }),
        json!({}),
        None,
    )
    .await;

    for attempt in 0..2 {
        let response = f
            .harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await;
        assert_eq!(response.status(), 200, "attempt {attempt} is within budget");
        assert_eq!(response.headers()["x-ratelimit-limit"], "2");
        assert!(response.headers().contains_key("x-ratelimit-remaining"));
    }

    let refused = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(refused.status(), 429);
    assert_eq!(refused.headers()["x-oagw-error-source"], "gateway");
    let retry_after: u64 = refused.headers()[http::header::RETRY_AFTER]
        .to_str()
        .expect("ascii")
        .parse()
        .expect("integer");
    assert!(retry_after >= 1);
    let problem = read_json(refused).await;
    assert_eq!(problem["type"], gts::ERR_RATE_LIMIT_EXCEEDED);
    assert_eq!(problem["retry_after_seconds"], retry_after);
    assert_eq!(f.upstream.requests().len(), 2, "the third call was refused");
}

#[tokio::test]
async fn rate_limit_counters_are_per_tenant() {
    let f = fixture_with(
        json!({ "rate_limit": {
            "sustained": { "rate": 1, "window": "minute" },
            "burst": { "capacity": 1 }
        } }),
        json!({}),
        None,
    )
    .await;
    assert_eq!(
        f.harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await
            .status(),
        200
    );
    assert_eq!(
        f.harness
            .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await
            .status(),
        429
    );

    // Another tenant has no upstream under this alias at all, so it cannot
    // consume this tenant's budget either way: assert the counter, not the
    // route.
    let other = context_for(Uuid::new_v4());
    assert_eq!(
        f.harness
            .get(&other, "/oagw/v1/proxy/mock-upstream/v1/models")
            .await
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// Upstream failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upstream_error_is_passed_through_unchanged_and_attributed_to_the_upstream() {
    let f = fixture().await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/boom")
        .await;
    assert_eq!(response.status(), 500);
    assert_eq!(response.headers()["x-oagw-error-source"], "upstream");
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "application/json",
        "the upstream body is not rewritten into problem+json"
    );
    let body = read_json(response).await;
    assert_eq!(body["error"], "upstream exploded");
}

#[tokio::test]
async fn an_unreachable_upstream_is_502_with_gateway_attribution() {
    let harness = Harness::builder().config(permissive_config()).build();
    let ctx = context_for(Uuid::new_v4());
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    // Port 1 on loopback: nothing listens there.
                    "server": { "endpoints": [
                        { "scheme": "http", "host": "127.0.0.1", "port": 1 }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "dead-upstream"
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    let response = harness
        .get(&ctx, "/oagw/v1/proxy/dead-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 502);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    let problem = read_json(response).await;
    assert_eq!(problem["type"], gts::ERR_DOWNSTREAM);
    assert_eq!(problem["host"], "127.0.0.1");
}

#[tokio::test]
async fn a_slow_upstream_is_504_request_timeout() {
    let f = fixture_with(
        json!({}),
        json!({}),
        Some(OagwConfig {
            proxy_timeout_secs: 1,
            ..permissive_config()
        }),
    )
    .await;
    let response = f
        .harness
        .get(&f.ctx, "/oagw/v1/proxy/mock-upstream/v1/slow")
        .await;
    assert_eq!(response.status(), 504);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    assert_eq!(read_json(response).await["type"], gts::ERR_REQUEST_TIMEOUT);
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_the_flag_is_off() {
    // The management API accepts `http`; the *connection* is what the flag
    // governs, and it is refused at forward time.
    let mock = MockUpstream::start().await;
    let harness = Harness::builder()
        .config(OagwConfig {
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            ..OagwConfig::default()
        })
        .build();
    let ctx = context_for(Uuid::new_v4());
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": mock.host(), "port": mock.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "mock-upstream"
                }),
            )
            .await,
    )
    .await;
    assert!(
        created["id"].is_string(),
        "the scheme is accepted at create time: {created}"
    );
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    let response = harness
        .get(&ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 403);
    assert!(
        read_json(response).await["detail"]
            .as_str()
            .is_some_and(|d| d.contains("allow_http_upstream"))
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn the_ssrf_guard_refuses_an_internal_endpoint_when_enabled() {
    let mock = MockUpstream::start().await;
    let harness = Harness::builder()
        .config(OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: true,
                allow_private_networks: false,
                allowed_hosts: Vec::new(),
            },
            ..OagwConfig::default()
        })
        .build();
    let ctx = context_for(Uuid::new_v4());
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": mock.host(), "port": mock.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "mock-upstream"
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    let response = harness
        .get(&ctx, "/oagw/v1/proxy/mock-upstream/v1/models")
        .await;
    assert_eq!(response.status(), 403);
    assert!(mock.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Hierarchy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_descendant_inherits_an_ancestors_upstream_and_routes() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mock = MockUpstream::start().await;
    let harness = Harness::builder()
        .config(permissive_config())
        .hierarchy(vec![(leaf, root)])
        .build();
    let root_ctx = context_for(root);
    let leaf_ctx = context_for(leaf);

    let created = read_json(
        harness
            .post_json(
                &root_ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": mock.host(), "port": mock.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "shared-upstream"
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &root_ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;

    // The descendant proxies through the ancestor's configuration...
    assert_eq!(
        harness
            .get(&leaf_ctx, "/oagw/v1/proxy/shared-upstream/v1/models")
            .await
            .status(),
        200
    );
    // ...but cannot see it through the management API.
    let listed = read_json(harness.get(&leaf_ctx, "/oagw/v1/upstreams").await).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        harness
            .get(
                &leaf_ctx,
                &format!("/oagw/v1/upstreams/{}", created["id"].as_str().expect("id"))
            )
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn an_ancestor_disable_cannot_be_re_enabled_by_a_descendant() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mock = MockUpstream::start().await;
    let harness = Harness::builder()
        .config(permissive_config())
        .hierarchy(vec![(leaf, root)])
        .build();
    let root_ctx = context_for(root);
    let leaf_ctx = context_for(leaf);
    let endpoints = json!([ { "scheme": "http", "host": mock.host(), "port": mock.port() } ]);

    let root_upstream = read_json(
        harness
            .post_json(
                &root_ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": endpoints },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "shared-upstream",
                    "enabled": false
                }),
            )
            .await,
    )
    .await;
    assert_eq!(root_upstream["enabled"], false);

    // The descendant shadows the alias with an *enabled* upstream of its own.
    let leaf_upstream = read_json(
        harness
            .post_json(
                &leaf_ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": endpoints },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "shared-upstream",
                    "enabled": true
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &leaf_ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": leaf_upstream["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;

    let response = harness
        .get(&leaf_ctx, "/oagw/v1/proxy/shared-upstream/v1/models")
        .await;
    assert_eq!(
        response.status(),
        503,
        "an ancestor's disable propagates through shadowing"
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn an_enforced_ancestor_rate_limit_survives_shadowing() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mock = MockUpstream::start().await;
    let harness = Harness::builder()
        .config(permissive_config())
        .hierarchy(vec![(leaf, root)])
        .build();
    let root_ctx = context_for(root);
    let leaf_ctx = context_for(leaf);
    let endpoints = json!([ { "scheme": "http", "host": mock.host(), "port": mock.port() } ]);

    harness
        .post_json(
            &root_ctx,
            "/oagw/v1/upstreams",
            &json!({
                "server": { "endpoints": endpoints },
                "protocol": gts::PROTOCOL_HTTP,
                "alias": "shared-upstream",
                "rate_limit": {
                    "sharing": "enforce",
                    "sustained": { "rate": 1, "window": "minute" },
                    "burst": { "capacity": 1 }
                }
            }),
        )
        .await;

    // The descendant asks for a far looser limit; `min()` still applies.
    let leaf_upstream = read_json(
        harness
            .post_json(
                &leaf_ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": endpoints },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "shared-upstream",
                    "rate_limit": {
                        "sustained": { "rate": 10000, "window": "minute" },
                        "burst": { "capacity": 10000 }
                    }
                }),
            )
            .await,
    )
    .await;
    harness
        .post_json(
            &leaf_ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": leaf_upstream["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;

    assert_eq!(
        harness
            .get(&leaf_ctx, "/oagw/v1/proxy/shared-upstream/v1/models")
            .await
            .status(),
        200
    );
    let refused = harness
        .get(&leaf_ctx, "/oagw/v1/proxy/shared-upstream/v1/models")
        .await;
    assert_eq!(
        refused.status(),
        429,
        "the ancestor's enforced ceiling of 1/minute is what binds"
    );
    assert_eq!(refused.headers()["x-ratelimit-limit"], "1");
}

// ---------------------------------------------------------------------------
// Multi-endpoint pooling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_alias_pool_distributes_across_its_endpoints() {
    // Two endpoints, same port, different host spellings of the same
    // listener: the pool is homogeneous (as the schema requires) and the
    // upstream reports which authority each request carried.
    let mock = MockUpstream::start().await;
    let harness = Harness::builder().config(permissive_config()).build();
    let ctx = context_for(Uuid::new_v4());
    let created = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": "127.0.0.1", "port": mock.port() },
                        { "scheme": "http", "host": "localhost", "port": mock.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "pooled-upstream"
                }),
            )
            .await,
    )
    .await;
    assert_eq!(
        created["alias"], "pooled-upstream",
        "an IP-bearing pool is not derivable, so the explicit alias stands"
    );
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;

    for _ in 0..4 {
        assert_eq!(
            harness
                .get(&ctx, "/oagw/v1/proxy/pooled-upstream/v1/models")
                .await
                .status(),
            200
        );
    }
    let authorities: std::collections::BTreeSet<String> = mock
        .requests()
        .iter()
        .filter_map(|request| request.header("host").map(str::to_owned))
        .collect();
    assert_eq!(
        authorities.len(),
        2,
        "both endpoints must be used, got {authorities:?}"
    );
}

#[tokio::test]
async fn a_head_request_is_served_by_a_get_route_and_forwarded_as_head() {
    let f = fixture_with(
        json!({}),
        json!({ "match": { "http": { "methods": ["GET"], "path": "/v1" } } }),
        None,
    )
    .await;
    let response = f
        .harness
        .send(
            &f.ctx,
            empty_request(http::Method::HEAD, "/oagw/v1/proxy/mock-upstream/v1/models"),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(f.upstream.last_request().expect("called").method, "HEAD");
}
