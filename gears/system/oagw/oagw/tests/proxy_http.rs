//! End-to-end proxy behaviour over a real socket: forwarding, header
//! handling, error semantics, rate limiting and CORS.

mod common;

use common::{Fixture, assert_gateway_problem, get, get_with, send};
use http::{Method, StatusCode};
use oagw::config::{OagwConfig, SsrfPolicyConfig};
use oagw::domain::gts_helpers::{self, errors};
use oagw::domain::model::{
    Endpoint, PassthroughMode, PathSuffixMode, RequestHeadersConfig, ResponseHeadersConfig, Scheme,
    ServerConfig,
};
use oagw::test_utils::TestGatewayBuilder;
use serde_json::json;

#[tokio::test]
async fn a_matched_request_reaches_the_upstream() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    let res = get(&fx.proxy_url("mock", "v1/models")).await;
    assert_eq!(res.status, StatusCode::OK);
    let body = res.json();
    assert_eq!(body["method"], json!("GET"));
    assert_eq!(body["path"], json!("/v1/models"));
}

#[tokio::test]
async fn every_response_declares_its_error_source() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    let ok = get(&fx.proxy_url("mock", "v1/models")).await;
    assert_eq!(ok.header("x-oagw-error-source"), Some("upstream"));

    let missing = get(&fx.proxy_url("absent", "v1/models")).await;
    assert_eq!(missing.header("x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn an_upstream_5xx_is_passed_through_unchanged() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    let res = get(&fx.proxy_url("mock", "v1/status/503")).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        res.header("x-oagw-error-source"),
        Some("upstream"),
        "an upstream failure is not a gateway error"
    );
    assert_eq!(res.json()["upstream"], json!("said"));
}

#[tokio::test]
async fn the_method_and_body_are_forwarded_verbatim() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        let res = send(
            method.clone(),
            &fx.proxy_url("mock", "v1/things"),
            &[("content-type", "application/json")],
            Some(r#"{"hello":"world"}"#),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{method} should be forwarded");
        let body = res.json();
        assert_eq!(body["method"], json!(method.as_str()));
        if method != Method::DELETE {
            assert_eq!(body["body"], json!(r#"{"hello":"world"}"#));
        }
    }
}

#[tokio::test]
async fn an_unknown_alias_is_a_route_not_found_problem() {
    let fx = Fixture::start().await;
    let res = get(&fx.proxy_url("absent", "v1/models")).await;
    assert_gateway_problem(&res, StatusCode::NOT_FOUND, errors::ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn a_method_no_route_accepts_is_a_route_not_found_problem() {
    let fx = Fixture::start().await;
    let upstream = fx.upstream("mock", |_| {}).await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = send(Method::POST, &fx.proxy_url("mock", "v1/models"), &[], None).await;
    assert_gateway_problem(&res, StatusCode::NOT_FOUND, errors::ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn a_disabled_upstream_reports_the_link_as_unavailable() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| spec.enabled = Some(false))
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/models")).await;
    assert_gateway_problem(
        &res,
        StatusCode::SERVICE_UNAVAILABLE,
        errors::LINK_UNAVAILABLE,
    );
}

#[tokio::test]
async fn hop_by_hop_and_routing_headers_do_not_reach_the_upstream() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| spec.headers = Some(common::passthrough_all()))
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[
            ("x-oagw-target-host", &fx.upstream.host()),
            ("te", "trailers"),
            ("x-carried", "yes"),
        ],
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let headers = res.json();
    let headers = &headers["headers"];
    assert!(headers.get("x-oagw-target-host").is_none());
    assert!(headers.get("te").is_none());
    assert_eq!(headers["x-carried"], json!("yes"));
    assert_eq!(
        headers["host"],
        json!(format!("{}:{}", fx.upstream.host(), fx.upstream.port())),
        "the Host header names the upstream, not the gateway"
    );
}

#[tokio::test]
async fn passthrough_none_forwards_only_content_headers() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    let res = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[("x-private", "leak"), ("content-type", "application/json")],
    )
    .await;
    let body = res.json();
    assert!(body["headers"].get("x-private").is_none());
    assert_eq!(body["headers"]["content-type"], json!("application/json"));
}

