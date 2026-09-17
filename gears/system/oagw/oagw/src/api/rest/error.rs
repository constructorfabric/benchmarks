//! Error mapping for the REST layer.
//!
//! Gateway errors are [`OagwError`]s and carry their own `IntoResponse`
//! (RFC 9457 + `X-OAGW-Error-Source`). This module adapts the remaining
//! handler failure modes (extractor rejections, anyhow) onto the same shape.

use axum::response::{IntoResponse, Response};

use crate::domain::error::OagwError;

/// Result alias used by every handler.
pub type ApiResult<T> = Result<T, OagwError>;

/// Maps any error type into a gateway problem document.
#[must_use]
pub fn into_problem(err: OagwError) -> Response {
    err.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn validation_maps_to_400_problem() {
        let response = into_problem(OagwError::Validation("bad".to_owned()));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(content_type.starts_with("application/problem+json"));
    }

    #[test]
    fn rate_limit_carries_the_error_source_header() {
        let response = into_problem(OagwError::SecretNotFound("x".to_owned()));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response
                .headers()
                .get(crate::domain::error::ErrorSource::HEADER_NAME)
                .and_then(|value| value.to_str().ok()),
            Some("gateway")
        );
    }
}
