#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::{DomainError, OagwErrorType};

/// The eleven `cf.oagw.*` identities mandated by `DESIGN` §3.3, asserted
/// verbatim: these strings are the wire contract, so a spelling drift here is
/// a breaking change for every client that switches on `type`.
#[test]
fn oagw_error_types_project_the_design_table_verbatim() {
    let expected = [
        (
            OagwErrorType::Validation,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            400,
            "Validation Error",
        ),
        (
            OagwErrorType::MissingTargetHost,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
            400,
            "Missing Target Host Header",
        ),
        (
            OagwErrorType::InvalidTargetHost,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
            400,
            "Invalid Target Host Format",
        ),
        (
            OagwErrorType::UnknownTargetHost,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
            400,
            "Unknown Target Host",
        ),
        (
            OagwErrorType::AuthFailed,
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            401,
            "Authentication Failed",
        ),
        (
            OagwErrorType::RouteNotFound,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            404,
            "Route Not Found",
        ),
        (
            OagwErrorType::PluginInUse,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            409,
            "Plugin In Use",
        ),
        (
            OagwErrorType::PayloadTooLarge,
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            413,
            "Payload Too Large",
        ),
        (
            OagwErrorType::RateLimitExceeded,
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            429,
            "Rate Limit Exceeded",
        ),
        (
            OagwErrorType::SecretNotFound,
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            500,
            "Secret Not Found",
        ),
        (
            OagwErrorType::LinkUnavailable,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            503,
            "Link Unavailable",
        ),
    ];

    for (variant, expected_type, expected_status, expected_title) in expected {
        assert_eq!(variant.gts_id(), expected_type, "{variant:?}");
        assert_eq!(variant.status(), expected_status, "{variant:?}");
        assert_eq!(variant.title(), expected_title, "{variant:?}");
    }
}

#[test]
fn proxy_identities_are_partitioned_from_management_identities() {
    assert!(!OagwErrorType::Validation.is_proxy_error());
    for variant in [
        OagwErrorType::MissingTargetHost,
        OagwErrorType::InvalidTargetHost,
        OagwErrorType::UnknownTargetHost,
        OagwErrorType::AuthFailed,
        OagwErrorType::RouteNotFound,
        OagwErrorType::PayloadTooLarge,
        OagwErrorType::RateLimitExceeded,
        OagwErrorType::SecretNotFound,
        OagwErrorType::LinkUnavailable,
    ] {
        assert!(variant.is_proxy_error(), "{variant:?}");
    }
}

#[test]
fn constructors_box_their_cause() {
    let cause = std::io::Error::other("boom");
    let internal = DomainError::internal("upstream write failed", cause);
    assert!(matches!(internal, DomainError::Internal { .. }));

    let cause = std::io::Error::other("boom");
    let unavailable = DomainError::unavailable("dependency down", cause);
    assert!(matches!(
        unavailable,
        DomainError::ServiceUnavailable { .. }
    ));

    assert!(matches!(
        DomainError::validation("bad alias"),
        DomainError::Validation { .. }
    ));
}

/// `DESIGN` §3.3 rows that only the data plane can produce. The dispatch
/// contract fixes their statuses (502 / 503 / 504) and the table fixes the
/// instance ids, so both are asserted verbatim here.
#[test]
fn data_plane_identities_project_the_design_table_verbatim() {
    let expected = [
        (
            OagwErrorType::ProtocolError,
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            502,
            "Protocol Error",
        ),
        (
            OagwErrorType::DownstreamError,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            502,
            "Downstream Error",
        ),
        (
            OagwErrorType::StreamAborted,
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            502,
            "Stream Aborted",
        ),
        (
            OagwErrorType::CircuitBreakerOpen,
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            503,
            "Circuit Breaker Open",
        ),
        (
            OagwErrorType::PluginNotFound,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            503,
            "Plugin Not Found",
        ),
        (
            OagwErrorType::ConnectionTimeout,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            504,
            "Connection Timeout",
        ),
        (
            OagwErrorType::RequestTimeout,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            504,
            "Request Timeout",
        ),
        (
            OagwErrorType::IdleTimeout,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
            504,
            "Idle Timeout",
        ),
    ];

    for (variant, expected_type, expected_status, expected_title) in expected {
        assert_eq!(variant.gts_id(), expected_type, "{variant:?}");
        assert_eq!(variant.status(), expected_status, "{variant:?}");
        assert_eq!(variant.title(), expected_title, "{variant:?}");
        assert!(variant.is_proxy_error(), "{variant:?}");
    }
}

/// The proxy/management partition is still total: no identity is both.
#[test]
fn management_identities_stay_out_of_the_proxy_partition() {
    for variant in [OagwErrorType::Validation, OagwErrorType::PluginInUse] {
        assert!(!variant.is_proxy_error(), "{variant:?}");
    }
}

#[test]
fn data_plane_constructors_carry_their_context() {
    let protocol =
        DomainError::protocol_error("malformed chunked framing", Some("trace-1".to_owned()));
    match &protocol {
        DomainError::ProtocolError { detail, trace_id } => {
            assert_eq!(detail.as_str(), "malformed chunked framing");
            assert_eq!(trace_id.as_deref(), Some("trace-1"));
        }
        other => panic!("unexpected {other:?}"),
    }

    let downstream = DomainError::downstream_error("upstream answered 503");
    assert!(matches!(downstream, DomainError::DownstreamError { .. }));

    let aborted = DomainError::stream_aborted("client went away mid-body");
    assert!(matches!(aborted, DomainError::StreamAborted { .. }));

    let breaker = DomainError::circuit_breaker_open("api.example.com", 30);
    match &breaker {
        DomainError::CircuitBreakerOpen { alias, retry_after } => {
            assert_eq!(alias.as_str(), "api.example.com");
            assert_eq!(*retry_after, 30);
        }
        other => panic!("unexpected {other:?}"),
    }

    let missing = DomainError::plugin_not_found("gts.cf.core.oagw.plugin.v1~x");
    match &missing {
        DomainError::PluginNotFound { plugin_id } => {
            assert_eq!(plugin_id.as_str(), "gts.cf.core.oagw.plugin.v1~x");
        }
        other => panic!("unexpected {other:?}"),
    }

    let connect = DomainError::connection_timeout("api.example.com", 2);
    assert!(matches!(connect, DomainError::ConnectionTimeout { .. }));

    let request = DomainError::request_timeout(2);
    assert!(matches!(request, DomainError::RequestTimeout { .. }));

    let idle = DomainError::idle_timeout(5);
    assert!(matches!(idle, DomainError::IdleTimeout { .. }));
}