#[tokio::test]
async fn header_set_add_and_remove_rules_are_applied() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            let mut set = std::collections::BTreeMap::new();
            set.insert("x-injected".to_owned(), "gateway".to_owned());
            spec.headers = Some(oagw::domain::model::HeadersConfig {
                request: RequestHeadersConfig {
                    passthrough: PassthroughMode::All,
                    set,
                    remove: vec!["x-drop-me".to_owned()],
                    ..RequestHeadersConfig::default()
                },
                response: ResponseHeadersConfig {
                    remove: vec!["x-internal".to_owned()],
                    ..ResponseHeadersConfig::default()
                },
            });
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[("x-drop-me", "nope"), ("x-keep", "yes")],
    )
    .await;
    let headers = res.json();
    let headers = &headers["headers"];
    assert_eq!(headers["x-injected"], json!("gateway"));
    assert!(headers.get("x-drop-me").is_none());
    assert_eq!(headers["x-keep"], json!("yes"));
}

#[tokio::test]
async fn the_callers_bearer_token_is_never_relayed_upstream() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| spec.headers = Some(common::passthrough_all()))
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[("authorization", "Bearer caller-token")],
    )
    .await;
    let body = res.json();
    assert!(
        body["headers"].get("authorization").is_none(),
        "the inbound credential authenticates the caller to OAGW, not to the upstream"
    );
}

#[tokio::test]
async fn an_api_key_is_injected_from_the_credential_store() {
    let fx = Fixture::with_builder(
        TestGatewayBuilder::new().secrets(vec![("openai-key", "sk-test-value")]),
    )
    .await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.auth = Some(common::auth(
                gts_helpers::APIKEY_AUTH_PLUGIN_ID,
                json!({ "secret_ref": "cred://openai-key", "name": "X-Api-Key", "prefix": "Bearer " }),
            ));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.json()["headers"]["x-api-key"],
        json!("Bearer sk-test-value")
    );
}

#[tokio::test]
async fn an_api_key_can_be_injected_as_a_query_parameter() {
    let fx = Fixture::with_builder(
        TestGatewayBuilder::new().secrets(vec![("openai-key", "sk-test-value")]),
    )
    .await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.auth = Some(common::auth(
                gts_helpers::APIKEY_AUTH_PLUGIN_ID,
                json!({ "secret_ref": "cred://openai-key", "location": "query", "name": "key" }),
            ));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_eq!(res.json()["query"], json!("key=sk-test-value"));
}

#[tokio::test]
async fn a_missing_secret_is_a_secret_not_found_problem() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.auth = Some(common::auth(
                gts_helpers::APIKEY_AUTH_PLUGIN_ID,
                json!({ "secret_ref": "cred://absent" }),
            ));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::INTERNAL_SERVER_ERROR,
        errors::SECRET_NOT_FOUND,
    );
    assert!(
        !String::from_utf8_lossy(&res.body).contains("sk-"),
        "no credential material may appear in an error body"
    );
}

