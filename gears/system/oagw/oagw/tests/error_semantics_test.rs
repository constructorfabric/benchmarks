//! Error semantics (T066, T067).
//!
//! Every row of `contracts/errors.md` is exercised: the HTTP status, the exact
//! GTS `type`, the title, the problem content type and the error-source header.
//! The table is driven through the same translation the handlers use, so a new
//! variant that forgets its row fails here. The wire tests then cover the rows
//! that are reachable end-to-end, including the upstream passthrough of T067.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use common::*;
use httpmock::prelude::*;
use oagw::api::rest::error::{http_status_of, problem_from_domain_error};
use oagw::domain::error::{DomainError, RateLimitSnapshot, ReferencedBy};
use oagw::domain::gts_helpers as gts;
use serde_json::{Value, json};

/// The base prefix of every OAGW error type.
const BASE: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// One row of the error contract.
struct Row {
    status: u16,
    suffix: &'static str,
    title: &'static str,
    error: DomainError,
}

/// Every row of `contracts/errors.md`, in the order the contract lists them.
fn rows() -> Vec<Row> {
    vec![
        Row {
            status: 400,
            suffix: "validation.error.v1",
            title: "Validation Error",
            error: DomainError::validation("alias is required"),
        },
        Row {
            status: 400,
            suffix: "routing.missing_target_host.v1",
            title: "Missing Target Host Header",
            error: DomainError::MissingTargetHost {
                alias: "vendor.com".to_string(),
                valid_hosts: vec!["us.vendor.com".to_string(), "eu.vendor.com".to_string()],
                upstream_id: "u1".to_string(),
            },
        },
        Row {
            status: 400,
            suffix: "routing.invalid_target_host.v1",
            title: "Invalid Target Host Format",
            error: DomainError::InvalidTargetHost {
                invalid_value: "host:8443".to_string(),
            },
        },
        Row {
            status: 400,
            suffix: "routing.unknown_target_host.v1",
            title: "Unknown Target Host",
            error: DomainError::UnknownTargetHost {
                invalid_value: "ap.vendor.com".to_string(),
                valid_hosts: vec!["us.vendor.com".to_string()],
            },
        },
        Row {
            status: 401,
            suffix: "auth.failed.v1",
            title: "Authentication Failed",
            error: DomainError::AuthenticationFailed {
                detail: "the upstream refused the credentials".to_string(),
                plugin_id: Some(gts::BUILTIN_AUTH_APIKEY.to_string()),
            },
        },
        Row {
            status: 403,
            suffix: "permission.denied.v1",
            title: "Permission Denied",
            error: DomainError::permission_denied("the caller may not override"),
        },
        Row {
            status: 404,
            suffix: "route.not_found.v1",
            title: "Route Not Found",
            error: DomainError::RouteNotFound {
                detail: "no route matches".to_string(),
                alias: Some("vendor.com".to_string()),
                host: None,
                path: Some("v1/models".to_string()),
            },
        },
        Row {
            status: 409,
            suffix: "plugin.in_use.v1",
            title: "Plugin In Use",
            error: DomainError::PluginInUse {
                plugin_id: "p1".to_string(),
                referenced_by: ReferencedBy {
                    upstreams: vec!["u1".to_string()],
                    routes: Vec::new(),
                },
            },
        },
        Row {
            status: 409,
            suffix: "conflict.v1",
            title: "Conflict",
            error: DomainError::conflict("the alias is already bound"),
        },
        Row {
            status: 413,
            suffix: "payload.too_large.v1",
            title: "Payload Too Large",
            error: DomainError::PayloadTooLarge {
                detail: "the body exceeds 1 MiB".to_string(),
            },
        },
        Row {
            status: 429,
            suffix: "rate_limit.exceeded.v1",
            title: "Rate Limit Exceeded",
            error: DomainError::RateLimitExceeded {
                snapshot: RateLimitSnapshot {
                    limit: 100,
                    remaining: 0,
                    reset: 1_706_626_800,
                    retry_after: 15,
                },
                host: Some("api.vendor.com".to_string()),
                path: Some("v1/chat".to_string()),
                upstream_id: Some("u1".to_string()),
            },
        },
        Row {
            status: 500,
            suffix: "secret.not_found.v1",
            title: "Secret Not Found",
            error: DomainError::SecretNotFound {
                detail: "credential `cred://alpha` does not exist".to_string(),
                plugin_id: Some(gts::BUILTIN_AUTH_APIKEY.to_string()),
            },
        },
        Row {
            status: 502,
            suffix: "protocol.error.v1",
            title: "Protocol Error",
            error: DomainError::ProtocolError {
                detail: "the handshake was malformed".to_string(),
                host: Some("api.vendor.com".to_string()),
            },
        },
        Row {
            status: 502,
            suffix: "downstream.error.v1",
            title: "Downstream Error",
            error: DomainError::DownstreamError {
                status: 502,
                host: Some("api.vendor.com".to_string()),
            },
        },
        Row {
            status: 502,
            suffix: "stream.aborted.v1",
            title: "Stream Aborted",
            error: DomainError::StreamAborted {
                detail: "the upstream hung up mid-stream".to_string(),
            },
        },
        Row {
            status: 503,
            suffix: "link.unavailable.v1",
            title: "Link Unavailable",
            error: DomainError::LinkUnavailable {
                detail: "upstream `vendor.com` is disabled".to_string(),
                upstream_id: Some("u1".to_string()),
                alias: Some("vendor.com".to_string()),
            },
        },
        Row {
            status: 503,
            suffix: "circuit_breaker.open.v1",
            title: "Circuit Breaker Open",
            error: DomainError::CircuitBreakerOpen {
                host: Some("api.vendor.com".to_string()),
            },
        },
        Row {
            status: 503,
            suffix: "plugin.not_found.v1",
            title: "Plugin Not Found",
            error: DomainError::PluginNotFound {
                plugin_id: "p1".to_string(),
            },
        },
        Row {
            status: 503,
            suffix: "service.unavailable.v1",
            title: "Service Unavailable",
            error: DomainError::ServiceUnavailable {
                detail: "warming up".to_string(),
                retry_after: Some(5),
            },
        },
        Row {
            status: 504,
            suffix: "timeout.connection.v1",
            title: "Connection Timeout",
            error: DomainError::ConnectionTimeout {
                host: Some("api.vendor.com".to_string()),
            },
        },
        Row {
            status: 504,
            suffix: "timeout.request.v1",
            title: "Request Timeout",
            error: DomainError::RequestTimeout {
                host: Some("api.vendor.com".to_string()),
            },
        },
        Row {
            status: 504,
            suffix: "timeout.idle.v1",
            title: "Idle Timeout",
            error: DomainError::IdleTimeout {
                host: Some("api.vendor.com".to_string()),
            },
        },
        Row {
            status: 500,
            suffix: "internal.v1",
            title: "Internal Error",
            error: DomainError::internal("unexpected state"),
        },
    ]
}

