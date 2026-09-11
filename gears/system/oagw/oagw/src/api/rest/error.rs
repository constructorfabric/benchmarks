//! Wire projection of [`OagwError`].
//!
//! Every OAGW error answers with `application/problem+json` (RFC 9457) and
//! `X-OAGW-Error-Source: gateway`. The `instance` member is filled by
//! [`fill_problem_instance`], a thin layer over the gear's own routes, so
//! handlers never have to thread the request path through their signatures.

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};

use crate::domain::error::OagwError;
use crate::infra::proxy::problem_response;

/// Media type of every gateway error body.
pub const PROBLEM_JSON: &str = "application/problem+json";

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        problem_response(&self, None)
    }
}

/// Fill the RFC 9457 `instance` member with the request path when a handler
/// did not set one.
///
/// Only `application/problem+json` bodies are touched, so streamed proxy
/// responses (SSE, chunked, upgraded connections) pass through untouched.
pub async fn fill_problem_instance(request: Request, next: Next) -> Response {
    let path = request
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or_else(
            || request.uri().path().to_owned(),
            |original| original.0.path().to_owned(),
        );
    let response = next.run(request).await;

    let is_problem = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(PROBLEM_JSON));
    if !is_problem {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        // A problem body larger than 64 KiB is not one we produced; leave the
        // response alone rather than truncating it.
        return Response::from_parts(parts, Body::empty());
    };
    let Ok(mut problem) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let Some(object) = problem.as_object_mut() else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if object.contains_key("instance") {
        return Response::from_parts(parts, Body::from(bytes));
    }
    object.insert("instance".to_owned(), serde_json::Value::from(path));
    let Ok(rendered) = serde_json::to_vec(&problem) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Ok(length) = HeaderValue::from_str(&rendered.len().to_string()) {
        parts.headers.insert(CONTENT_LENGTH, length);
    }
    Response::from_parts(parts, Body::from(rendered))
}

#[cfg(test)]
mod tests {
    use super::{PROBLEM_JSON, fill_problem_instance};
    use crate::domain::error::{ERROR_SOURCE_HEADER, OagwError};
    use axum::Router;
    use axum::body::Body;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use http::Request;
    use tower::ServiceExt;

    async fn read_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn errors_render_as_problem_json_tagged_as_gateway() {
        let response = OagwError::not_found("upstream 'x' not found").into_response();
        assert_eq!(response.status(), 404);
        assert_eq!(response.headers()[http::header::CONTENT_TYPE], PROBLEM_JSON);
        assert_eq!(response.headers()[ERROR_SOURCE_HEADER], "gateway");
        let body = read_json(response).await;
        assert_eq!(body["status"], 404);
        assert_eq!(body["title"], "Not Found");
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.not_found.v1"
        );
    }

    #[tokio::test]
    async fn the_layer_fills_the_instance_member() {
        let app = Router::new()
            .route(
                "/oagw/v1/upstreams/{id}",
                get(|| async { OagwError::not_found("nope").into_response() }),
            )
            .route_layer(axum::middleware::from_fn(fill_problem_instance));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/oagw/v1/upstreams/abc")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = read_json(response).await;
        assert_eq!(body["instance"], "/oagw/v1/upstreams/abc");
    }

    #[tokio::test]
    async fn the_layer_preserves_extension_members() {
        let app = Router::new()
            .route(
                "/oagw/v1/proxy/{alias}",
                get(|| async {
                    OagwError::rate_limit_exceeded("too fast")
                        .with("host", "api.openai.com")
                        .with_retry_after(15)
                        .into_response()
                }),
            )
            .route_layer(axum::middleware::from_fn(fill_problem_instance));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/oagw/v1/proxy/api.openai.com")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.headers()[http::header::RETRY_AFTER], "15");
        let body = read_json(response).await;
        assert_eq!(body["host"], "api.openai.com");
        assert_eq!(body["retry_after_seconds"], 15);
        assert_eq!(body["instance"], "/oagw/v1/proxy/api.openai.com");
    }

    #[tokio::test]
    async fn the_layer_leaves_non_problem_responses_alone() {
        let app = Router::new()
            .route("/stream", get(|| async { "event: ping\n\n" }))
            .route_layer(axum::middleware::from_fn(fill_problem_instance));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/stream")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert_eq!(&bytes[..], b"event: ping\n\n");
    }
}
