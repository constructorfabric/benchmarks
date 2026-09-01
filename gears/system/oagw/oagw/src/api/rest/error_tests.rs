//! Tests for the RFC 9457 problem mapping.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::response::IntoResponse;

use super::OagwProblem;
use crate::domain::error::{DomainError, ReferencedBy};

#[test]
fn maps_the_twenty_contract_types_to_their_gts_identifiers() {
    let cases: Vec<(DomainError, u16, &str)> = vec![
        (
            DomainError::Validation("bad".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        ),
        (
            DomainError::MissingTargetHost("x".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
        ),
        (
            DomainError::InvalidTargetHost("x".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
        ),
        (
            DomainError::UnknownTargetHost("x".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
        ),
        (
            DomainError::AuthenticationFailed("x".into()),
            401,
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
        ),
        (
            DomainError::RouteNotFound("x".into()),
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        ),
        (
            DomainError::PayloadTooLarge("x".into()),
            413,
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
        ),
        (
            DomainError::RateLimitExceeded {
                detail: "x".into(),
                retry_after_seconds: 3,
            },
            429,
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
        ),
        (
            DomainError::Conflict("x".into()),
            409,
            "gts.cf.core.errors.err.v1~cf.core.err.already_exists.v1",
        ),
        (
            DomainError::SecretNotFound("x".into()),
            500,
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
        ),
        (
            DomainError::ProtocolError("x".into()),
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
        ),
        (
            DomainError::DownstreamError("x".into()),
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
        ),
        (
            DomainError::StreamAborted("x".into()),
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
        ),
        (
            DomainError::LinkUnavailable {
                detail: "x".into(),
                retry_after_seconds: 1,
            },
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        ),
        (
            DomainError::CircuitBreakerOpen {
                detail: "x".into(),
                retry_after_seconds: 1,
            },
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
        ),
        (
            DomainError::PluginNotFound("x".into()),
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
        ),
        (
            DomainError::ConnectionTimeout("x".into()),
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
        ),
        (
            DomainError::RequestTimeout("x".into()),
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
        ),
        (
            DomainError::IdleTimeout("x".into()),
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        ),
        (
            DomainError::CorsOriginNotAllowed("x".into()),
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
        ),
        (
            DomainError::CorsMethodNotAllowed("x".into()),
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
        ),
    ];

    assert_eq!(cases.len(), 21);
    for (error, expected_status, expected_type) in cases {
        let response = OagwProblem::new(error.clone()).into_response();
        assert_eq!(response.status().as_u16(), expected_status, "{error:?}");
        assert_eq!(
            response
                .headers()
                .get(super::ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("gateway")
        );
        // RFC 9457 §3: a problem carries `application/problem+json`, not the
        // `application/json` an axum `Json` responder would emit.
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(super::PROBLEM_MEDIA_TYPE),
            "{error:?}"
        );
        let body = super::problem_body(&error);
        assert_eq!(body["type"], expected_type);
        assert_eq!(body["status"], expected_status);
        assert!(!body["detail"].as_str().unwrap_or_default().is_empty());
    }
}

#[test]
fn every_problem_names_the_oagw_error_domain_and_code() {
    let errors = [
        DomainError::Validation("x".into()),
        DomainError::Conflict("x".into()),
        DomainError::PayloadTooLarge("x".into()),
        DomainError::UpstreamDisabled {
            alias: "api.example.com".into(),
            retry_after_seconds: 5,
        },
        DomainError::CorsOriginNotAllowed("x".into()),
    ];
    for error in errors {
        let body = super::problem_body(&error);
        assert_eq!(body["error_domain"], "oagw.v1", "{error:?}");
        assert!(!body["error_code"].as_str().unwrap_or_default().is_empty());
        assert!(body["context"].is_object(), "{error:?}");
    }
}

#[test]
fn every_problem_carries_the_request_uri_as_instance() {
    let error = DomainError::RouteNotFound("no route".into());
    let body = OagwProblem::at("/api/oagw/v1/proxy/api.example.com/v1/chat", error.clone()).body();
    assert_eq!(
        body["instance"],
        "/api/oagw/v1/proxy/api.example.com/v1/chat"
    );
    assert_eq!(body["path"], "/api/oagw/v1/proxy/api.example.com/v1/chat");

    // Without an instance the field is omitted rather than rendered empty.
    let bare = super::problem_body(&error);
    assert!(bare.get("instance").is_none());
}

#[test]
fn data_plane_problems_carry_the_alias_host_and_path() {
    let error = DomainError::RouteNotFound("no route".into());
    let problem = OagwProblem::for_upstream("api.example.com", "/v1/chat", error.clone());
    let body = problem.body();
    assert_eq!(body["host"], "api.example.com");
    assert_eq!(body["path"], "/v1/chat");
    // `instance` is the client-visible URI and is supplied by the handler on
    // top of the upstream-relative `path`, so it is absent here.
    assert!(body.get("instance").is_none());

    // An explicit instance (the full client-visible URI) wins over the
    // upstream-relative path for `instance`, while `path` keeps the suffix.
    let body = problem
        .instance("/api/oagw/v1/proxy/api.example.com/v1/chat".to_owned())
        .body();
    assert_eq!(
        body["instance"],
        "/api/oagw/v1/proxy/api.example.com/v1/chat"
    );
    assert_eq!(body["path"], "/v1/chat");
}

#[test]
fn retriable_errors_carry_retry_after() {
    let response = OagwProblem::new(DomainError::RateLimitExceeded {
        detail: "x".into(),
        retry_after_seconds: 9,
    })
    .into_response();
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("9")
    );
    let response = OagwProblem::new(DomainError::Validation("x".into())).into_response();
    assert!(response.headers().get("retry-after").is_none());
}

#[test]
fn plugin_in_use_reports_the_referencing_resources() {
    let error = DomainError::PluginInUse {
        detail: "in use".into(),
        plugin_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1".into(),
        referenced_by: ReferencedBy {
            upstreams: vec!["api.example.com".into()],
            routes: vec!["route-1".into()],
        },
    };
    let body = super::problem_body(&error);
    assert_eq!(body["error_code"], "PLUGIN_IN_USE");
    assert_eq!(
        body["context"]["referenced_by"]["upstreams"][0],
        "api.example.com"
    );
    // The plugin identity is duplicated at the top level so clients can read
    // it without knowing the `context` extension shape.
    assert_eq!(
        body["plugin_id"],
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(body["referenced_by"]["upstreams"][0], "api.example.com");
    assert_eq!(body["referenced_by"]["routes"][0], "route-1");
}

#[test]
fn a_validation_rejection_keeps_the_extractor_status() {
    use axum::http::StatusCode;

    // Malformed JSON is reported as the canonical validation problem while
    // keeping axum's own status code.
    let problem = super::rejection_problem(
        Some("/api/oagw/v1/upstreams".to_owned()),
        StatusCode::BAD_REQUEST,
        "Failed to parse the request body as JSON".to_owned(),
    );
    assert_eq!(problem.status(), StatusCode::BAD_REQUEST);
    let body = problem.body();
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["error_code"], "VALIDATION_FAILED");
    assert_eq!(body["instance"], "/api/oagw/v1/upstreams");

    // A body that parses but does not match the declared shape is 422.
    let problem = super::rejection_problem(
        None,
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing field `server`".to_owned(),
    );
    assert_eq!(problem.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        super::problem_body(&DomainError::Validation("x".into()))
            .get("instance")
            .is_none()
    );
}