#[tokio::test]
async fn every_error_contract_row_has_its_documented_envelope() {
    for row in rows() {
        let instance = "/oagw/v1/proxy/vendor.com/v1/models";
        let problem = problem_from_domain_error(&row.error, instance);
        let expected_type = format!("{BASE}{}", row.suffix);

        assert_eq!(
            problem.type_id, expected_type,
            "the GTS type of {}",
            row.suffix
        );
        assert_eq!(problem.title, row.title, "the title of {}", row.suffix);
        assert_eq!(problem.status, row.status, "the status of {}", row.suffix);
        assert_eq!(
            http_status_of(&row.error), row.status,
            "the status helper of {}",
            row.suffix
        );
        assert_eq!(problem.source, "gateway", "{} errors come from the gateway", row.suffix);
        assert_eq!(problem.instance.as_deref(), Some(instance));

        let response = problem.clone().into_response();
        assert_eq!(
            response.status(),
            StatusCode::from_u16(row.status).unwrap(),
            "{} status on the wire",
            row.suffix
        );
        assert_eq!(
            header(&response, "content-type").as_deref(),
            Some("application/problem+json"),
            "{} content type",
            row.suffix
        );
        assert_eq!(
            header(&response, "x-oagw-error-source").as_deref(),
            Some("gateway"),
            "{} source header",
            row.suffix
        );
        let body: Value = serde_json::from_slice(&read_body(response).await)
            .unwrap_or_else(|_| panic!("{} renders a JSON problem", row.suffix));
        assert_eq!(body["type"], json!(expected_type), "{} body type", row.suffix);
        assert_eq!(body["title"], json!(row.title), "{} body title", row.suffix);
        assert_eq!(body["status"], json!(row.status), "{} body status", row.suffix);
        assert_eq!(body["instance"], json!(instance), "{} body instance", row.suffix);
    }
}

