//! Rendering a data-plane failure as an RFC 9457 problem document.
//!
//! [`ProxyFailure`] already carries everything the wire needs — status, GTS
//! `type`, `title`, `detail`, extension members and the extra headers the
//! failure wants (`Retry-After`, `X-RateLimit-*`, `Vary`). This module only
//! turns it into bytes, plus the one header `docs/ADR/0007` adds:
//! `X-OAGW-Error-Source`, which distinguishes a *gateway* decision from an
//! *upstream* answer that is being passed through.
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use toolkit_canonical_errors::Problem;

use crate::infra::proxy::{ErrorSource, ProxyFailure};

/// Media type of an RFC 9457 problem document.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// A data-plane failure, ready to be rendered on the wire.
#[derive(Debug, Clone)]
pub struct ProxyError {
    failure: ProxyFailure,
}

impl ProxyError {
    /// Wrap a data-plane failure.
    #[must_use]
    pub const fn new(failure: ProxyFailure) -> Self {
        Self { failure }
    }

    /// The failure this error renders.
    #[must_use]
    pub const fn failure(&self) -> &ProxyFailure {
        &self.failure
    }

    /// The problem document the failure renders to.
    #[must_use]
    pub fn problem(&self) -> Problem {
        let status = self.failure.status;
        Problem {
            problem_type: self.failure.type_uri.clone(),
            title: self.failure.title.clone(),
            status,
            detail: self.failure.detail.clone(),
            instance: None,
            trace_id: None,
            context: self.failure.context.clone(),
            error_code: None,
            error_domain: None,
        }
    }
}

impl From<ProxyFailure> for ProxyError {
    fn from(failure: ProxyFailure) -> Self {
        Self::new(failure)
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.failure.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = match serde_json::to_vec(&self.problem()) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "oagw: failed to serialize problem document");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, PROBLEM_JSON)],
                    r#"{"type":"gts.cf.core.errors.err.v1~cf.core.err.internal.v1","title":"Internal Error","status":500,"detail":"internal error","context":{}}"#,
                )
                    .into_response();
            }
        };
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(PROBLEM_JSON),
        );
        response.headers_mut().insert(
            crate::api::rest::error::ERROR_SOURCE_HEADER,
            header::HeaderValue::from_static(source_value(self.failure.source)),
        );
        for (name, value) in self.failure.headers.iter() {
            response.headers_mut().append(name, value.clone());
        }
        response
    }
}

/// Wire value of [`crate::infra::proxy::ErrorSource`].
#[must_use]
pub const fn source_value(source: ErrorSource) -> &'static str {
    source.as_str()
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
