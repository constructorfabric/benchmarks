use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware;
use axum::routing::get;
use axum::{Json, Router};
use tower::ServiceExt;

use super::*;
use crate::domain::error::{ErrorExtensions, ErrorKind};

fn router() -> Router {
    Router::new()
        .route(
            "/boom",
            get(|| async {
                let extensions = ErrorExtensions {
                    host: Some("api.openai.com".to_owned()),
                    retry_after_seconds: Some(15),
                    valid_hosts: vec!["us.vendor.com".to_owned()],
                    ..ErrorExtensions::default()
                };
                let error = DomainError::new(ErrorKind::RateLimitExceeded, "too fast")
                    .with_extensions(extensions);
                Result::<(), ProblemResponse>::Err(error.into())
            }),
        )
        .layer(middleware::from_fn(set_problem_instance))
}

#[tokio::test]
async fn problems_render_the_contract_catalog() {
    let response = router()
        .oneshot(
            Request::builder()
                .uri("/boom")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "15");

    let body = serde_json::from_slice::<serde_json::Value>(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("json");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(body["status"], 429);
    assert_eq!(body["detail"], "too fast");
    assert_eq!(body["instance"], "/boom");
    assert_eq!(body["title"], "Rate Limit Exceeded");
    assert_eq!(body["context"]["host"], "api.openai.com");
    assert_eq!(
        body["context"]["valid_hosts"],
        serde_json::json!(["us.vendor.com"])
    );
    assert_eq!(body["context"]["retry_after_seconds"], 15);
}

/// The host's canonical error middleware deserializes every problem body into
/// a `Problem`; a body it cannot parse is logged at `error!` and returned
/// without `trace_id` / `instance` enrichment.
#[tokio::test]
async fn problem_bodies_round_trip_through_the_canonical_middleware() {
    let response = router()
        .oneshot(
            Request::builder()
                .uri("/boom")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    let problem: toolkit_canonical_errors::Problem =
        serde_json::from_slice(&bytes).expect("canonical problem");
    assert_eq!(
        problem.problem_type,
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    // The oagw extension members survive the canonical round trip.
    assert_eq!(problem.context["host"], "api.openai.com");
    assert_eq!(problem.context["valid_hosts"][0], "us.vendor.com");
}

#[tokio::test]
async fn non_problem_bodies_are_left_alone() {
    let router = Router::new()
        .route(
            "/ok",
            get(|| async { Json(serde_json::json!({ "value": 1 })) }),
        )
        .layer(middleware::from_fn(set_problem_instance));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/ok")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    let body = serde_json::from_slice::<serde_json::Value>(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("json");
    assert_eq!(body["value"], 1);
    assert!(body.get("instance").is_none());
}
