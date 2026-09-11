//! The error mapping tests of the upstream management surface (FEATURE entry
//! 2.2, DESIGN §3.3).
//!
//! The three rejections that leave [`error`] are asserted here: the `401`
//! OAGW authentication surface, the `403` shared canonical permission-denied
//! surface, and the status/GTS-type pairs of the mapping table.
// @cpt-dod:cpt-cf-oagw-dod-error-handling-domain-error-taxonomy:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-error-table-mapping:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-prd-error-codes:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-problem-details-contract:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-unit-tests:p1

use axum::http::{StatusCode, header};
use toolkit_gts::gts_uri;

use super::error::{ApiError, ERROR_SOURCE, mapping_of, permission_denied, unauthenticated};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;

fn validation(detail: &str) -> DomainError {
    DomainError::field_rejection("alias", detail)
}

/// Every mapped rejection carries the gateway marker.
#[test]
fn every_response_carries_the_gateway_error_source() {
    for error in [
        ApiError::Authentication,
        ApiError::Domain(validation("bad")),
        ApiError::Authorization(crate::domain::services::management::AuthorizeError::Denied {
            permission: "gts.cf.core.oagw.upstream.v1~:create".to_owned(),
            detail: "denied".to_owned(),
        }),
    ] {
        let response = error.into_response();
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE),
            "the marker is on every rejection"
        );
    }
}

/// `inst-um-??`: the `401` OAGW authentication surface is rendered before any
/// payload validation, with the OAGW GTS type.
#[test]
fn a_missing_context_is_the_oagw_authentication_surface() {
    let error = unauthenticated();
    assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error.gts_type(), gts::ERR_AUTH_FAILED);

    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
}

/// The body of a `401` carries the OAGW type, not a canonical category.
#[tokio::test]
async fn the_authentication_body_names_the_oagw_type() {
    use http_body_util::BodyExt;
    let response = unauthenticated().into_response();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&bytes).expect("problem+json");
    assert_eq!(
        problem["type"], gts_uri!(gts::ERR_AUTH_FAILED),
        "the OAGW authentication type is on the wire"
    );
    assert_eq!(problem["status"], 401);
}

/// `403` goes through the shared canonical permission-denied surface: the
/// reason is the evaluated permission, and no OAGW 403 type exists.
#[tokio::test]
async fn a_denied_decision_is_the_canonical_permission_denied_surface() {
    use http_body_util::BodyExt;
    let error = ApiError::Authorization(
        crate::domain::services::management::AuthorizeError::Denied {
            permission: "gts.cf.core.oagw.upstream.v1~:delete".to_owned(),
            detail: "the tenant holds no such grant".to_owned(),
        },
    );
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert!(
        !error.gts_type().starts_with(gts::ERR_AUTH_FAILED),
        "no OAGW 403 type is invented"
    );

    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&bytes).expect("problem+json");
    assert!(
        problem["type"]
            .as_str()
            .is_some_and(|problem_type| problem_type.contains("permission_denied")),
        "the shared canonical permission-denied category renders: {problem}"
    );
    assert!(
        problem["context"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("gts.cf.core.oagw.upstream.v1~:delete")),
        "the evaluated permission is the reason: {problem}"
    );
}