#[tokio::test]
async fn the_extension_fields_landing_on_each_row() {
    let rendered = |err: &DomainError| {
        problem_from_domain_error(err, "/oagw/v1/proxy/vendor.com").to_json()
    };

    // Missing target host: the alias, the hosts that could have been chosen
    // and the upstream the caller is talking to.
    let body = rendered(&DomainError::MissingTargetHost {
        alias: "vendor.com".to_string(),
        valid_hosts: vec!["us.vendor.com".to_string(), "eu.vendor.com".to_string()],
        upstream_id: "gts.cf.core.oagw.upstream.v1~abc".to_string(),
    });
    assert_eq!(body["alias"], json!("vendor.com"));
    assert_eq!(body["valid_hosts"], json!(["us.vendor.com", "eu.vendor.com"]));
    assert_eq!(body["upstream_id"], json!("gts.cf.core.oagw.upstream.v1~abc"));

    // Invalid and unknown target host name the rejected value.
    let body = rendered(&DomainError::InvalidTargetHost {
        invalid_value: "host:8443".to_string(),
    });
    assert_eq!(body["invalid_value"], json!("host:8443"));
    let body = rendered(&DomainError::UnknownTargetHost {
        invalid_value: "ap.vendor.com".to_string(),
        valid_hosts: vec!["us.vendor.com".to_string()],
    });
    assert_eq!(body["invalid_value"], json!("ap.vendor.com"));
    assert_eq!(body["valid_hosts"], json!(["us.vendor.com"]));

    // A refused authentication names the plugin that refused it.
    let body = rendered(&DomainError::AuthenticationFailed {
        detail: "refused".to_string(),
        plugin_id: Some(gts::BUILTIN_AUTH_APIKEY.to_string()),
    });
    assert_eq!(body["plugin_id"], json!(gts::BUILTIN_AUTH_APIKEY));

    // Route not found carries the alias and the path that matched nothing.
    let body = rendered(&DomainError::RouteNotFound {
        detail: "no route".to_string(),
        alias: Some("vendor.com".to_string()),
        host: Some("api.vendor.com".to_string()),
        path: Some("v1/models".to_string()),
    });
    assert_eq!(body["alias"], json!("vendor.com"));
    assert_eq!(body["host"], json!("api.vendor.com"));
    assert_eq!(body["path"], json!("v1/models"));

    // A plugin still in use says who holds it.
    let body = rendered(&DomainError::PluginInUse {
        plugin_id: "p1".to_string(),
        referenced_by: ReferencedBy {
            upstreams: vec!["u1".to_string()],
            routes: vec!["r1".to_string()],
        },
    });
    assert_eq!(body["plugin_id"], json!("p1"));
    assert_eq!(body["referenced_by"], json!({ "upstreams": ["u1"], "routes": ["r1"] }));

    // Rate limiting advertises the retry hint.
    let body = rendered(&DomainError::RateLimitExceeded {
        snapshot: RateLimitSnapshot {
            limit: 100,
            remaining: 0,
            reset: 1_706_626_800,
            retry_after: 15,
        },
        host: Some("api.vendor.com".to_string()),
        path: Some("v1/chat".to_string()),
        upstream_id: Some("u1".to_string()),
    });
    assert_eq!(body["retry_after_seconds"], json!(15));
    assert_eq!(body["host"], json!("api.vendor.com"));
    assert_eq!(body["path"], json!("v1/chat"));
    assert_eq!(body["upstream_id"], json!("u1"));

    // A missing credential names the plugin that wanted it.
    let body = rendered(&DomainError::SecretNotFound {
        detail: "missing".to_string(),
        plugin_id: Some(gts::BUILTIN_AUTH_APIKEY.to_string()),
    });
    assert_eq!(body["plugin_id"], json!(gts::BUILTIN_AUTH_APIKEY));

    // Link unavailable identifies the upstream and its alias.
    let body = rendered(&DomainError::LinkUnavailable {
        detail: "disabled".to_string(),
        upstream_id: Some("u1".to_string()),
        alias: Some("vendor.com".to_string()),
    });
    assert_eq!(body["upstream_id"], json!("u1"));
    assert_eq!(body["alias"], json!("vendor.com"));

    // Timeouts, breakers, protocol failures and downstream errors name the
    // host they were talking to.
    for err in [
        DomainError::ProtocolError { detail: "x".to_string(), host: Some("h".to_string()) },
        DomainError::DownstreamError { status: 500, host: Some("h".to_string()) },
        DomainError::CircuitBreakerOpen { host: Some("h".to_string()) },
        DomainError::ConnectionTimeout { host: Some("h".to_string()) },
        DomainError::RequestTimeout { host: Some("h".to_string()) },
        DomainError::IdleTimeout { host: Some("h".to_string()) },
    ] {
        assert_eq!(rendered(&err)["host"], json!("h"), "{err:?}");
    }

    // A withheld service-unavailable retry hint stays absent rather than null.
    let body = rendered(&DomainError::ServiceUnavailable {
        detail: "warming up".to_string(),
        retry_after: None,
    });
    assert!(body.get("retry_after_seconds").is_none());
}

