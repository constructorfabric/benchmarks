//! The problem+json serializer and the error-contract layer of FEATURE entry
//! 2.5 (`cpt-cf-oagw-algo-error-handling-serialization`,
//! `cpt-cf-oagw-algo-error-handling-extension-fields`).
//!
//! The wire shape is asserted here: the five RFC 9457 members, the closed
//! extension vocabulary at the top level, the headers that ride with a body,
//! and the request-scoped members only the router can fill.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use toolkit_gts::gts_uri;

use super::error::{ApiError, ErrorOccurrence, ProblemDetails, TRACE_ID_HEADER};
use crate::domain::error::{DomainError, ReferencedBy};
use crate::domain::gts_helpers as gts;

/// The body of `error` as a JSON object.
async fn body_of(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    serde_json::from_slice(&bytes).expect("the problem body is JSON")
}

/// The member names of a serialized body.
fn members(document: &serde_json::Value) -> Vec<String> {
    document.as_object().expect("an object").keys().cloned().collect()
}

#[tokio::test]
async fn the_body_carries_exactly_the_five_members_and_the_closed_vocabulary() {
    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::AuthenticationFailed {
                upstream_id: Some("up-1".to_owned()),
                host: Some("api.vendor.com".to_owned()),
                path: Some("/v1/orders".to_owned()),
                trace_id: Some("trace-1".to_owned()),
            },
            &ErrorOccurrence {
                instance: Some("/oagw/v1/proxy/api.vendor.com/v1/orders".to_owned()),
                ..ErrorOccurrence::default()
            },
        )
        .into_response(),
    )
    .await;
    let carried = members(&document);
    let closed = [
        "type",
        "title",
        "status",
        "detail",
        "instance",
        "upstream_id",
        "host",
        "path",
        "retry_after_seconds",
        "trace_id",
        "referenced_by",
        "alias",
        "valid_hosts",
        "invalid_value",
    ];
    for member in &carried {
        assert!(
            closed.contains(&member.as_str()),
            "`{member}` is outside the closed vocabulary: {document}"
        );
    }
    for member in ["type", "title", "status", "detail", "instance", "upstream_id", "host", "path", "trace_id"] {
        assert!(carried.contains(&member.to_owned()), "`{member}` is carried when known: {document}");
    }
    assert!(
        document.get("context").is_none(),
        "extension members are at the top level, never under `context`: {document}"
    );
    assert_eq!(document["type"], gts_uri!(gts::ERR_AUTH_FAILED));
    assert_eq!(document["status"], 401);
    assert_eq!(document["title"], "Authentication failed");
}

#[tokio::test]
async fn an_unknown_member_is_omitted_and_never_null() {
    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::RouteNotFound { path: None, trace_id: None },
            &ErrorOccurrence::default(),
        )
        .into_response(),
    )
    .await;
    for member in
        ["upstream_id", "host", "retry_after_seconds", "trace_id", "referenced_by", "alias", "valid_hosts", "invalid_value"]
    {
        assert!(
            document.get(member).is_none(),
            "`{member}` is omitted, never `null`: {document}"
        );
    }
}

#[tokio::test]
async fn the_content_type_and_the_status_are_the_mapped_ones() {
    let response = ProblemDetails::from_error(
        &DomainError::PayloadTooLarge { path: None, trace_id: None, upstream_id: None, limit_bytes: None },
        &ErrorOccurrence::default(),
    )
    .into_response();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get(axum::http::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        Some("application/problem+json"),
    );
}

