use super::*;

fn assert_row(kind: ErrorKind, status: u16, instance: &'static str) {
    assert_eq!(kind.http_status(), status, "status for {kind:?}");
    assert_eq!(kind.gts_instance(), instance, "gts instance for {kind:?}");
    assert_eq!(
        kind.gts_type(),
        format!("gts.cf.core.errors.err.v1~{instance}")
    );
    assert!(!kind.title().is_empty());
}

#[test]
fn error_catalog_matches_the_contract_table() {
    assert_row(ErrorKind::Validation, 400, "cf.oagw.validation.error.v1");
    assert_row(
        ErrorKind::MissingTargetHost,
        400,
        "cf.oagw.routing.missing_target_host.v1",
    );
    assert_row(
        ErrorKind::InvalidTargetHost,
        400,
        "cf.oagw.routing.invalid_target_host.v1",
    );
    assert_row(
        ErrorKind::UnknownTargetHost,
        400,
        "cf.oagw.routing.unknown_target_host.v1",
    );
    assert_row(
        ErrorKind::AuthenticationFailed,
        401,
        "cf.oagw.auth.failed.v1",
    );
    assert_row(ErrorKind::RouteNotFound, 404, "cf.oagw.route.not_found.v1");
    assert_row(ErrorKind::PluginInUse, 409, "cf.oagw.plugin.in_use.v1");
    assert_row(
        ErrorKind::PayloadTooLarge,
        413,
        "cf.oagw.payload.too_large.v1",
    );
    assert_row(
        ErrorKind::RateLimitExceeded,
        429,
        "cf.oagw.rate_limit.exceeded.v1",
    );
    assert_row(
        ErrorKind::SecretNotFound,
        500,
        "cf.oagw.secret.not_found.v1",
    );
    assert_row(ErrorKind::ProtocolError, 502, "cf.oagw.protocol.error.v1");
    assert_row(
        ErrorKind::DownstreamError,
        502,
        "cf.oagw.downstream.error.v1",
    );
    assert_row(ErrorKind::StreamAborted, 502, "cf.oagw.stream.aborted.v1");
    assert_row(
        ErrorKind::LinkUnavailable,
        503,
        "cf.oagw.link.unavailable.v1",
    );
    assert_row(
        ErrorKind::CircuitBreakerOpen,
        503,
        "cf.oagw.circuit_breaker.open.v1",
    );
    assert_row(
        ErrorKind::PluginNotFound,
        503,
        "cf.oagw.plugin.not_found.v1",
    );
    assert_row(
        ErrorKind::ConnectionTimeout,
        504,
        "cf.oagw.timeout.connection.v1",
    );
    assert_row(ErrorKind::RequestTimeout, 504, "cf.oagw.timeout.request.v1");
    assert_row(ErrorKind::IdleTimeout, 504, "cf.oagw.timeout.idle.v1");
}

#[test]
fn retriable_flags_follow_the_contract() {
    assert!(!ErrorKind::Validation.retriable());
    assert!(ErrorKind::RateLimitExceeded.retriable());
    assert!(ErrorKind::LinkUnavailable.retriable());
    assert!(ErrorKind::CircuitBreakerOpen.retriable());
    assert!(ErrorKind::RequestTimeout.retriable());
    assert!(!ErrorKind::DownstreamError.retriable());
    assert!(!ErrorKind::PluginNotFound.retriable());
}

#[test]
fn constructors_set_kind_and_detail() {
    let error = DomainError::rate_limit_exceeded("too many requests");
    assert_eq!(error.kind, ErrorKind::RateLimitExceeded);
    assert_eq!(error.detail, "too many requests");
    assert_eq!(*error.extensions, super::ErrorExtensions::default());
    assert!(error.to_string().contains("429"));
}

#[test]
fn extensions_carry_routing_context() {
    let extensions = super::ErrorExtensions {
        upstream_id: Some("gts.cf.core.oagw.upstream.v1~abc".to_owned()),
        host: Some("api.openai.com".to_owned()),
        valid_hosts: vec!["us.vendor.com".to_owned()],
        invalid_value: Some("nope".to_owned()),
        ..super::ErrorExtensions::default()
    };
    let error =
        DomainError::new(ErrorKind::UnknownTargetHost, "unknown").with_extensions(extensions);
    assert_eq!(error.extensions.valid_hosts.len(), 1);
    assert_eq!(error.extensions.invalid_value.as_deref(), Some("nope"));
}
