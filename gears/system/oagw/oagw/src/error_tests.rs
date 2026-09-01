//! Tests for [`crate::domain::error`].

use crate::domain::error::{
    APPLICATION_PROBLEM_JSON, ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM,
    OagwError, ReferencedBy, problem_context,
};
use crate::domain::model::format_upstream_id;
use axum::Router;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower::ServiceExt;
use uuid::Uuid;

fn into_owned(response: axum::http::Response<Body>) -> Response {
    response.into_response()
}

/// Renders `error` exactly as the wire sees it: the problem body the response
/// carries, with the `context` extension fields mirrored to the top level.
///
/// The wire path itself (`into_response` plus the error layer) is asserted in
/// [`crate::api::rest::error_layer`] and the integration tests; this helper
/// keeps the JSON assertions synchronous.
fn rendered(error: &OagwError) -> serde_json::Value {
    let mut body = error.problem_body().clone();
    body.apply_context_extensions();
    serde_json::to_value(&body).expect("serialise")
}

#[test]
fn every_variant_maps_to_its_design_status() {
    let upstream_id = Uuid::from_u128(0xA1);
    let cases: Vec<(OagwError, StatusCode, &str)> = vec![
        (
            OagwError::validation("alias is required"),
            StatusCode::BAD_REQUEST,
            "validation.error",
        ),
        (
            OagwError::missing_target_host("no target host on the request"),
            StatusCode::BAD_REQUEST,
            "routing.missing_target_host",
        ),
        (
            OagwError::invalid_target_host("target host is not an IP or hostname"),
            StatusCode::BAD_REQUEST,
            "routing.invalid_target_host",
        ),
        (
            OagwError::unknown_target_host("host is not part of the upstream pool"),
            StatusCode::BAD_REQUEST,
            "routing.unknown_target_host",
        ),
        (
            OagwError::authentication_failed("api key rejected"),
            StatusCode::UNAUTHORIZED,
            "auth.failed",
        ),
        (
            OagwError::route_not_found("no route matched POST /v1/x"),
            StatusCode::NOT_FOUND,
            "route.not_found",
        ),
        (
            OagwError::plugin_in_use("plugin is still referenced"),
            StatusCode::CONFLICT,
            "plugin.in_use",
        ),
        (
            OagwError::payload_too_large("body exceeds 100 MB"),
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload.too_large",
        ),
        (
            OagwError::rate_limit_exceeded("tenant budget exhausted"),
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit.exceeded",
        ),
        (
            OagwError::secret_not_found("secret payments-key is missing"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "secret.not_found",
        ),
        (
            OagwError::protocol_error("upstream sent a malformed header block"),
            StatusCode::BAD_GATEWAY,
            "protocol.error",
        ),
        (
            OagwError::downstream_error("upstream returned 500"),
            StatusCode::BAD_GATEWAY,
            "downstream.error",
        ),
        (
            OagwError::stream_aborted("SSE stream reset by peer"),
            StatusCode::BAD_GATEWAY,
            "stream.aborted",
        ),
        (
            OagwError::link_unavailable("no healthy endpoint"),
            StatusCode::SERVICE_UNAVAILABLE,
            "link.unavailable",
        ),
        (
            OagwError::circuit_breaker_open("upstream breaker is open"),
            StatusCode::SERVICE_UNAVAILABLE,
            "circuit_breaker.open",
        ),
        (
            OagwError::plugin_not_found("plugin 42 is not registered"),
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin.not_found",
        ),
        (
            OagwError::connection_timeout("connect phase exceeded 2s"),
            StatusCode::GATEWAY_TIMEOUT,
            "timeout.connection",
        ),
        (
            OagwError::request_timeout("exchange exceeded 2s"),
            StatusCode::GATEWAY_TIMEOUT,
            "timeout.request",
        ),
        (
            OagwError::idle_timeout("no bytes for 2s"),
            StatusCode::GATEWAY_TIMEOUT,
            "timeout.idle",
        ),
        (
            OagwError::conflict("duplicate alias"),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            OagwError::not_found("no such route"),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            OagwError::forbidden("origin not allowed"),
            StatusCode::FORBIDDEN,
            "cors.forbidden",
        ),
        (
            OagwError::bind_forbidden("ancestor enforces the alias"),
            StatusCode::FORBIDDEN,
            "tenancy.bind_forbidden",
        ),
    ];

    for (error, expected_status, expected_fragment) in cases {
        assert_eq!(error.status(), expected_status, "{error}");
        let gts_type = error.gts_type();
        assert_eq!(
            gts_type,
            format!("gts.cf.core.errors.err.v1~cf.oagw.{expected_fragment}.v1"),
            "{error}"
        );
        let body = error.problem_body();
        assert_eq!(body.status, expected_status.as_u16(), "{error}");
        assert!(!body.title.is_empty(), "{error}");
        assert_eq!(body.r#type, gts_type);
        assert_eq!(body.detail, error.detail(), "{error}");
        assert_eq!(error.context(), &body.context, "{error}");
    }

    // `with_upstream_id` carries the GTS instance id (a UUID string).
    let with_id = OagwError::not_found("x").with_upstream_id(upstream_id);
    let expected = upstream_id.to_string();
    assert_eq!(
        with_id.context().upstream_id.as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn problem_body_carries_the_context_extension_object() {
    let error = OagwError::unknown_target_host("host does not belong to the upstream pool")
        .with_alias("payments")
        .with_host("evil.example.com")
        .with_path("/oagw/v1/proxy/payments/v1/pay")
        .with_valid_hosts(vec!["api.example.com".to_owned()])
        .with_invalid_value("evil.example.com");
    let body = rendered(&error);
    assert_eq!(body["title"], "Unknown Target Host");
    assert_eq!(body["status"], serde_json::json!(400));
    assert_eq!(body["detail"], "host does not belong to the upstream pool");
    assert_eq!(body["instance"], "/oagw/v1/proxy/payments/v1/pay");
    let context = &body["context"];
    assert!(context.is_object());
    assert_eq!(context["alias"], "payments");
    assert_eq!(context["host"], "evil.example.com");
    assert_eq!(
        context["valid_hosts"],
        serde_json::json!(["api.example.com"])
    );
    assert_eq!(context["invalid_value"], "evil.example.com");
    assert!(
        context.get("plugin_id").is_none(),
        "unset fields are omitted"
    );
    assert!(context.get("referenced_by").is_none());
}

#[test]
fn extension_fields_are_emitted_at_the_top_level_and_in_context() {
    let error = OagwError::unknown_target_host("host does not belong to the upstream pool")
        .with_alias("payments")
        .with_host("evil.example.com")
        .with_path("/oagw/v1/proxy/payments/v1/pay")
        .with_upstream_id(Uuid::from_u128(0xA1))
        .with_valid_hosts(vec!["api.example.com".to_owned()])
        .with_invalid_value("evil.example.com");
    let body = rendered(&error);

    // ADR-0007 wire examples: the extension fields sit next to the RFC fields.
    assert_eq!(body["alias"], "payments", "{body}");
    assert_eq!(body["host"], "evil.example.com", "{body}");
    assert_eq!(body["path"], "/oagw/v1/proxy/payments/v1/pay", "{body}");
    assert_eq!(
        body["upstream_id"],
        Uuid::from_u128(0xA1).to_string(),
        "{body}"
    );
    assert_eq!(body["valid_hosts"], serde_json::json!(["api.example.com"]));
    assert_eq!(body["invalid_value"], "evil.example.com");

    // ... and the nested `context` object carries the same values.
    assert_eq!(body["context"]["alias"], "payments", "{body}");
    assert_eq!(body["context"]["host"], "evil.example.com", "{body}");
    assert_eq!(body["context"]["path"], "/oagw/v1/proxy/payments/v1/pay");
    assert_eq!(
        body["context"]["upstream_id"],
        Uuid::from_u128(0xA1).to_string()
    );
    assert_eq!(body["context"]["valid_hosts"], body["valid_hosts"]);
    assert_eq!(body["context"]["invalid_value"], "evil.example.com");

    // A body without extensions renders neither view.
    let plain = rendered(&OagwError::validation("nope"));
    assert!(plain.get("alias").is_none(), "{plain}");
    assert!(plain.get("upstream_id").is_none(), "{plain}");
    assert!(plain.get("valid_hosts").is_none(), "{plain}");
    assert!(plain["context"].is_object(), "context stays on the wire");
    assert!(plain["context"].as_object().expect("object").is_empty());
}

#[test]
fn a_problem_body_round_trips_through_its_wire_shape() {
    let error = OagwError::unknown_target_host("host is unknown")
        .with_alias("payments")
        .with_valid_hosts(vec!["api.example.com".to_owned()]);
    let body = rendered(&error);
    let reparsed: crate::domain::error::ProblemBody = serde_json::from_value(body).expect("parses");
    assert_eq!(reparsed.alias.as_deref(), Some("payments"));
    assert_eq!(reparsed.context.alias.as_deref(), Some("payments"));
    assert_eq!(reparsed.valid_hosts, vec!["api.example.com".to_owned()]);
    assert_eq!(reparsed.status, 400);
}

#[test]
fn plugin_in_use_reports_its_referencers() {
    let error = OagwError::plugin_in_use("plugin is still referenced")
        .with_plugin_id("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1")
        .with_referenced_by(ReferencedBy {
            upstreams: vec![format_upstream_id(Uuid::from_u128(0xA1))],
            routes: Vec::new(),
        });
    let body = rendered(&error);
    assert_eq!(body["status"], serde_json::json!(409));
    assert_eq!(
        body["context"]["plugin_id"],
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(
        body["context"]["referenced_by"]["upstreams"],
        serde_json::json!([format_upstream_id(Uuid::from_u128(0xA1))])
    );
    assert!(body["context"]["referenced_by"].get("routes").is_none());
    assert_eq!(
        body["plugin_id"], "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1",
        "the extension field is top level too"
    );
    assert_eq!(
        body["referenced_by"]["upstreams"],
        serde_json::json!([format_upstream_id(Uuid::from_u128(0xA1))])
    );
}

#[test]
fn retry_after_is_a_top_level_extension_field_and_a_header() {
    let error =
        OagwError::rate_limit_exceeded("tenant budget exhausted").with_retry_after_seconds(15);
    let body = rendered(&error);
    assert_eq!(body["retry_after_seconds"], serde_json::json!(15), "{body}");
    assert_eq!(
        body["context"]["retry_after_seconds"],
        serde_json::json!(15)
    );
}

#[test]
fn response_sets_content_type_error_source_and_retry_after() {
    let error =
        OagwError::rate_limit_exceeded("tenant budget exhausted").with_retry_after_seconds(30);
    let response = into_owned(error.clone().into_response());
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(APPLICATION_PROBLEM_JSON)
    );
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("30")
    );

    let upstream_failure = OagwError::downstream_error("upstream returned 502");
    let mapped = upstream_failure.into_response_with_source(ERROR_SOURCE_UPSTREAM);
    assert_eq!(
        mapped
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_UPSTREAM)
    );
    assert!(
        !mapped
            .headers()
            .contains_key(axum::http::header::RETRY_AFTER)
    );
}

#[test]
fn display_and_error_impl_expose_type_and_detail() {
    let error = OagwError::route_not_found("no route matched");
    let rendered = error.to_string();
    assert!(rendered.contains("no route matched"), "{rendered}");
    assert!(rendered.contains("route.not_found"), "{rendered}");

    fn takes_std_error(error: impl std::error::Error) -> usize {
        error.to_string().len()
    }
    assert!(takes_std_error(error) > 0);
}

#[test]
fn problem_context_builder_builds_the_same_shape_as_the_fluent_setters() {
    let built = problem_context()
        .alias("payments")
        .host("api.example.com")
        .retry_after_seconds(5)
        .build();
    let fluent = OagwError::rate_limit_exceeded("x")
        .with_alias("payments")
        .with_host("api.example.com")
        .with_retry_after_seconds(5)
        .context()
        .clone();
    assert_eq!(built, fluent);
    assert_eq!(built.alias.as_deref(), Some("payments"));
    assert_eq!(built.retry_after_seconds, Some(5));
}

#[tokio::test]
async fn handlers_return_problem_json_through_the_axum_surface() {
    async fn failing() -> OagwError {
        std::future::ready(OagwError::validation("alias is required")).await
    }

    let app = Router::new().route("/boom", get(failing));
    let request = HttpRequest::get("/boom")
        .body(Body::empty())
        .expect("request");
    let response = into_owned(app.oneshot(request).await.expect("response"));
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("problem json");
    assert_eq!(body["title"], "Validation Error");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(body["context"].is_object());
}

#[test]
fn the_bind_denial_is_not_the_cors_denial() {
    let bind = OagwError::bind_forbidden("an ancestor enforces the alias")
        .with_alias("api.openai.com")
        .with_upstream_id(Uuid::from_u128(0xA1));
    assert_eq!(
        bind.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.tenancy.bind_forbidden.v1"
    );
    assert_eq!(bind.status(), StatusCode::FORBIDDEN);

    let cors = OagwError::forbidden("origin not allowed");
    assert_eq!(
        cors.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.cors.forbidden.v1"
    );

    let body = rendered(&bind);
    assert_eq!(body["type"], bind.gts_type(), "{body}");
    assert_eq!(body["alias"], "api.openai.com", "{body}");
    assert_eq!(body["context"]["alias"], "api.openai.com", "{body}");
}
