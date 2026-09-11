//! Unit tests for the error table mapping.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::time::Duration;

/// The status, fully typed identifier and title a condition must map to.
type Mapped = (u16, String, &'static str);

fn expected(status: u16, type_id: &'static str, title: &'static str) -> Mapped {
    (status, type_id.to_owned(), title)
}

#[test]
fn error_table_maps_every_type() {
    let cases: Vec<(OagwError, Mapped)> = vec![
        (
            OagwError::ValidationError("bad alias".to_owned()),
            expected(
                400,
                "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                "Validation Error",
            ),
        ),
        (
            OagwError::RouteError("bad route".to_owned()),
            expected(
                400,
                "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                "Route Error",
            ),
        ),
        (
            OagwError::MissingTargetHost("h".to_owned()),
            expected(
                400,
                "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
                "Missing Target Host",
            ),
        ),
        (
            OagwError::InvalidTargetHost("h".to_owned()),
            expected(
                400,
                "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
                "Invalid Target Host",
            ),
        ),
        (
            OagwError::UnknownTargetHost("h".to_owned()),
            expected(
                400,
                "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
                "Unknown Target Host",
            ),
        ),
        (
            OagwError::AuthenticationFailed("no".to_owned()),
            expected(
                401,
                "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
                "Authentication Failed",
            ),
        ),
        (
            OagwError::RouteNotFound("no".to_owned()),
            expected(
                404,
                "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
                "Route Not Found",
            ),
        ),
        (
            OagwError::PluginInUse("p".to_owned()),
            expected(
                409,
                "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
                "Plugin In Use",
            ),
        ),
        (
            OagwError::PayloadTooLarge("big".to_owned()),
            expected(
                413,
                "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
                "Payload Too Large",
            ),
        ),
        (
            OagwError::RateLimitExceeded {
                detail: "r".to_owned(),
                retry_after: Duration::from_secs(3),
            },
            expected(
                429,
                "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
                "Rate Limit Exceeded",
            ),
        ),
        (
            OagwError::SecretNotFound("s".to_owned()),
            expected(
                500,
                "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
                "Secret Not Found",
            ),
        ),
        (
            OagwError::ProtocolError("p".to_owned()),
            expected(
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
                "Protocol Error",
            ),
        ),
        (
            OagwError::DownstreamError("d".to_owned()),
            expected(
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
                "Downstream Error",
            ),
        ),
        (
            OagwError::StreamAborted("s".to_owned()),
            expected(
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
                "Stream Aborted",
            ),
        ),
        (
            OagwError::LinkUnavailable("l".to_owned()),
            expected(
                503,
                "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
                "Link Unavailable",
            ),
        ),
        (
            OagwError::CircuitBreakerOpen("c".to_owned()),
            expected(
                503,
                "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
                "Circuit Breaker Open",
            ),
        ),
        (
            OagwError::PluginNotFound("p".to_owned()),
            expected(
                503,
                "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
                "Plugin Not Found",
            ),
        ),
        (
            OagwError::ConnectionTimeout("t".to_owned()),
            expected(
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
                "Connection Timeout",
            ),
        ),
        (
            OagwError::RequestTimeout("t".to_owned()),
            expected(
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
                "Request Timeout",
            ),
        ),
        (
            OagwError::IdleTimeout("t".to_owned()),
            expected(
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
                "Idle Timeout",
            ),
        ),
    ];

    for (err, (status, type_id, title)) in cases {
        assert_eq!(err.status(), status, "{err:?} status");
        assert_eq!(&err.type_id(), &type_id, "{err:?} type");
        assert_eq!(err.title(), title, "{err:?} title");
    }
}

#[test]
fn retriable_rows_report_retry_guidance() {
    let rate = OagwError::RateLimitExceeded {
        detail: "exhausted".to_owned(),
        retry_after: Duration::from_secs(7),
    };
    assert!(rate.retriable());
    assert_eq!(rate.retry_after(), Some(Duration::from_secs(7)));

    for err in [
        OagwError::LinkUnavailable("x".to_owned()),
        OagwError::CircuitBreakerOpen("x".to_owned()),
        OagwError::ConnectionTimeout("x".to_owned()),
        OagwError::RequestTimeout("x".to_owned()),
        OagwError::IdleTimeout("x".to_owned()),
    ] {
        assert!(err.retriable());
        assert!(err.retry_after().is_some());
    }
}

#[test]
fn non_retriable_rows_report_no_guidance() {
    for err in [
        OagwError::ValidationError("x".to_owned()),
        OagwError::RouteNotFound("x".to_owned()),
        OagwError::PayloadTooLarge("x".to_owned()),
        OagwError::PluginInUse("x".to_owned()),
        OagwError::DownstreamError("x".to_owned()),
    ] {
        assert!(!err.retriable());
        assert!(err.retry_after().is_none());
    }
}

#[test]
fn secret_touching_errors_are_marked() {
    assert!(OagwError::SecretNotFound("cred://x".to_owned()).touches_secret());
    assert!(OagwError::AuthenticationFailed("bad".to_owned()).touches_secret());
    assert!(!OagwError::ValidationError("bad".to_owned()).touches_secret());
}

#[test]
fn domain_error_maps_onto_the_wire_vocabulary() {
    assert_eq!(
        OagwError::from(DomainError::Invalid("nope".to_owned())).status(),
        400
    );
    assert_eq!(
        OagwError::from(DomainError::NotFound {
            kind: "upstream".to_owned(),
            target: "u".to_owned()
        })
        .status(),
        404
    );
    assert_eq!(
        OagwError::from(DomainError::Conflict("dup".to_owned())).status(),
        409
    );
}

#[test]
fn detail_is_carried_through() {
    let err = OagwError::ValidationError("alias mismatch".to_owned());
    assert_eq!(err.detail(), "alias mismatch");
    assert!(err.to_string().contains("Validation Error"));
}
