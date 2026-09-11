//! How a gateway error reaches the wire.

use super::*;
use crate::domain::error::ErrorKind;
use axum::body::to_bytes;

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn a_problem_response_is_rfc_9457_and_marked_gateway() {
    let error = OagwError::new(ErrorKind::RouteNotFound, "no route here");
    let response = problem_response(&error, Some("/oagw/v1/proxy/x/y"));

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
    assert_eq!(response.headers().get(ERROR_SOURCE).unwrap(), "gateway");

    let json = body_json(response).await;
    assert_eq!(json["status"], 404);
    assert_eq!(json["instance"], "/oagw/v1/proxy/x/y");
}

#[tokio::test]
async fn a_retriable_error_advertises_retry_after_and_the_retriable_marker() {
    let error = OagwError::new(ErrorKind::RateLimitExceeded, "slow down").with_retry_after(30);
    let response = problem_response(&error, None);

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "30");
    assert_eq!(response.headers().get("x-oagw-retriable").unwrap(), "true");
}

#[tokio::test]
async fn a_non_retriable_error_carries_no_retriable_marker() {
    let response = problem_response(&OagwError::validation("bad"), None);
    assert!(response.headers().get("x-oagw-retriable").is_none());
    assert!(response.headers().get(header::RETRY_AFTER).is_none());
}

#[tokio::test]
async fn attached_headers_reach_the_response() {
    let error = OagwError::new(ErrorKind::RateLimitExceeded, "slow down")
        .with_header("X-RateLimit-Limit", "100")
        .with_header("X-RateLimit-Remaining", "0");
    let response = problem_response(&error, None);
    assert_eq!(response.headers().get("x-ratelimit-limit").unwrap(), "100");
    assert_eq!(response.headers().get("x-ratelimit-remaining").unwrap(), "0");
}

#[tokio::test]
async fn an_api_error_renders_through_into_response() {
    let response = ApiError::new(OagwError::conflict("taken"), "/oagw/v1/upstreams")
        .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = body_json(response).await;
    assert_eq!(json["instance"], "/oagw/v1/upstreams");
}

#[test]
fn responses_can_be_marked_as_relayed_from_the_upstream() {
    let mut response = StatusCode::OK.into_response();
    mark_upstream(&mut response);
    assert_eq!(response.headers().get(ERROR_SOURCE).unwrap(), "upstream");
    mark_gateway(&mut response);
    assert_eq!(response.headers().get(ERROR_SOURCE).unwrap(), "gateway");
}