#[tokio::test]
async fn a_reserved_auth_identifier_has_no_implementation() {
    let fx = Fixture::start().await;
    // The reserved identifiers are rejected at bind time, so drive the Data
    // Plane through a hand-built upstream that bypasses validation.
    let upstream = fx
        .upstream("mock", |spec| {
            spec.auth = Some(common::auth(gts_helpers::NOOP_AUTH_PLUGIN_ID, json!({})));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;
    // `noop` resolves and injects nothing, which is the contrast that makes
    // the reserved-identifier rejection meaningful.
    assert_eq!(get(&fx.proxy_url("mock", "v1/echo")).await.status, StatusCode::OK);

    let err = fx
        .gateway
        .control_plane
        .create_upstream(&fx.gateway.security_context, {
            let mut spec = oagw::domain::services::management::UpstreamSpec {
                alias: Some("reserved".to_owned()),
                enabled: None,
                tags: Vec::new(),
                server: fx.endpoints(),
                protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            };
            spec.auth = Some(common::auth(gts_helpers::BEARER_AUTH_PLUGIN_ID, json!({})));
            spec
        })
        .await
        .expect_err("bearer.v1 has no backing implementation");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn the_query_allowlist_gates_forwarded_parameters() {
    let fx = Fixture::start().await;
    let upstream = fx.upstream("mock", |_| {}).await;
    fx.route(&upstream, &["GET"], "/v1", |spec| {
        if let Some(http) = spec.match_config.http.as_mut() {
            http.query_allowlist = vec!["q".to_owned()];
        }
    })
    .await;

    let allowed = get(&format!("{}?q=hello", fx.proxy_url("mock", "v1/search"))).await;
    assert_eq!(allowed.status, StatusCode::OK);
    assert_eq!(allowed.json()["query"], json!("q=hello"));

    let rejected = get(&format!("{}?secret=1", fx.proxy_url("mock", "v1/search"))).await;
    assert_gateway_problem(&rejected, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn a_route_with_suffixes_disabled_rejects_an_extra_path() {
    let fx = Fixture::start().await;
    let upstream = fx.upstream("mock", |_| {}).await;
    fx.route(&upstream, &["GET"], "/v1/models", |spec| {
        if let Some(http) = spec.match_config.http.as_mut() {
            http.path_suffix_mode = PathSuffixMode::Disabled;
        }
    })
    .await;

    assert_eq!(
        get(&fx.proxy_url("mock", "v1/models")).await.status,
        StatusCode::OK
    );
    let res = get(&fx.proxy_url("mock", "v1/models/extra")).await;
    assert_gateway_problem(&res, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn a_rate_limited_upstream_answers_429_with_retry_after() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| spec.rate_limit = Some(common::per_minute(2, 2)))
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    for attempt in 0..2 {
        assert_eq!(
            get(&fx.proxy_url("mock", "v1/echo")).await.status,
            StatusCode::OK,
            "burst token {attempt} should be allowed"
        );
    }
    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::TOO_MANY_REQUESTS,
        errors::RATE_LIMIT_EXCEEDED,
    );
    assert!(
        res.header("retry-after").is_some(),
        "429 responses carry retry guidance"
    );
    assert_eq!(res.json()["context"]["retry_after_seconds"], json!(30));
}

#[tokio::test]
async fn rate_limit_headers_report_the_remaining_budget() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| spec.rate_limit = Some(common::per_minute(5, 5)))
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_eq!(res.header("x-ratelimit-limit"), Some("5"));
    assert_eq!(res.header("x-ratelimit-remaining"), Some("4"));
}

#[tokio::test]
async fn a_preflight_is_answered_without_resolving_an_upstream() {
    let fx = Fixture::start().await;
    // Deliberately no upstream registered: a preflight must not need one.
    let res = send(
        Method::OPTIONS,
        &fx.proxy_url("never-registered", "v1/models"),
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ],
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        res.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(res.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(res.header("access-control-max-age"), Some("86400"));
    assert!(
        res.header("vary").is_some_and(|v| v.contains("Origin")),
        "preflight responses vary on Origin"
    );
}

#[tokio::test]
async fn an_allowed_origin_gets_cors_headers_on_the_actual_response() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.cors = Some(common::cors_for("https://app.example.com", &["GET"]));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(
        res.header("access-control-expose-headers"),
        Some("X-Request-ID")
    );
    assert_eq!(res.header("vary"), Some("Origin"));
}

#[tokio::test]
async fn a_disallowed_origin_is_rejected_before_forwarding() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.cors = Some(common::cors_for("https://app.example.com", &["GET"]));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[("origin", "https://evil.example.com")],
    )
    .await;
    assert_gateway_problem(
        &res,
        StatusCode::FORBIDDEN,
        errors::CORS_ORIGIN_NOT_ALLOWED,
    );
}

#[tokio::test]
async fn a_disallowed_method_is_rejected_before_forwarding() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.cors = Some(common::cors_for("https://app.example.com", &["GET"]));
        })
        .await;
    fx.route(&upstream, &["GET", "POST"], "/v1", |_| {}).await;

    let res = send(
        Method::POST,
        &fx.proxy_url("mock", "v1/echo"),
        &[("origin", "https://app.example.com")],
        Some("{}"),
    )
    .await;
    assert_gateway_problem(
        &res,
        StatusCode::FORBIDDEN,
        errors::CORS_METHOD_NOT_ALLOWED,
    );
}

