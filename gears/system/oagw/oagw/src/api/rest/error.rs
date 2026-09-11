//! RFC 9457 Problem Details rendering (`cpt-cf-oagw-principle-rfc9457`) with
//! the `X-OAGW-Error-Source` attribution header of ADR-0007.

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use serde_json::{Map, Value};

use crate::domain::error::{ErrorSource, OagwError};
use crate::util::ERROR_SOURCE_HEADER;

pub const PROBLEM_JSON: &str = "application/problem+json";

/// Serialize a gateway error into its Problem Details document.
///
/// Extension members are emitted as siblings of the standard fields, as RFC
/// 9457 §3.2 prescribes.
#[must_use]
pub fn problem_body(err: &OagwError, instance: Option<&str>) -> Value {
    let mut body = Map::new();
    body.insert("type".to_owned(), Value::String(err.kind.gts_type().to_owned()));
    body.insert("title".to_owned(), Value::String(err.kind.title().to_owned()));
    body.insert("status".to_owned(), Value::from(err.status()));
    body.insert("detail".to_owned(), Value::String(err.detail.clone()));
    if let Some(instance) = instance {
        body.insert("instance".to_owned(), Value::String(instance.to_owned()));
    }
    if let Some(seconds) = err.retry_after_seconds {
        body.insert("retry_after_seconds".to_owned(), Value::from(seconds));
    }
    for (key, value) in &err.extensions {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

/// Full HTTP response for a gateway-originated error.
#[must_use]
pub fn problem_response(err: &OagwError, instance: Option<&str>) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let payload = problem_body(err, instance);
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());

    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, PROBLEM_JSON)
        .header(ERROR_SOURCE_HEADER, ErrorSource::Gateway.as_str())
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

    if let Some(seconds) = err.retry_after_seconds
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    for (name, value) in &err.extra_headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        problem_response(&self, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::ErrorKind;

    #[test]
    fn the_envelope_carries_the_five_standard_members() {
        let err = OagwError::new(ErrorKind::RouteNotFound, "no match");
        let body = problem_body(&err, Some("/oagw/v1/proxy/x/y"));
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(body["title"], "Route Not Found");
        assert_eq!(body["status"], 404);
        assert_eq!(body["detail"], "no match");
        assert_eq!(body["instance"], "/oagw/v1/proxy/x/y");
    }

    #[test]
    fn extensions_are_siblings_of_the_standard_members() {
        let err = OagwError::new(ErrorKind::MissingTargetHost, "need a host")
            .with_ext("alias", "vendor.com")
            .with_ext(
                "valid_hosts",
                serde_json::json!(["us.vendor.com", "eu.vendor.com"]),
            );
        let body = problem_body(&err, None);
        assert_eq!(body["alias"], "vendor.com");
        assert_eq!(body["valid_hosts"][0], "us.vendor.com");
        assert!(body.get("instance").is_none());
    }

    #[test]
    fn retry_after_lands_in_the_header_and_the_body() {
        let err = OagwError::new(ErrorKind::RateLimitExceeded, "slow down").with_retry_after(15);
        let body = problem_body(&err, None);
        assert_eq!(body["retry_after_seconds"], 15);

        let response = problem_response(&err, None);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("15")
        );
    }

    #[test]
    fn responses_are_problem_json_attributed_to_the_gateway() {
        let response = problem_response(&OagwError::validation("bad"), None);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }

    #[test]
    fn the_body_omits_context_so_the_canonical_layer_leaves_it_alone() {
        // `toolkit_canonical_errors::Problem` requires a `context` member;
        // omitting it makes the shared middleware pass the document through
        // instead of re-serializing it and dropping the extensions above.
        let body = problem_body(&OagwError::validation("bad"), None);
        assert!(body.get("context").is_none());
        assert!(serde_json::from_value::<toolkit_canonical_errors::Problem>(body).is_err());
    }
}