#[tokio::test]
async fn every_rendered_body_is_gateway_sourced() {
    for error in [
        DomainError::field_rejection("alias", "rejected"),
        DomainError::LinkUnavailable { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::PluginInUse { referenced_by: ReferencedBy::default() },
    ] {
        let response = ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response();
        assert_eq!(
            response
                .headers()
                .get(crate::domain::headers::ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("gateway"),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn retry_after_and_the_member_carry_the_same_value() {
    let error = DomainError::RequestTimeout {
        upstream_id: Some("up-1".to_owned()),
        host: Some("api.vendor.com".to_owned()),
        guidance_secs: Some(5),
        trace_id: None,
    };
    let response = ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        response.headers().get(axum::http::header::RETRY_AFTER).and_then(|value| value.to_str().ok()),
        Some("5"),
    );
    let document = body_of(response).await;
    assert_eq!(document["retry_after_seconds"], 5);
}

#[tokio::test]
async fn a_retriable_type_without_guidance_emits_neither_member_nor_header() {
    for error in [
        DomainError::LinkUnavailable { upstream_id: None, host: None, path: None, trace_id: None },
        DomainError::CircuitBreakerOpen { upstream_id: None, host: None, trace_id: None },
    ] {
        let response = ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response.headers().get(axum::http::header::RETRY_AFTER).is_none(),
            "{error:?} carries no `Retry-After`"
        );
        let document = body_of(response).await;
        assert!(document.get("retry_after_seconds").is_none(), "{document}");
    }
}

#[tokio::test]
async fn the_cors_rejections_carry_vary_origin() {
    for error in [
        DomainError::CorsOriginNotAllowed { path: None, trace_id: None },
        DomainError::CorsMethodNotAllowed { path: None, trace_id: None },
    ] {
        let response = ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get(axum::http::header::VARY).and_then(|value| value.to_str().ok()),
            Some("Origin"),
        );
    }
    let response = ProblemDetails::from_error(
        &DomainError::field_rejection("alias", "rejected"),
        &ErrorOccurrence::default(),
    )
    .into_response();
    assert!(response.headers().get(axum::http::header::VARY).is_none());
}

#[tokio::test]
async fn the_routing_family_is_carried_by_the_target_host_errors() {
    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::UnknownTargetHost {
                upstream_id: Some("up-1".to_owned()),
                invalid_value: "other.host".to_owned(),
                valid_hosts: vec!["a.vendor.com".to_owned(), "b.vendor.com".to_owned()],
                trace_id: None,
            },
            &ErrorOccurrence::default(),
        )
        .into_response(),
    )
    .await;
    assert_eq!(document["type"], gts_uri!(gts::ERR_UNKNOWN_TARGET_HOST));
    assert_eq!(document["invalid_value"], "other.host");
    assert_eq!(document["valid_hosts"], serde_json::json!(["a.vendor.com", "b.vendor.com"]));
    assert_eq!(document["upstream_id"], "up-1");

    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::MissingTargetHost {
                upstream_id: Some("up-1".to_owned()),
                alias: Some("api.vendor.com".to_owned()),
                valid_hosts: vec!["a.vendor.com".to_owned()],
                trace_id: None,
            },
            &ErrorOccurrence::default(),
        )
        .into_response(),
    )
    .await;
    assert_eq!(document["alias"], "api.vendor.com");
    assert_eq!(document["valid_hosts"], serde_json::json!(["a.vendor.com"]));
    assert!(document.get("invalid_value").is_none());
}

#[tokio::test]
async fn the_invalid_value_echo_is_bounded_and_control_free() {
    let hostile = format!("a{}\u{7}b{}", "x".repeat(200), "\u{1b}");
    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::InvalidTargetHost {
                upstream_id: None,
                invalid_value: hostile,
                trace_id: None,
            },
            &ErrorOccurrence::default(),
        )
        .into_response(),
    )
    .await;
    let echoed = document["invalid_value"].as_str().expect("the echo");
    assert!(echoed.len() <= 128, "the echo is bounded: {}", echoed.len());
    assert!(!echoed.chars().any(char::is_control), "no control character survives: {echoed:?}");
    assert!(
        !document["detail"].as_str().expect("detail").contains("xxxxx"),
        "the echo never reaches `detail`"
    );
}

#[tokio::test]
async fn referenced_by_is_carried_by_the_409_only() {
    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::PluginInUse {
                referenced_by: ReferencedBy {
                    upstreams: vec!["gts.cf.core.oagw.upstream.v1~abc".to_owned()],
                    routes: Vec::new(),
                },
            },
            &ErrorOccurrence::default(),
        )
        .into_response(),
    )
    .await;
    assert_eq!(document["status"], 409);
    assert_eq!(
        document["referenced_by"]["upstreams"],
        serde_json::json!(["gts.cf.core.oagw.upstream.v1~abc"])
    );

    for error in [
        DomainError::field_rejection("alias", "rejected"),
        DomainError::RouteNotFound { path: None, trace_id: None },
        DomainError::Conflict { detail: "alias taken".to_owned(), referenced_by: None },
    ] {
        let document = body_of(ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response()).await;
        assert!(document.get("referenced_by").is_none(), "{document}");
    }
}

