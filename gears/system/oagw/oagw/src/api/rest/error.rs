// @cpt-begin:cpt-cf-oagw-dod-gear-foundation-error-model:p1:inst-error
//! Mapping from domain errors onto the wire.
//!
//! Gateway errors are RFC 9457 problem documents carrying a global type system
//! identifier. Every response the gear returns carries `X-OAGW-Error-Source`,
//! so a caller can tell a gateway failure from a relayed upstream one.

use crate::domain::error::{DomainError, ERROR_SOURCE_HEADER, ErrorSource};
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode};
use toolkit_canonical_errors::Problem;

/// Stamp the error-source header onto a response.
pub fn set_error_source(response: &mut Response, source: ErrorSource) {
    response.headers_mut().insert(
        ERROR_SOURCE_HEADER,
        HeaderValue::from_static(source.as_str()),
    );
}

/// Render a domain error as a gateway problem response.
#[must_use]
pub fn problem_response(error: &DomainError) -> Response {
    let status = error.kind.status();
    let problem = Problem {
        problem_type: error.kind.gts_type().to_owned(),
        title: error.kind.title().to_owned(),
        status,
        detail: error.detail.clone(),
        instance: None,
        trace_id: None,
        context: error.context.clone(),
        error_code: None,
        error_domain: Some("oagw.v1".to_owned()),
    };
    let mut response = problem.into_response();
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    set_error_source(&mut response, ErrorSource::Gateway);
    response
}

impl IntoResponse for DomainError {
    fn into_response(self) -> Response {
        problem_response(&self)
    }
}
// @cpt-end:cpt-cf-oagw-dod-gear-foundation-error-model:p1:inst-error

#[cfg(test)]
mod tests {
    use super::problem_response;
    use crate::domain::error::{DomainError, ERROR_SOURCE_HEADER, ErrorKind};

    #[test]
    fn a_gateway_error_is_a_problem_document_marked_gateway() {
        let error = DomainError::new(ErrorKind::RouteNotFound, "no upstream with that alias");
        let response = problem_response(&error);
        assert_eq!(response.status(), 404);
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .expect("error source header"),
            "gateway"
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .expect("content type"),
            "application/problem+json"
        );
    }

    #[test]
    fn the_status_follows_the_error_kind() {
        for (kind, expected) in [
            (ErrorKind::ValidationError, 400),
            (ErrorKind::UpstreamAliasConflict, 409),
            (ErrorKind::RouteMatchConflict, 409),
            (ErrorKind::PayloadTooLarge, 413),
            (ErrorKind::RateLimitExceeded, 429),
            (ErrorKind::RequestTimeout, 504),
        ] {
            let response = problem_response(&DomainError::new(kind, "x"));
            assert_eq!(response.status(), expected, "{kind:?}");
        }
    }
}
