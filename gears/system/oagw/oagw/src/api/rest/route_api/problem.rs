//! Bare RFC 9457 envelopes for the two management-API error scenarios that
//! `cpt-cf-oagw-dod-route-error-mapping` documents as "status + envelope
//! only, no GTS `type` asserted" (controller decision D1): the
//! management-API by-id lookup miss (`404`, distinct from proxy-time
//! `RouteNotFound`) and the match-rule uniqueness collision (`409`).
//!
//! `crate::error::OagwError` cannot represent either case: every
//! [`crate::error::OagwErrorKind`] catalog entry carries a fixed GTS
//! `type`, and reusing the `400`-scoped `ValidationError` identifier at a
//! different HTTP status would defeat `type`-based client dispatch (the
//! defect this feature's own review found and corrected). Rather than
//! modify `error.rs` -- owned by entry 2.1 -- this sibling type lives here
//! and reuses only that module's already-`pub` error-source constants.

// @cpt-dod:cpt-cf-oagw-dod-route-error-mapping:p1
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;

use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME};

const FALLBACK_BODY: &[u8] =
    br#"{"type":"about:blank","title":"Internal","status":500,"detail":"failed to serialize problem"}"#;

/// RFC 9457 envelope carrying only `type: "about:blank"` (the RFC's own
/// default when no more specific `type` is asserted), `title`, `status`,
/// and `detail` -- deliberately no GTS-typed `type` member.
#[derive(Debug, Clone, Serialize)]
pub struct BareProblem {
    #[serde(rename = "type")]
    problem_type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
}

impl BareProblem {
    /// `404` -- management-API by-id lookup miss
    /// (`cpt-cf-oagw-algo-route-tenant-scope-resolve`).
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self {
            problem_type: "about:blank",
            title: "Not Found",
            status: StatusCode::NOT_FOUND.as_u16(),
            detail: detail.into(),
        }
    }

    /// `409` -- match-rule uniqueness collision
    /// (`cpt-cf-oagw-algo-route-uniqueness-check`).
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self {
            problem_type: "about:blank",
            title: "Conflict",
            status: StatusCode::CONFLICT.as_u16(),
            detail: detail.into(),
        }
    }
}

impl IntoResponse for BareProblem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| FALLBACK_BODY.to_vec());
        let mut response = (
            status,
            [(header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)],
            body,
        )
            .into_response();
        response.headers_mut().insert(
            header::HeaderName::from_static(ERROR_SOURCE_HEADER_NAME),
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        response
    }
}

/// Union of the two error shapes a route-management handler can return:
/// a catalog-typed [`crate::error::OagwError`] (`400 ValidationError`), or
/// a [`BareProblem`] (`404`/`409`).
#[derive(Debug, Clone)]
pub enum RouteApiError {
    Typed(crate::error::OagwError),
    Bare(BareProblem),
}

impl From<crate::error::OagwError> for RouteApiError {
    fn from(err: crate::error::OagwError) -> Self {
        Self::Typed(err)
    }
}

impl From<BareProblem> for RouteApiError {
    fn from(err: BareProblem) -> Self {
        Self::Bare(err)
    }
}

impl IntoResponse for RouteApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Typed(err) => err.into_response(),
            Self::Bare(err) => err.into_response(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn not_found_renders_status_404_with_no_type_and_the_gateway_header() {
        let response = BareProblem::not_found("route not found").into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(APPLICATION_PROBLEM_JSON)
        );
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["type"], "about:blank");
        assert_eq!(json["status"], 404);
    }

    #[tokio::test]
    async fn conflict_renders_status_409_with_no_type() {
        let response = BareProblem::conflict("collision").into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["type"], "about:blank");
        assert_eq!(json["status"], 409);
    }

    #[tokio::test]
    async fn route_api_error_dispatches_both_variants() {
        let typed: RouteApiError =
            crate::error::OagwError::new(crate::error::OagwErrorKind::ValidationError, "bad")
                .into();
        assert_eq!(typed.into_response().status(), StatusCode::BAD_REQUEST);

        let bare: RouteApiError = BareProblem::not_found("missing").into();
        assert_eq!(bare.into_response().status(), StatusCode::NOT_FOUND);
    }
}