#[tokio::test]
async fn the_internal_variants_render_the_platform_internal_category() {
    for error in [
        DomainError::Internal("invariant violated".to_owned()),
        DomainError::PluginInternal("plugin panicked".to_owned()),
    ] {
        let response = ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let document = body_of(response).await;
        assert_eq!(
            document["type"],
            gts_uri!(gts::CANONICAL_INTERNAL_TYPE),
            "an invariant violation is not attributable to the caller"
        );
    }
    // The constant cannot drift from the platform's own spelling.
    assert_eq!(
        toolkit_canonical_errors::CanonicalError::internal("x").create().gts_type(),
        gts::CANONICAL_INTERNAL_TYPE,
    );
}

#[tokio::test]
async fn the_occurrence_overlay_prefers_what_the_error_carries() {
    let document = body_of(
        ProblemDetails::from_error(
            &DomainError::ProtocolError {
                upstream_id: Some("from-the-error".to_owned()),
                host: Some("from-the-error".to_owned()),
                path: None,
                trace_id: None,
            },
            &ErrorOccurrence {
                instance: Some("/oagw/v1/proxy/a/v1".to_owned()),
                upstream_id: Some("from-the-site".to_owned()),
                host: Some("from-the-site".to_owned()),
                path: Some("/v1".to_owned()),
                trace_id: Some("trace-1".to_owned()),
            },
        )
        .into_response(),
    )
    .await;
    assert_eq!(document["upstream_id"], "from-the-error");
    assert_eq!(document["host"], "from-the-error");
    assert_eq!(document["path"], "/v1", "a member the variant does not carry is filled");
    assert_eq!(document["instance"], "/oagw/v1/proxy/a/v1");
    assert_eq!(document["trace_id"], "trace-1");
}

/// The error-contract layer fills the two request-scoped members the handler
/// cannot know, and leaves a body that already carries them alone.
#[tokio::test]
async fn the_layer_fills_instance_and_trace_id_on_the_way_out() {
    use tower::ServiceExt;

    async fn handler() -> impl IntoResponse {
        ApiError::Domain(DomainError::RouteNotFound { path: None, trace_id: None })
    }
    let router = axum::Router::new()
        .route("/oagw/v1/proxy/{alias}/{*path_suffix}", axum::routing::get(handler))
        .layer(axum::middleware::from_fn(super::error::complete_problem_context));

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.vendor.com/v9")
        .header(TRACE_ID_HEADER, "trace-9")
        .body(axum::body::Body::empty())
        .expect("the request is well formed");
    let response = router.oneshot(request).await.expect("the router answers");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let document = body_of(response).await;
    assert_eq!(
        document["instance"], "/oagw/v1/proxy/api.vendor.com/v9",
        "the gear-relative path with no `/api` segment"
    );
    assert_eq!(document["trace_id"], "trace-9");
}

/// A body that already carries both members is not rewritten, and a success
/// response is not touched at all.
#[tokio::test]
async fn the_layer_leaves_a_complete_body_and_a_success_response_alone() {
    use tower::ServiceExt;

    let router = axum::Router::new()
        .route("/oagw/v1/upstreams", axum::routing::get(|| async {
            axum::Json(serde_json::json!({ "items": [] }))
        }))
        .route("/oagw/v1/routes/{id}", axum::routing::put(|| async {
            ApiError::Domain(DomainError::ValidationError {
                detail: "field `alias` rejected: taken".to_owned(),
                path: Some("/oagw/v1/routes/1".to_owned()),
                trace_id: Some("carried".to_owned()),
            })
        }))
        .layer(axum::middleware::from_fn(super::error::complete_problem_context));

    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/oagw/v1/upstreams")
                .header(TRACE_ID_HEADER, "ignored")
                .body(axum::body::Body::empty())
                .expect("the request is well formed"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let document = body_of(response).await;
    assert!(document.get("instance").is_none(), "a success body is untouched: {document}");

    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri("/oagw/v1/routes/1")
                .header(TRACE_ID_HEADER, "inbound")
                .body(axum::body::Body::empty())
                .expect("the request is well formed"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let document = body_of(response).await;
    assert_eq!(document["trace_id"], "carried", "the value the error carries wins");
    assert_eq!(document["path"], "/oagw/v1/routes/1");
}

/// The echo of a trace identifier is bounded, like every request-derived value.
#[tokio::test]
async fn the_trace_identifier_echo_is_bounded() {
    let long = format!("{}{}", "t".repeat(400), "\u{1}");
    let error = DomainError::RouteNotFound { path: None, trace_id: Some(long) };
    let document = body_of(ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response()).await;
    let echoed = document["trace_id"].as_str().expect("the trace identifier");
    assert!(echoed.len() <= 128, "the echo is bounded: {}", echoed.len());
    assert!(!echoed.chars().any(char::is_control), "no control character survives");
}
