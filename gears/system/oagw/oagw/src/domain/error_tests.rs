//! Tests for the OAGW error catalogue (DESIGN §3.3 error table).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use super::*;

const ERR_TABLE: [(&str, u16, bool); 20] = [
    (
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        400,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
        400,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
        400,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
        400,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
        401,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        404,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
        409,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
        413,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
        429,
        true,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
        500,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
        502,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
        502,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
        502,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        503,
        true,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
        503,
        true,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
        503,
        false,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
        504,
        true,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
        504,
        true,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        504,
        true,
    ),
    (
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        400,
        false,
    ),
];

/// Every data-plane catalogue row must be reachable and carry the exact GTS
/// type id, status and retriable flag spelled out in DESIGN §3.3.
#[test]
fn catalogue_matches_design_error_table() {
    let observed: Vec<(&str, u16, bool)> = [
        DomainError::ValidationError {
            detail: "x".to_owned(),
            invalid_value: None,
            alias: None,
        },
        DomainError::MissingTargetHost {
            upstream_id: None,
            path: None,
            valid_hosts: Vec::new(),
        },
        DomainError::InvalidTargetHost {
            invalid_value: "x".to_owned(),
            upstream_id: None,
        },
        DomainError::UnknownTargetHost {
            invalid_value: "x".to_owned(),
            upstream_id: None,
            valid_hosts: Vec::new(),
        },
        DomainError::AuthenticationFailed {
            detail: "x".to_owned(),
            upstream_id: None,
            host: None,
        },
        DomainError::RouteNotFound { path: None },
        DomainError::PluginInUse {
            plugin_id: "p".to_owned(),
            referenced_by: ReferencedBy::empty(),
        },
        DomainError::PayloadTooLarge { limit_bytes: 1 },
        DomainError::RateLimitExceeded {
            retry_after_seconds: 15,
            upstream_id: None,
            host: None,
            path: None,
        },
        DomainError::SecretNotFound {
            detail: "x".to_owned(),
            upstream_id: None,
        },
        DomainError::ProtocolError {
            detail: "x".to_owned(),
            upstream_id: None,
            host: None,
        },
        DomainError::DownstreamError {
            detail: "x".to_owned(),
            upstream_status: 500,
            upstream_id: None,
            host: None,
        },
        DomainError::StreamAborted {
            detail: "x".to_owned(),
            upstream_id: None,
        },
        DomainError::LinkUnavailable {
            upstream_id: None,
            host: None,
        },
        DomainError::CircuitBreakerOpen {
            upstream_id: None,
            host: None,
        },
        DomainError::PluginNotFound {
            plugin_id: "p".to_owned(),
            detail: "x".to_owned(),
            upstream_id: None,
        },
        DomainError::ConnectionTimeout {
            upstream_id: None,
            host: None,
        },
        DomainError::RequestTimeout {
            timeout_seconds: 30,
            upstream_id: None,
        },
        DomainError::IdleTimeout {
            timeout_seconds: 30,
            upstream_id: None,
        },
        DomainError::RouteError {
            detail: "x".to_owned(),
            invalid_value: None,
        },
    ]
    .into_iter()
    .map(|err| (err.gts_type(), err.status(), err.meta().retriable))
    .collect();

    assert_eq!(observed.len(), ERR_TABLE.len());
    for (idx, (gts_type, status, retriable)) in observed.iter().enumerate() {
        let (want_type, want_status, want_retriable) = ERR_TABLE[idx];
        assert_eq!(*gts_type, want_type, "row {idx} type");
        assert_eq!(*status, want_status, "row {idx} status");
        assert_eq!(*retriable, want_retriable, "row {idx} retriable");
    }
}

#[test]
fn management_errors_use_catalogue_naming() {
    let upstream = DomainError::NotFound {
        resource: ResourceKind::Upstream,
        id: "abc".to_owned(),
        detail: "missing".to_owned(),
    };
    assert_eq!(upstream.status(), 404);
    assert_eq!(upstream.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1");
    assert_eq!(upstream.title(), "Upstream Not Found");

    let plugin = DomainError::NotFound {
        resource: ResourceKind::Plugin,
        id: "abc".to_owned(),
        detail: "missing".to_owned(),
    };
    assert_eq!(plugin.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");

    let conflict = DomainError::AliasConflict {
        alias: "api.example.com".to_owned(),
        existing_upstream_id: Uuid::nil(),
    };
    assert_eq!(conflict.status(), 409);
    assert_eq!(conflict.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1");

    let route_conflict = DomainError::Conflict {
        detail: "duplicate".to_owned(),
        invalid_value: None,
    };
    assert_eq!(route_conflict.status(), 409);
}

#[test]
fn retriable_errors_carry_retry_after() {
    let limited = DomainError::RateLimitExceeded {
        retry_after_seconds: 15,
        upstream_id: None,
        host: None,
        path: None,
    };
    assert_eq!(limited.retry_after_seconds(), Some(15));

    let breaker = DomainError::CircuitBreakerOpen {
        upstream_id: None,
        host: None,
    };
    assert!(breaker.retry_after_seconds().is_some());

    let validation = DomainError::validation("bad");
    assert_eq!(validation.retry_after_seconds(), None);
}

#[test]
fn extensions_expose_adr_0007_members() {
    let err = DomainError::UnknownTargetHost {
        invalid_value: "api.example.net".to_owned(),
        upstream_id: Some(Uuid::nil()),
        valid_hosts: vec!["api.openai.com".to_owned()],
    };
    let extensions = err.extensions();
    let fields = extensions.iter_json();
    let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"upstream_id"));
    assert!(names.contains(&"valid_hosts"));
    assert!(names.contains(&"invalid_value"));
}

#[test]
fn oagw_error_type_uses_canonical_prefix() {
    assert_eq!(
        oagw_error_type("demo"),
        "gts.cf.core.errors.err.v1~cf.oagw.demo.v1"
    );
}