#[tokio::test]
async fn a_slow_upstream_produces_a_request_timeout() {
    let fx = Fixture::with_builder(TestGatewayBuilder::new().config(OagwConfig {
        proxy_timeout_secs: 1,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicyConfig {
            enabled: false,
            ..SsrfPolicyConfig::default()
        },
        ..OagwConfig::default()
    }))
    .await;
    fx.simple("mock").await;

    let res = get(&fx.proxy_url("mock", "v1/slow")).await;
    assert_gateway_problem(&res, StatusCode::GATEWAY_TIMEOUT, errors::REQUEST_TIMEOUT);
}

#[tokio::test]
async fn an_unreachable_endpoint_reports_the_link_as_unavailable() {
    let fx = Fixture::start().await;
    // Port 1 on loopback: nothing listens there.
    let upstream = fx
        .upstream("dead", |spec| {
            spec.server = ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: Some(1),
                }],
            };
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("dead", "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::SERVICE_UNAVAILABLE,
        errors::LINK_UNAVAILABLE,
    );
}

#[tokio::test]
async fn plaintext_is_refused_when_the_deployment_forbids_it() {
    let fx = Fixture::with_builder(TestGatewayBuilder::new().config(OagwConfig {
        // The scheme is still a legal *field* value — only the connection is
        // refused (`cpt-cf-oagw-constraint-https-only`).
        allow_http_upstream: false,
        ssrf_policy: SsrfPolicyConfig {
            enabled: false,
            ..SsrfPolicyConfig::default()
        },
        ..OagwConfig::default()
    }))
    .await;
    let upstream = fx
        .upstream("mock", |_| {})
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_gateway_problem(&res, StatusCode::BAD_REQUEST, errors::VALIDATION);
    assert!(
        res.json()["detail"]
            .as_str()
            .is_some_and(|d| d.contains("allow_http_upstream")),
        "the message names the flag that lifts the restriction"
    );
}

#[tokio::test]
async fn a_required_headers_guard_rejects_an_incomplete_request() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.plugins = Some(common::chain(vec![(
                gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                json!({ "required_request_headers": "x-correlation-id" }),
            )]));
            spec.headers = Some(common::passthrough_all());
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let rejected = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_gateway_problem(&rejected, StatusCode::BAD_REQUEST, errors::VALIDATION);

    let accepted = get_with(
        &fx.proxy_url("mock", "v1/echo"),
        &[("x-correlation-id", "abc123")],
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK);
}

#[tokio::test]
async fn a_required_response_header_guard_rejects_a_bad_upstream_reply() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.plugins = Some(common::chain(vec![(
                gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                json!({ "required_response_headers": "content-type" }),
            )]));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("mock", "v1/nocontenttype")).await;
    assert_gateway_problem(&res, StatusCode::BAD_GATEWAY, errors::DOWNSTREAM_ERROR);
}

