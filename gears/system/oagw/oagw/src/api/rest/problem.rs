//! RFC 9457 response shaping.
//!
//! `instance` names the request URI path that produced the problem document.
//! It is the transport boundary's knowledge, not the handler's, so it is
//! stamped here once for every error the gear renders instead of threading the
//! path through each handler's `map_err`.

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;

/// Upper bound on a problem document; the documents are small and fixed-shape.
const PROBLEM_BODY_LIMIT: usize = 64 * 1024;

/// Records the request path on every problem-details response the gear emits.
pub async fn with_instance(request: Request, next: Next) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;

    let is_problem = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/problem+json"));
    if !is_problem {
        return response;
    }
    if !is_problem_status(response.status()) {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, PROBLEM_BODY_LIMIT).await else {
        return axum::response::Response::from_parts(parts, axum::body::Body::empty());
    };
    let Ok(mut document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    if let Some(object) = document.as_object_mut() {
        object
            .entry("instance")
            .or_insert_with(|| serde_json::Value::String(path));
    }
    axum::response::Response::from_parts(
        parts,
        axum::body::Body::from(serde_json::to_vec(&document).unwrap_or_else(|_| bytes.to_vec())),
    )
}

/// Whether a status is one the gear renders as problem details.
#[must_use]
pub fn is_problem_status(status: StatusCode) -> bool {
    status.is_client_error() || status.is_server_error()
}

/// A `problem+json` content type.
#[must_use]
pub fn problem_content_type() -> HeaderValue {
    HeaderValue::from_static("application/problem+json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn failing() -> Result<axum::response::Response, crate::api::rest::OagwError> {
        Err(crate::api::rest::OagwError::gateway(
            crate::domain::error::DomainError::RouteNotFound,
        ))
    }

    #[tokio::test]
    async fn a_problem_document_names_the_request_path() {
        let app = axum::Router::new()
            .route("/oagw/v1/proxy/alias/v1", get(failing))
            .layer(axum::middleware::from_fn(with_instance));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/oagw/v1/proxy/alias/v1")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), 404);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let document: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            document["instance"], "/oagw/v1/proxy/alias/v1",
            "{document}"
        );
    }

    #[tokio::test]
    async fn a_successful_response_is_left_alone() {
        let app = axum::Router::new()
            .route("/ok", get(|| async { "fine" }))
            .layer(axum::middleware::from_fn(with_instance));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ok")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), 200);
    }

    #[test]
    fn only_error_statuses_are_problem_documents() {
        assert!(is_problem_status(StatusCode::NOT_FOUND));
        assert!(is_problem_status(StatusCode::BAD_GATEWAY));
        assert!(!is_problem_status(StatusCode::OK));
        assert!(!is_problem_status(StatusCode::NO_CONTENT));
    }
}
