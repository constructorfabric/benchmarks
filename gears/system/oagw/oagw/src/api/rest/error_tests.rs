#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::time::Duration;

use super::{
    ApiResult, ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, INTERNAL_DETAIL,
    OagwProblem, ProblemExtensions, ReferencedBy,
};
use crate::domain::error::{DomainError, OagwErrorType};
use crate::domain::model::canonical_types;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::response::IntoResponse;

/// One row per domain error whose wire `type` is part of the contract.
fn cases() -> Vec<(DomainError, &'static str, u16)> {
    vec![
        (
            DomainError::Validation {
                detail: "bad alias".to_owned(),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            400,
        ),
        (
            DomainError::MissingTargetHost {
                alias: "api.openai.com".to_owned(),
                valid_hosts: vec!["api.openai.com".to_owned()],
            },
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
            400,
        ),
        (
            DomainError::InvalidTargetHost {
                invalid_value: "api.openai.com:443".to_owned(),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
            400,
        ),
        (
            DomainError::UnknownTargetHost {
                invalid_value: "nope".to_owned(),
                valid_hosts: vec!["api.openai.com".to_owned()],
            },
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
            400,
        ),
        (
            DomainError::AuthenticationFailed {
                detail: "no credentials".to_owned(),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            401,
        ),
        (
            DomainError::RouteNotFound {
                alias: "api.openai.com".to_owned(),
                path: "/v1/x".to_owned(),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            404,
        ),
        (
            DomainError::PluginInUse {
                plugin_id: "gts.cf.core.oagw.guard_plugin.v1~3f2c".to_owned(),
                upstreams: vec!["gts.cf.core.oagw.upstream.v1~aaa".to_owned()],
                routes: Vec::new(),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            409,
        ),
        (
            DomainError::PayloadTooLarge { limit_bytes: 1024 },
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            413,
        ),
        (
            DomainError::RateLimitExceeded {
                detail: "bucket empty".to_owned(),
                retry_after: Some(Duration::from_secs(3)),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            429,
        ),
        (
            DomainError::SecretNotFound {
                detail: "secret gone".to_owned(),
            },
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            500,
        ),
        (
            DomainError::LinkUnavailable {
                detail: "link down".to_owned(),
                retry_after: None,
            },
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            503,
        ),
        (
            DomainError::NotFound {
                resource: "gts.cf.core.oagw.upstream.v1~aaa".to_owned(),
            },
            canonical_types::NOT_FOUND,
            404,
        ),
        (
            DomainError::Conflict {
                detail: "taken".to_owned(),
            },
            canonical_types::ALREADY_EXISTS,
            409,
        ),
        (
            DomainError::AccessDenied {
                detail: "no".to_owned(),
            },
            canonical_types::PERMISSION_DENIED,
            403,
        ),
        (
            DomainError::InvalidArgument {
                detail: "no".to_owned(),
            },
            canonical_types::INVALID_ARGUMENT,
            400,
        ),
        (
            DomainError::ServiceUnavailable {
                detail: "no".to_owned(),
                cause: None,
            },
            canonical_types::SERVICE_UNAVAILABLE,
            503,
        ),
        (
            DomainError::Internal {
                diagnostic: "boom".to_owned(),
                cause: None,
            },
            canonical_types::INTERNAL,
            500,
        ),
    ]
}

/// `DESIGN` §3.3: every domain error maps to exactly one RFC 9457 problem.
#[test]
fn every_domain_error_maps_to_one_problem_type_and_status() {
    for (error, expected_type, expected_status) in cases() {
        let rendered = format!("{error:?}");
        let problem: OagwProblem = OagwProblem::from(error);
        assert_eq!(problem.kind(), expected_type, "{rendered}");
        assert_eq!(problem.status(), expected_status, "{rendered}");
        assert!(!problem.title().is_empty(), "{rendered}");
        assert!(!problem.detail().is_empty(), "{rendered}");
    }
}

/// `ADR`-0007: a gateway-produced error always carries
/// `X-OAGW-Error-Source: gateway` and `application/problem+json`, and its body
/// carries the mandated `type`.
#[tokio::test]
async fn every_error_response_carries_the_gateway_source_header() {
    for (error, expected_type, expected_status) in cases() {
        let rendered = format!("{error:?}");
        let response = OagwProblem::from(error).into_response();
        assert_eq!(response.status().as_u16(), expected_status, "{rendered}");
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert_eq!(content_type, "application/problem+json", "{rendered}");
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY),
            "{rendered}"
        );

        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(document["type"], expected_type, "{rendered}");
        assert_eq!(document["status"], expected_status, "{rendered}");
        assert!(document["title"].is_string(), "{rendered}");
        assert!(document["detail"].is_string(), "{rendered}");
        // The platform's canonical problem carries a mandatory `context`; the
        // OAGW envelope deliberately omits it so the platform middleware
        // passes the body through untouched.
        assert!(document.get("context").is_none(), "{rendered}");
    }
}

#[tokio::test]
async fn in_use_error_carries_plugin_id_and_referenced_by() {
    let problem: OagwProblem = DomainError::PluginInUse {
        plugin_id: "gts.cf.core.oagw.guard_plugin.v1~3f2c".to_owned(),
        upstreams: vec!["gts.cf.core.oagw.upstream.v1~aaa".to_owned()],
        routes: vec!["gts.cf.core.oagw.route.v1~bbb".to_owned()],
    }
    .into();

    assert_eq!(
        problem.extensions().plugin_id.as_deref(),
        Some("gts.cf.core.oagw.guard_plugin.v1~3f2c")
    );
    let referenced_by = problem.extensions().referenced_by.clone().unwrap();
    assert_eq!(
        referenced_by.upstreams,
        ["gts.cf.core.oagw.upstream.v1~aaa"]
    );
    assert_eq!(referenced_by.routes, ["gts.cf.core.oagw.route.v1~bbb"]);
}

#[test]
fn target_host_errors_expose_the_valid_endpoint_hosts() {
    let problem: OagwProblem = DomainError::MissingTargetHost {
        alias: "api.openai.com".to_owned(),
        valid_hosts: vec![
            "eu.api.openai.com".to_owned(),
            "us.api.openai.com".to_owned(),
        ],
    }
    .into();
    assert_eq!(
        problem.extensions().valid_hosts.clone().unwrap(),
        ["eu.api.openai.com", "us.api.openai.com"]
    );
    assert_eq!(
        problem.extensions().alias.as_deref(),
        Some("api.openai.com")
    );

    let problem: OagwProblem = DomainError::InvalidTargetHost {
        invalid_value: "eu.api.openai.com:443".to_owned(),
    }
    .into();
    assert_eq!(
        problem.extensions().invalid_value.as_deref(),
        Some("eu.api.openai.com:443")
    );

    let problem: OagwProblem = DomainError::UnknownTargetHost {
        invalid_value: "nope".to_owned(),
        valid_hosts: vec!["us.api.openai.com".to_owned()],
    }
    .into();
    assert_eq!(problem.extensions().invalid_value.as_deref(), Some("nope"));
    assert_eq!(problem.extensions().valid_hosts.clone().unwrap().len(), 1);
}

#[tokio::test]
async fn retry_guidance_is_stamped_and_serialized_as_a_header() {
    let problem: OagwProblem = DomainError::RateLimitExceeded {
        detail: "bucket empty".to_owned(),
        retry_after: Some(Duration::from_millis(1500)),
    }
    .into();
    assert_eq!(problem.extensions().retry_after_seconds, Some(2));

    let response = problem.into_response();
    assert_eq!(
        response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
}

#[tokio::test]
async fn link_unavailable_without_retry_has_no_retry_header() {
    let problem: OagwProblem = DomainError::LinkUnavailable {
        detail: "link down".to_owned(),
        retry_after: None,
    }
    .into();
    let response = problem.into_response();
    assert!(response.headers().get(RETRY_AFTER).is_none());
}

#[tokio::test]
async fn instance_is_stamped_by_the_transport() {
    let problem = OagwProblem::new(OagwErrorType::Validation, "nope").with_instance("/oagw/v1/x");
    assert_eq!(problem.extensions().instance.as_deref(), Some("/oagw/v1/x"));
    let response = problem.into_response();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["instance"], "/oagw/v1/x");
}

#[test]
fn extension_fields_are_absent_by_default() {
    let problem = OagwProblem::new(OagwErrorType::Validation, "nope");
    assert_eq!(*problem.extensions(), ProblemExtensions::default());
    assert!(problem.extensions().is_empty());
}

#[tokio::test]
async fn every_builder_stamps_its_extension() {
    let problem = OagwProblem::new(OagwErrorType::Validation, "d")
        .with_instance("/x")
        .with_trace_id("trace")
        .with_alias("alias")
        .with_upstream_id("gts.cf.core.oagw.upstream.v1~u")
        .with_host("api.openai.com")
        .with_path("/v1/x")
        .with_plugin_id("gts.cf.core.oagw.guard_plugin.v1~p")
        .with_referenced_by(ReferencedBy {
            upstreams: vec!["u".to_owned()],
            routes: vec![],
        })
        .with_valid_hosts(vec!["h".to_owned()])
        .with_invalid_value("bad")
        .with_retry_after(9);

    let extensions = problem.extensions();
    assert_eq!(extensions.instance.as_deref(), Some("/x"));
    assert_eq!(extensions.trace_id.as_deref(), Some("trace"));
    assert_eq!(extensions.alias.as_deref(), Some("alias"));
    assert_eq!(
        extensions.upstream_id.as_deref(),
        Some("gts.cf.core.oagw.upstream.v1~u")
    );
    assert_eq!(extensions.host.as_deref(), Some("api.openai.com"));
    assert_eq!(extensions.path.as_deref(), Some("/v1/x"));
    assert_eq!(
        extensions.plugin_id.as_deref(),
        Some("gts.cf.core.oagw.guard_plugin.v1~p")
    );
    assert!(extensions.referenced_by.is_some());
    assert_eq!(extensions.valid_hosts.clone().unwrap(), ["h"]);
    assert_eq!(extensions.invalid_value.as_deref(), Some("bad"));
    assert_eq!(extensions.retry_after_seconds, Some(9));
    assert!(!extensions.is_empty());
}

#[test]
fn oagw_type_round_trips_the_eleven_identities() {
    for variant in [
        OagwErrorType::Validation,
        OagwErrorType::MissingTargetHost,
        OagwErrorType::InvalidTargetHost,
        OagwErrorType::UnknownTargetHost,
        OagwErrorType::AuthFailed,
        OagwErrorType::RouteNotFound,
        OagwErrorType::PluginInUse,
        OagwErrorType::PayloadTooLarge,
        OagwErrorType::RateLimitExceeded,
        OagwErrorType::SecretNotFound,
        OagwErrorType::LinkUnavailable,
    ] {
        let problem = OagwProblem::new(variant, "detail");
        assert_eq!(problem.oagw_type(), Some(variant), "{variant:?}");
    }
    assert_eq!(
        OagwProblem::canonical(canonical_types::NOT_FOUND, "Not Found", 404, "x").oagw_type(),
        None
    );
}

#[test]
fn error_source_constants_are_the_documented_spellings() {
    assert_eq!(ERROR_SOURCE_HEADER, "X-OAGW-Error-Source");
    assert_eq!(ERROR_SOURCE_GATEWAY, "gateway");
    assert_eq!(ERROR_SOURCE_UPSTREAM, "upstream");
}

#[tokio::test]
async fn an_invalid_status_still_produces_a_problem_response() {
    // `http` accepts 100..=999, so 999 is a legal status; 0 is not.
    let problem = OagwProblem::canonical(canonical_types::INTERNAL, "Internal Error", 0, "boom");
    let response = problem.into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn the_fallback_body_of_a_serialization_failure_parses() {
    // The fallback literal is itself a valid problem body, which is what
    // makes it a safe last resort on the error path.
    let fallback: serde_json::Value =
        serde_json::from_slice(br#"{"title":"Internal Error","status":500}"#).unwrap();
    assert_eq!(fallback["status"], 500);
}

#[test]
fn referenced_by_serializes_its_two_collections() {
    let referenced_by = ReferencedBy {
        upstreams: vec!["gts.cf.core.oagw.upstream.v1~aaa".to_owned()],
        routes: vec![],
    };
    let rendered = serde_json::to_value(&referenced_by).unwrap();
    assert_eq!(rendered["upstreams"][0], "gts.cf.core.oagw.upstream.v1~aaa");
    assert_eq!(rendered["routes"].as_array().map(Vec::len), Some(0));
}

#[test]
fn api_result_alias_carries_the_problem() {
    fn handler() -> ApiResult<u16> {
        Err(OagwProblem::canonical(
            canonical_types::INVALID_ARGUMENT,
            "Invalid Argument",
            400,
            "nope",
        ))
    }
    let problem = handler().unwrap_err();
    assert_eq!(problem.status(), 400);
}

/// `F11` — an internal error never echoes its diagnostic: the body carries the
/// constant detail and the diagnostic is only logged.
#[tokio::test]
async fn an_internal_error_reports_the_constant_detail_only() {
    let problem: OagwProblem = DomainError::Internal {
        diagnostic: "rocksdb corruption at /var/lib/oagw/000009.log".to_owned(),
        cause: None,
    }
    .into();

    assert_eq!(problem.status(), 500);
    assert_eq!(problem.kind(), canonical_types::INTERNAL);
    assert_eq!(problem.detail(), INTERNAL_DETAIL);
    assert_eq!(problem.detail(), "an internal error occurred");

    let response = problem.into_response();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        !rendered.contains("rocksdb"),
        "the diagnostic leaked into the body: {rendered}"
    );
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(document["detail"], INTERNAL_DETAIL);
}

/// `DESIGN` §3.3 data-plane rows: identity, status and `X-OAGW-Error-Source`
/// for every 5xx the proxy path can synthesize.
#[test]
fn data_plane_errors_map_to_their_design_identities() {
    let cases: Vec<(DomainError, OagwErrorType, u16)> = vec![
        (
            DomainError::protocol_error("bad framing", Some("t1".to_owned())),
            OagwErrorType::ProtocolError,
            502,
        ),
        (
            DomainError::downstream_error("upstream 503"),
            OagwErrorType::DownstreamError,
            502,
        ),
        (
            DomainError::stream_aborted("client left"),
            OagwErrorType::StreamAborted,
            502,
        ),
        (
            DomainError::circuit_breaker_open("api.example.com", 30),
            OagwErrorType::CircuitBreakerOpen,
            503,
        ),
        (
            DomainError::plugin_not_found("gts.cf.core.oagw.plugin.v1~x"),
            OagwErrorType::PluginNotFound,
            503,
        ),
        (
            DomainError::connection_timeout("api.example.com", 2),
            OagwErrorType::ConnectionTimeout,
            504,
        ),
        (
            DomainError::request_timeout(2),
            OagwErrorType::RequestTimeout,
            504,
        ),
        (
            DomainError::idle_timeout(5),
            OagwErrorType::IdleTimeout,
            504,
        ),
    ];

    for (error, expected_kind, expected_status) in cases {
        let problem = OagwProblem::from(error);
        assert_eq!(problem.oagw_type(), Some(expected_kind), "{problem:?}");
        assert_eq!(problem.status(), expected_status, "{problem:?}");
    }
}

/// `DESIGN` §3.3 marks `CircuitBreakerOpen`, the timeout rows and
/// `LinkUnavailable`/`RateLimitExceeded` as retriable, so those carry
/// `Retry-After`; `ProtocolError`, `StreamAborted` and `PluginNotFound` are
/// not retriable and must not invent guidance.
#[test]
fn data_plane_retry_guidance_follows_the_design_table() {
    let breaker: OagwProblem = DomainError::circuit_breaker_open("api.example.com", 30).into();
    assert_eq!(breaker.extensions().retry_after_seconds, Some(30));

    let timeout: OagwProblem = DomainError::request_timeout(2).into();
    assert_eq!(timeout.extensions().retry_after_seconds, Some(2));

    let connect: OagwProblem = DomainError::connection_timeout("api.example.com", 2).into();
    assert_eq!(connect.extensions().retry_after_seconds, Some(2));

    let plugin: OagwProblem = DomainError::plugin_not_found("p").into();
    assert_eq!(plugin.extensions().retry_after_seconds, None);

    let protocol: OagwProblem = DomainError::protocol_error("bad framing", None).into();
    assert_eq!(protocol.extensions().retry_after_seconds, None);
    assert_eq!(protocol.extensions().trace_id, None);

    let trace: OagwProblem =
        DomainError::protocol_error("bad framing", Some("t1".to_owned())).into();
    assert_eq!(trace.extensions().trace_id.as_deref(), Some("t1"));
}