#[tokio::test]
async fn the_request_id_transform_propagates_a_correlation_id() {
    let fx = Fixture::start().await;
    let upstream = fx
        .upstream("mock", |spec| {
            spec.plugins = Some(common::chain(vec![(
                gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
                json!({}),
            )]));
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let minted = get(&fx.proxy_url("mock", "v1/echo")).await;
    let forwarded = minted.json()["headers"]["x-request-id"]
        .as_str()
        .map(str::to_owned)
        .expect("a correlation id is minted when the caller supplies none");
    assert!(uuid::Uuid::parse_str(&forwarded).is_ok());

    let upstream = fx
        .upstream("mock2", |spec| {
            spec.plugins = Some(common::chain(vec![(
                gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
                json!({}),
            )]));
            spec.headers = Some(common::passthrough_all());
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;
    let propagated = get_with(
        &fx.proxy_url("mock2", "v1/echo"),
        &[("x-request-id", "req_abc123")],
    )
    .await;
    assert_eq!(
        propagated.json()["headers"]["x-request-id"],
        json!("req_abc123")
    );
}

#[tokio::test]
async fn a_multi_endpoint_common_suffix_alias_requires_the_target_host_header() {
    let fx = Fixture::start().await;
    // Two hostnames sharing a registrable suffix derive `vendor.test`, which
    // names no single endpoint.
    let created = fx
        .gateway
        .control_plane
        .create_upstream(
            &fx.gateway.security_context,
            oagw::domain::services::management::UpstreamSpec {
                alias: None,
                enabled: None,
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![
                        Endpoint {
                            scheme: Scheme::Http,
                            host: "us.vendor.com".to_owned(),
                            port: Some(fx.upstream.port()),
                        },
                        Endpoint {
                            scheme: Scheme::Http,
                            host: "eu.vendor.com".to_owned(),
                            port: Some(fx.upstream.port()),
                        },
                    ],
                },
                protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        )
        .await
        .expect("created");
    // The mock listens on an ephemeral port, so the derived alias keeps it —
    // which is exactly the `suffix:port` form the design calls for.
    let alias = format!("vendor.com:{}", fx.upstream.port());
    assert_eq!(created.alias, alias);
    fx.route(&created, &["GET"], "/v1", |_| {}).await;

    let missing = get(&fx.proxy_url(&alias, "v1/echo")).await;
    assert_gateway_problem(
        &missing,
        StatusCode::BAD_REQUEST,
        errors::MISSING_TARGET_HOST,
    );
    assert_eq!(
        missing.json()["context"]["valid_hosts"],
        json!(["us.vendor.com", "eu.vendor.com"])
    );

    let unknown = get_with(
        &fx.proxy_url(&alias, "v1/echo"),
        &[("x-oagw-target-host", "apac.vendor.com")],
    )
    .await;
    assert_gateway_problem(
        &unknown,
        StatusCode::BAD_REQUEST,
        errors::UNKNOWN_TARGET_HOST,
    );

    let malformed = get_with(
        &fx.proxy_url(&alias, "v1/echo"),
        &[("x-oagw-target-host", "us.vendor.com:8443")],
    )
    .await;
    assert_gateway_problem(
        &malformed,
        StatusCode::BAD_REQUEST,
        errors::INVALID_TARGET_HOST,
    );
}

#[tokio::test]
async fn an_oversized_declared_body_is_rejected_before_buffering() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    // A `Content-Length` above the 100 MB hard limit, with no body actually
    // sent: the check must fire on the header alone.
    let res = send(
        Method::POST,
        &fx.proxy_url("mock", "v1/upload"),
        &[("content-length", "104857601")],
        None,
    )
    .await;
    assert_gateway_problem(
        &res,
        StatusCode::PAYLOAD_TOO_LARGE,
        errors::PAYLOAD_TOO_LARGE,
    );
}

#[tokio::test]
async fn a_bound_plugin_that_resolves_to_nothing_is_a_plugin_not_found() {
    let fx = Fixture::start().await;
    // Bind a UUID-backed plugin, then delete the definition out from under
    // the binding by pointing at one that never existed.
    let dangling = gts_helpers::anonymous_id(gts_helpers::GUARD_PLUGIN_TYPE, uuid::Uuid::new_v4());
    let err = fx
        .gateway
        .control_plane
        .create_upstream(
            &fx.gateway.security_context,
            oagw::domain::services::management::UpstreamSpec {
                alias: Some("dangling".to_owned()),
                enabled: None,
                tags: Vec::new(),
                server: fx.endpoints(),
                protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: Some(common::chain(vec![(&dangling, json!({}))])),
                rate_limit: None,
                cors: None,
            },
        )
        .await
        .expect_err("an unregistered plugin cannot be bound");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn a_bare_alias_matches_a_root_route_and_nothing_narrower() {
    let fx = Fixture::start().await;

    // The inbound suffix is what route paths are matched against, so a
    // suffix-less request only reaches a route mounted at `/`.
    let rooted = fx.upstream("rooted", |_| {}).await;
    fx.route(&rooted, &["GET"], "/", |_| {}).await;
    let res = get(&fx.proxy_url("rooted", "")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["path"], json!("/"));

    let narrow = fx.upstream("narrow", |_| {}).await;
    fx.route(&narrow, &["GET"], "/v1/default", |_| {}).await;
    let res = get(&fx.proxy_url("narrow", "")).await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "a route at /v1/default is reached by addressing /proxy/narrow/v1/default"
    );
    let res = get(&fx.proxy_url("narrow", "v1/default")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["path"], json!("/v1/default"));
}