#[tokio::test]
async fn the_cors_rows_carry_their_own_types() {
    let problem = oagw::api::rest::error::cors_origin_not_allowed("https://evil.example", "/oagw/v1/proxy/web");
    assert_eq!(problem.type_id, format!("{BASE}cors.origin_not_allowed.v1"));
    assert_eq!(problem.title, "CORS Origin Not Allowed");
    assert_eq!(problem.status, 403);
    assert_eq!(problem.invalid_value.as_deref(), Some("https://evil.example"));
    let response = problem.into_response();
    assert_eq!(header(&response, "x-oagw-error-source").as_deref(), Some("gateway"));

    let problem = oagw::api::rest::error::cors_method_not_allowed("DELETE", "/oagw/v1/proxy/web");
    assert_eq!(problem.type_id, format!("{BASE}cors.method_not_allowed.v1"));
    assert_eq!(problem.title, "CORS Method Not Allowed");
    assert_eq!(problem.status, 403);
}

#[tokio::test]
async fn no_error_body_or_log_line_carries_secret_material() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(500).body("boom");
    });
    let harness = Harness::plaintext_gear();
    let upstream_id =
        create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    // The credential the caller offered, and the one the plugin would inject.
    let secret = "sk-super-secret-value";
    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/models",
            &[("authorization", &format!("Bearer {secret}"))],
            None,
        ))
        .await;

    let body = String::from_utf8_lossy(&read_body(response).await).to_string();
    assert!(!body.contains(secret), "the secret never reaches a response body: {body}");
    assert!(!body.contains("sk-"), "no credential-looking material in the body: {body}");
}

// ---------------------------------------------------------------------------
// T067: upstream passthrough attribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upstream_500_is_relayed_verbatim_as_an_upstream_error() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(500)
            .header("content-type", "application/json")
            .body(r#"{"error":"vendor exploded"}"#);
    });
    let harness = Harness::plaintext_gear();
    let upstream_id =
        create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/vendor.com/v1/models", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        header(&response, "x-oagw-error-source").as_deref(),
        Some("upstream"),
        "the failure belongs to the upstream, not the gateway"
    );
    assert_eq!(
        header(&response, "content-type").as_deref(),
        Some("application/json"),
        "the upstream content type is preserved"
    );
    let body = String::from_utf8_lossy(&read_body(response).await).to_string();
    assert_eq!(body, r#"{"error":"vendor exploded"}"#);
    assert!(
        !body.contains("problem+json"),
        "a passthrough never becomes a problem document: {body}"
    );
}

#[tokio::test]
async fn an_upstream_429_is_attributed_to_the_upstream_with_its_own_body() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(429)
            .header("content-type", "text/plain")
            .header("retry-after", "77")
            .body("vendor quota");
    });
    let harness = Harness::plaintext_gear();
    let upstream_id =
        create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/vendor.com/v1/models", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        header(&response, "x-oagw-error-source").as_deref(),
        Some("upstream"),
        "the gateway did not produce this 429"
    );
    assert_eq!(
        header(&response, "retry-after").as_deref(),
        Some("77"),
        "the upstream's own retry hint is relayed"
    );
    let body = String::from_utf8_lossy(&read_body(response).await).to_string();
    assert_eq!(body, "vendor quota", "the upstream body is verbatim");
    assert!(
        !body.contains("rate_limit.exceeded"),
        "the gateway's 429 problem body must not be substituted: {body}"
    );
    assert!(
        !body.contains("retry_after_seconds"),
        "the gateway's problem fields stay out of a passthrough: {body}"
    );
}

// ---------------------------------------------------------------------------
// Wire-level rows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_alias_is_a_404_route_not_found_problem() {
    let harness = Harness::plaintext_gear();
    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/ghost/v1/models", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&response, "x-oagw-error-source").as_deref(), Some("gateway"));
    let body = read_json(response).await;
    assert_eq!(body["type"], json!(format!("{BASE}route.not_found.v1")));
    assert_eq!(body["title"], json!("Route Not Found"));
    assert_eq!(body["status"], json!(404));
    assert_eq!(body["instance"], json!("/oagw/v1/proxy/ghost"));
    assert_eq!(body["alias"], json!("ghost"));
}

