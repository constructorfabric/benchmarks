//! The error catalog's status / type / retriability table.

use super::*;

#[test]
fn the_catalog_matches_the_documented_status_codes() {
    for (kind, status) in [
        (ErrorKind::ValidationError, 400),
        (ErrorKind::MissingTargetHost, 400),
        (ErrorKind::InvalidTargetHost, 400),
        (ErrorKind::UnknownTargetHost, 400),
        (ErrorKind::AuthenticationFailed, 401),
        (ErrorKind::CorsOriginNotAllowed, 403),
        (ErrorKind::CorsMethodNotAllowed, 403),
        (ErrorKind::RouteNotFound, 404),
        (ErrorKind::PluginInUse, 409),
        (ErrorKind::PayloadTooLarge, 413),
        (ErrorKind::RateLimitExceeded, 429),
        (ErrorKind::SecretNotFound, 500),
        (ErrorKind::ProtocolError, 502),
        (ErrorKind::DownstreamError, 502),
        (ErrorKind::StreamAborted, 502),
        (ErrorKind::LinkUnavailable, 503),
        (ErrorKind::CircuitBreakerOpen, 503),
        (ErrorKind::PluginNotFound, 503),
        (ErrorKind::ConnectionTimeout, 504),
        (ErrorKind::RequestTimeout, 504),
        (ErrorKind::IdleTimeout, 504),
    ] {
        assert_eq!(kind.status(), status, "{kind:?}");
    }
}

#[test]
fn the_catalog_matches_the_documented_gts_identifiers() {
    assert_eq!(
        ErrorKind::RouteNotFound.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(
        ErrorKind::RateLimitExceeded.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(
        ErrorKind::MissingTargetHost.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
}

#[test]
fn retriability_follows_the_prd_table() {
    assert!(ErrorKind::RateLimitExceeded.retriable());
    assert!(ErrorKind::CircuitBreakerOpen.retriable());
    assert!(ErrorKind::RequestTimeout.retriable());
    assert!(!ErrorKind::ValidationError.retriable());
    assert!(!ErrorKind::RouteNotFound.retriable());
    assert!(!ErrorKind::SecretNotFound.retriable());
}

#[test]
fn a_problem_document_carries_the_rfc_9457_members_and_extensions() {
    let error = OagwError::new(ErrorKind::RateLimitExceeded, "too fast")
        .with("host", "api.openai.com")
        .with_retry_after(15);
    let problem = error.to_problem(Some("/oagw/v1/proxy/api.openai.com/v1/chat"));
    let json = serde_json::to_value(&problem).unwrap();

    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(json["title"], "Rate Limit Exceeded");
    assert_eq!(json["status"], 429);
    assert_eq!(json["detail"], "too fast");
    assert_eq!(json["instance"], "/oagw/v1/proxy/api.openai.com/v1/chat");
    // Extension members are flattened alongside the standard ones.
    assert_eq!(json["host"], "api.openai.com");
    assert_eq!(json["retry_after_seconds"], 15);
}

#[test]
fn retry_after_is_available_both_as_a_member_and_a_header_value() {
    let error = OagwError::new(ErrorKind::LinkUnavailable, "down").with_retry_after(7);
    assert_eq!(error.retry_after_seconds, Some(7));
    assert_eq!(
        error.extensions.get("retry_after_seconds"),
        Some(&serde_json::Value::from(7))
    );
}

#[test]
fn extra_headers_ride_along_with_the_error() {
    let error = OagwError::new(ErrorKind::RateLimitExceeded, "too fast")
        .with_header("X-RateLimit-Limit", "100");
    assert_eq!(
        error.headers,
        vec![("X-RateLimit-Limit".to_owned(), "100".to_owned())]
    );
}

#[test]
fn the_shorthand_constructors_pick_the_right_kind() {
    assert_eq!(OagwError::validation("x").status(), 400);
    assert_eq!(OagwError::not_found("x").status(), 404);
    assert_eq!(OagwError::conflict("x").status(), 409);
    assert_eq!(OagwError::forbidden("x").status(), 403);
    assert_eq!(OagwError::internal("x").status(), 500);
    assert_eq!(OagwError::unavailable("x").status(), 503);
}
