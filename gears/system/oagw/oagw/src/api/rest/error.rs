//! Rendering of gateway errors onto the wire.

use axum::response::{IntoResponse, Response};
use http::{HeaderName, HeaderValue, StatusCode, header};

use crate::domain::error::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_UPSTREAM, OagwError, PROBLEM_JSON,
};

/// `X-OAGW-Error-Source` as a header name.
pub const ERROR_SOURCE: HeaderName = HeaderName::from_static("x-oagw-error-source");

/// Render `error` as RFC 9457 problem details anchored at `instance`.
#[must_use]
pub fn problem_response(error: &OagwError, instance: Option<&str>) -> Response {
    let status =
        StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let problem = error.to_problem(instance);
    let body = serde_json::to_vec(&problem).unwrap_or_else(|_| {
        // Serializing a problem document cannot realistically fail; if it
        // does, still answer with a valid problem body rather than an empty
        // one.
        br#"{"type":"about:blank","title":"Internal Error","status":500,"detail":"error serialization failed"}"#
            .to_vec()
    });

    let mut response = (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON))],
        body,
    )
        .into_response();

    mark_gateway(&mut response);
    for (name, value) in &error.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.to_ascii_lowercase()),
            HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    if let Some(seconds) = error.retry_after_seconds {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    if error.kind.retriable() {
        response.headers_mut().insert(
            HeaderName::from_static("x-oagw-retriable"),
            HeaderValue::from_static("true"),
        );
    }
    response
}

/// Stamp a response as gateway-originated.
pub fn mark_gateway(response: &mut Response) {
    response
        .headers_mut()
        .insert(ERROR_SOURCE, HeaderValue::from_static(ERROR_SOURCE_GATEWAY));
}

/// Stamp a response as relayed from the upstream.
pub fn mark_upstream(response: &mut Response) {
    response
        .headers_mut()
        .insert(ERROR_SOURCE, HeaderValue::from_static(ERROR_SOURCE_UPSTREAM));
}

/// Wrapper that carries the request URI so `instance` can be filled in.
#[derive(Debug)]
pub struct ApiError {
    pub inner: OagwError,
    pub instance: Option<String>,
}

impl ApiError {
    #[must_use]
    pub fn new(inner: OagwError, instance: impl Into<String>) -> Self {
        Self {
            inner,
            instance: Some(instance.into()),
        }
    }
}

impl From<OagwError> for ApiError {
    fn from(inner: OagwError) -> Self {
        Self {
            inner,
            instance: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        problem_response(&self.inner, self.instance.as_deref())
    }
}

/// Result alias for the management handlers.
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