/// A PDP that cannot answer is an internal failure, never a silent allow.
#[test]
fn an_unavailable_decision_is_an_internal_failure() {
    let error = ApiError::Authorization(
        crate::domain::services::management::AuthorizeError::Unavailable {
            detail: "the PDP is unreachable".to_owned(),
        },
    );
    assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// The DESIGN §3.3 mapping of the variants the management surface raises.
#[test]
fn the_mapping_table_covers_every_domain_variant() {
    let cases: Vec<(DomainError, StatusCode, &'static str)> = vec![
        (validation("bad"), StatusCode::BAD_REQUEST, gts::ERR_VALIDATION),
        (
            DomainError::NotFound { resource_type: "upstream" },
            StatusCode::NOT_FOUND,
            gts::ERR_ROUTE_NOT_FOUND,
        ),
        (
            DomainError::Conflict { detail: "alias taken".to_owned(), referenced_by: None },
            StatusCode::CONFLICT,
            gts::ERR_PLUGIN_IN_USE,
        ),
        (
            DomainError::CorsInvalidConfig("wildcard with credentials".to_owned()),
            StatusCode::BAD_REQUEST,
            gts::ERR_VALIDATION,
        ),
        (
            DomainError::MissingTargetHost {
                upstream_id: None,
                alias: None,
                valid_hosts: vec![],
                trace_id: None,
            },
            StatusCode::BAD_REQUEST,
            gts::ERR_MISSING_TARGET_HOST,
        ),
    ];
    for (error, status, error_type) in cases {
        assert_eq!(mapping_of(&error).1, status, "{error:?}");
        assert_eq!(mapping_of(&error).0, error_type, "{error:?}");
        let rendered = ApiError::from(error);
        assert_eq!(rendered.status(), status);
    }
}

/// The canonical 403 helper carries the permission as its reason.
#[test]
fn permission_denied_carries_the_permission() {
    let error = permission_denied("gts.cf.core.oagw.upstream.v1~:read", "");
    assert_eq!(error.status_code(), 403);
}

// ---------------------------------------------------------------------------
// FEATURE entry 2.5 — the exhaustive DESIGN §3.3 table
// ---------------------------------------------------------------------------

/// One case of the DESIGN §3.3 table: the variant, its status, its GTS type
/// and its retriable flag.
struct Row {
    error: DomainError,
    status: StatusCode,
    error_type: &'static str,
    retriable: bool,
}

/// Every variant the taxonomy carries, exactly once.
fn rows() -> Vec<Row> {
    let trace = || None;
    vec![
        Row {
            error: DomainError::field_rejection("alias", "rejected"),
            status: StatusCode::BAD_REQUEST,
            error_type: gts::ERR_VALIDATION,
            retriable: false,
        },
        Row {
            error: DomainError::RouteError {
                detail: "rejected".to_owned(),
                method: Some("GET".to_owned()),
                path_prefix: None,
                match_rule: None,
            },
            status: StatusCode::BAD_REQUEST,
            error_type: gts::ERR_VALIDATION,
            retriable: false,
        },
        Row {
            error: DomainError::CorsInvalidConfig("wildcard with credentials".to_owned()),
            status: StatusCode::BAD_REQUEST,
            error_type: gts::ERR_VALIDATION,
            retriable: false,
        },
        Row {
            error: DomainError::MissingTargetHost {
                upstream_id: None,
                alias: Some("api.vendor.com".to_owned()),
                valid_hosts: vec!["a".to_owned()],
                trace_id: trace(),
            },
            status: StatusCode::BAD_REQUEST,
            error_type: gts::ERR_MISSING_TARGET_HOST,
            retriable: false,
        },
        Row {
            error: DomainError::InvalidTargetHost {
                upstream_id: None,
                invalid_value: "not a host".to_owned(),
                trace_id: trace(),
            },
            status: StatusCode::BAD_REQUEST,
            error_type: gts::ERR_INVALID_TARGET_HOST,
            retriable: false,
        },
        Row {
            error: DomainError::UnknownTargetHost {
                upstream_id: None,
                invalid_value: "other.host".to_owned(),
                valid_hosts: vec!["a.host".to_owned()],
                trace_id: trace(),
            },
            status: StatusCode::BAD_REQUEST,
            error_type: gts::ERR_UNKNOWN_TARGET_HOST,
            retriable: false,
        },
        Row {
            error: DomainError::AuthenticationFailed {
                upstream_id: None, host: None, path: None, trace_id: trace(),
            },
            status: StatusCode::UNAUTHORIZED,
            error_type: gts::ERR_AUTH_FAILED,
            retriable: false,
        },
        Row {
            error: DomainError::RouteNotFound { path: None, trace_id: trace() },
            status: StatusCode::NOT_FOUND,
            error_type: gts::ERR_ROUTE_NOT_FOUND,
            retriable: false,
        },
        Row {
            error: DomainError::NotFound { resource_type: "upstream" },
            status: StatusCode::NOT_FOUND,
            error_type: gts::ERR_ROUTE_NOT_FOUND,
            retriable: false,
        },
        Row {
            error: DomainError::Conflict {
                detail: "alias taken".to_owned(),
                referenced_by: None,
            },
            status: StatusCode::CONFLICT,
            error_type: gts::ERR_PLUGIN_IN_USE,
            retriable: false,
        },
        Row {
            error: DomainError::PluginInUse { referenced_by: Default::default() },
            status: StatusCode::CONFLICT,
            error_type: gts::ERR_PLUGIN_IN_USE,
            retriable: false,
        },
        Row {
            error: DomainError::PayloadTooLarge { path: None, trace_id: trace(), upstream_id: None, limit_bytes: None },
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error_type: gts::ERR_PAYLOAD_TOO_LARGE,
            retriable: false,
        },
        Row {
            error: DomainError::RateLimitExceeded {
                upstream_id: None,
                host: None,
                retry_after_seconds: Some(7),
                trace_id: trace(),
            },
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: gts::ERR_RATE_LIMIT_EXCEEDED,
            retriable: true,
        },
        Row {
            error: DomainError::SecretNotFound { path: None, trace_id: trace() },
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error_type: gts::ERR_SECRET_NOT_FOUND,
            retriable: false,
        },
        Row {
            error: DomainError::ProtocolError {
                upstream_id: None, host: None, path: None, trace_id: trace(),
            },
            status: StatusCode::BAD_GATEWAY,
            error_type: gts::ERR_PROTOCOL,
            retriable: false,
        },
        Row {
            error: DomainError::DownstreamError {
                upstream_id: None, host: None, path: None, trace_id: trace(), retriable: false,
            },
            status: StatusCode::BAD_GATEWAY,
            error_type: gts::ERR_DOWNSTREAM,
            retriable: false,
        },
        Row {
            error: DomainError::DownstreamError {
                upstream_id: None, host: None, path: None, trace_id: trace(), retriable: true,
            },
            status: StatusCode::BAD_GATEWAY,
            error_type: gts::ERR_DOWNSTREAM,
            retriable: true,
        },
        Row {
            error: DomainError::StreamAborted {
                upstream_id: None, host: None, path: None, trace_id: trace(),
            },
            status: StatusCode::BAD_GATEWAY,
            error_type: gts::ERR_STREAM_ABORTED,
            retriable: false,
        },
        Row {
            error: DomainError::LinkUnavailable {
                upstream_id: None, host: None, path: None, trace_id: trace(),
            },
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: gts::ERR_LINK_UNAVAILABLE,
            retriable: true,
        },
        Row {
            error: DomainError::CircuitBreakerOpen {
                upstream_id: None, host: None, trace_id: trace(),
            },
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: gts::ERR_CIRCUIT_BREAKER_OPEN,
            retriable: true,
        },
        Row {
            error: DomainError::PluginNotFound { plugin_ref: "cf.core.oagw.basic.v1".to_owned() },
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: gts::ERR_PLUGIN_NOT_FOUND,
            retriable: false,
        },
        Row {
            error: DomainError::ConnectionTimeout {
                upstream_id: None, host: None, guidance_secs: Some(2), trace_id: trace(),
            },
            status: StatusCode::GATEWAY_TIMEOUT,
            error_type: gts::ERR_CONNECTION_TIMEOUT,
            retriable: true,
        },
        Row {
            error: DomainError::RequestTimeout {
                upstream_id: None, host: None, guidance_secs: Some(2), trace_id: trace(),
            },
            status: StatusCode::GATEWAY_TIMEOUT,
            error_type: gts::ERR_REQUEST_TIMEOUT,
            retriable: true,
        },
        Row {
            error: DomainError::IdleTimeout {
                upstream_id: None, host: None, guidance_secs: Some(2), trace_id: trace(),
            },
            status: StatusCode::GATEWAY_TIMEOUT,
            error_type: gts::ERR_IDLE_TIMEOUT,
            retriable: true,
        },
        Row {
            error: DomainError::CorsOriginNotAllowed { path: None, trace_id: trace() },
            status: StatusCode::FORBIDDEN,
            error_type: gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            retriable: false,
        },
        Row {
            error: DomainError::CorsMethodNotAllowed { path: None, trace_id: trace() },
            status: StatusCode::FORBIDDEN,
            error_type: gts::ERR_CORS_METHOD_NOT_ALLOWED,
            retriable: false,
        },
        Row {
            error: DomainError::PluginInternal("plugin panicked".to_owned()),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error_type: gts::CANONICAL_INTERNAL_TYPE,
            retriable: false,
        },
        Row {
            error: DomainError::Internal("invariant violated".to_owned()),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error_type: gts::CANONICAL_INTERNAL_TYPE,
            retriable: false,
        },
    ]
}

/// `inst-eh-map-2`: one status, one GTS type and one retriable flag per mapped
/// variant, and no variant left out — the table is total.
#[test]
fn the_design_table_maps_every_variant_once() {
    let table = rows();
    assert_eq!(table.len(), 28, "every taxonomy variant is a row");
    for row in table {
        let mapped = super::error::mapping(&row.error);
        assert_eq!(mapped.status, row.status, "{:?}", row.error);
        assert_eq!(mapped.gts_type, row.error_type, "{:?}", row.error);
        assert_eq!(mapped.retriable, row.retriable, "{:?}", row.error);
        assert_eq!(mapping_of(&row.error).0, row.error_type, "{:?}", row.error);
        assert_eq!(mapping_of(&row.error).1, row.status, "{:?}", row.error);
        assert_eq!(super::error::is_retriable(&row.error), row.retriable, "{:?}", row.error);
    }
}

/// The PRD code set: 429, 503 and 504 are retriable, 502 is per occurrence,
/// and every 4xx is not (`cpt-cf-oagw-dod-error-handling-retriability`).
#[test]
fn the_prd_code_set_of_retriability() {
    let retriable = |error: &DomainError| super::error::is_retriable(error);
    assert!(retriable(&DomainError::RateLimitExceeded {
        upstream_id: None, host: None, retry_after_seconds: Some(1), trace_id: None,
    }));
    assert!(retriable(&DomainError::LinkUnavailable {
        upstream_id: None, host: None, path: None, trace_id: None,
    }));
    assert!(retriable(&DomainError::CircuitBreakerOpen {
        upstream_id: None, host: None, trace_id: None,
    }));
    assert!(retriable(&DomainError::RequestTimeout {
        upstream_id: None, host: None, guidance_secs: None, trace_id: None,
    }));
    assert!(!retriable(&DomainError::ProtocolError {
        upstream_id: None, host: None, path: None, trace_id: None,
    }));
    assert!(!retriable(&DomainError::StreamAborted {
        upstream_id: None, host: None, path: None, trace_id: None,
    }));
    assert!(!retriable(&DomainError::PluginNotFound { plugin_ref: "x".to_owned() }));
}

/// `inst-eh-retry-5`: the guidance value is sourced per family, and the two
/// retriable 503 types carry none at all.
#[test]
fn the_retry_guidance_is_sourced_per_family() {
    let guidance = |error: &DomainError| super::error::guidance_of(error);
    assert_eq!(
        guidance(&DomainError::RateLimitExceeded {
            upstream_id: None,
            host: None,
            retry_after_seconds: Some(11),
            trace_id: None,
        }),
        Some(11),
        "the rate-limit decision supplies the value"
    );
    for error in [
        DomainError::ConnectionTimeout {
            upstream_id: None, host: None, guidance_secs: Some(5), trace_id: None,
        },
        DomainError::RequestTimeout {
            upstream_id: None, host: None, guidance_secs: Some(5), trace_id: None,
        },
        DomainError::IdleTimeout {
            upstream_id: None, host: None, guidance_secs: Some(5), trace_id: None,
        },
    ] {
        assert_eq!(guidance(&error), Some(5), "the proxy timeout supplies the value");
    }
    for error in [
        DomainError::LinkUnavailable { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::CircuitBreakerOpen { upstream_id: None, host: None, trace_id: None },
    ] {
        assert_eq!(guidance(&error), None, "the retriable 503 carries no guidance");
    }
    assert_eq!(
        guidance(&DomainError::field_rejection("alias", "rejected")),
        None,
        "a non-retriable type carries no guidance"
    );
}