#[tokio::test]
async fn an_unroutable_method_is_a_404_with_the_forwarded_path() {
    let stub = MockServer::start();
    let harness = Harness::plaintext_gear();
    let upstream_id =
        create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request("DELETE", "/oagw/v1/proxy/vendor.com/v1/models", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = read_json(response).await;
    assert_eq!(body["type"], json!(format!("{BASE}route.not_found.v1")));
    assert_eq!(body["alias"], json!("vendor.com"));
    assert_eq!(body["path"], json!("/v1/models"));
}

#[tokio::test]
async fn a_bogus_target_host_hint_is_an_invalid_target_host_problem() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let upstream_id =
        create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/models",
            &[("x-oagw-target-host", "127.0.0.1:9999")],
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = read_json(response).await;
    assert_eq!(body["type"], json!(format!("{BASE}routing.invalid_target_host.v1")));
    assert_eq!(body["title"], json!("Invalid Target Host Format"));
    assert_eq!(body["invalid_value"], json!("127.0.0.1:9999"));
}

#[tokio::test]
async fn a_target_host_the_upstream_does_not_have_is_unknown() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let upstream_id =
        create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/models",
            &[("x-oagw-target-host", "ap.vendor.com")],
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = read_json(response).await;
    assert_eq!(body["type"], json!(format!("{BASE}routing.unknown_target_host.v1")));
    assert_eq!(body["invalid_value"], json!("ap.vendor.com"));
    assert_eq!(body["valid_hosts"], json!(["127.0.0.1"]));
}

#[tokio::test]
async fn a_validation_failure_is_a_400_problem() {
    let harness = Harness::plaintext_gear();
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(json!({ "enabled": true }))))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, "x-oagw-error-source").as_deref(), Some("gateway"));
    let body = read_json(response).await;
    assert_eq!(body["type"], json!(format!("{BASE}validation.error.v1")));
    assert_eq!(body["title"], json!("Validation Error"));
    assert_eq!(body["status"], json!(400));
    assert_eq!(body["instance"], json!("/oagw/v1/upstreams"));
}

/// The id the resolution algorithm hands to the data plane when a persisted
/// plugin's record is gone or names no executable implementation.
const UNKNOWN_NAMED: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nope.v1";

#[tokio::test]
async fn a_plugin_the_registry_does_not_know_is_a_503() {
    // A custom plugin definition is bound by UUID and is storable — US6/AC5
    // only refuses an *unknown* reference at write time — but scripted custom
    // plugins have no executable implementation in this gear, so a request
    // through one fails loud with the plugin-not-found row.
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let created = harness
        .send(harness.request(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "kind": "guard",
                "name": "custom-guard",
                "config": {},
                "source": "def plugin(ctx):\n    return None\n"
            })),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED, "the plugin is storable");
    let uuid = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    // A custom plugin is addressed by its full GTS identifier, whose instance
    // part is the record's UUID (DESIGN §"Plugin Identification Model").
    let plugin_id = format!("gts.cf.core.oagw.guard_plugin.v1~{uuid}");

    let mut body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    body["plugins"] = json!({ "items": [plugin_id] });
    let upstream = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(upstream.status(), StatusCode::CREATED, "a known plugin is bindable");
    let upstream_id = read_json(upstream).await["id"].as_str().unwrap_or_default().to_string();
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/vendor.com/v1/models", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, "x-oagw-error-source").as_deref(), Some("gateway"));
    let body = read_json(response).await;
    assert_eq!(body["type"], json!(format!("{BASE}plugin.not_found.v1")));
    assert_eq!(body["title"], json!("Plugin Not Found"));
    assert_eq!(body["plugin_id"], json!(plugin_id));
}

#[tokio::test]
async fn an_unknown_named_plugin_is_refused_at_write_time() {
    // US6/AC5: a configuration referencing a plugin identifier the gear cannot
    // resolve is rejected when the resource is created, not when it is used.
    let stub = MockServer::start();
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("refused", "127.0.0.1", stub.port(), "http");
    body["plugins"] = json!({ "items": [UNKNOWN_NAMED] });
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = read_json(response).await;
    assert_eq!(problem["type"], json!(format!("{BASE}validation.error.v1")));
}

/// The first value of a response header.
fn header(response: &axum::http::Response<axum::body::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}
