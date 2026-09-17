//! RFC 9457 problem+json responses for the OAGW API (DOCS §8).
//!
//! * Proxy-path failures map straight from [`DataPlaneError`], which
//!   already carries the exact `cf.oagw.*` GTS type, status and extension
//!   fields.
//! * Management-plane [`DomainError`]s map to the `cf.oagw.*` types where
//!   one exists and to the generic canonical `cf.core.err.*` categories
//!   otherwise.
//!
//! Extension fields ride in the `context` member so the api-gateway's
//! canonical error middleware — which re-serializes canonical `Problem`
//! envelopes and drops unknown *top-level* fields — preserves them. Every
//! OAGW-generated response carries `X-OAGW-Error-Source: gateway`
//! (ADR 0007).

use axum::Json;
use axum::http::header::HeaderName;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::json;

use crate::domain::error::{DataPlaneError, DomainError};
use crate::gts_helpers::{self, error_uri};
use crate::infra::proxy::{
    ERROR_SOURCE_GATEWAY, MAX_REQUEST_BODY_BYTES, X_OAGW_ERROR_SOURCE,
};

/// Generic canonical error-type fragments (no `cf.oagw.*` equivalent
/// exists for these management-plane failures).
mod canonical_type {
    pub const NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~";
    pub const ALREADY_EXISTS: &str = "gts.cf.core.errors.err.v1~cf.core.err.already_exists.v1~";
    pub const ABORTED: &str = "gts.cf.core.errors.err.v1~cf.core.err.aborted.v1~";
    pub const PERMISSION_DENIED: &str = crate::gts_helpers::ERR_PERMISSION_DENIED;
    pub const INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.core.err.internal.v1~";
    pub const SERVICE_UNAVAILABLE: &str =
        "gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~";
    pub const UNIMPLEMENTED: &str = "gts.cf.core.errors.err.v1~cf.core.err.unimplemented.v1~";
}

/// A gateway-generated problem+json error.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    problem_type: String,
    title: &'static str,
    detail: String,
    context: serde_json::Value,
    headers: HeaderMap,
}

impl ApiError {
    fn problem(
        status: StatusCode,
        problem_type: &str,
        title: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            problem_type: problem_type.to_owned(),
            title,
            detail: detail.into(),
            context: json!({}),
            headers: HeaderMap::new(),
        }
    }

    /// Request body exceeds the 100 MB hard cap (DOCS §8,
    /// `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`).
    #[must_use]
    pub fn payload_too_large(alias: &str, path: &str) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            problem_type: error_uri(gts_helpers::ERR_PAYLOAD_TOO_LARGE),
            title: "Payload Too Large",
            detail: format!("request body exceeds the {MAX_REQUEST_BODY_BYTES} byte limit"),
            context: json!({ "alias": alias, "path": path }),
            headers: HeaderMap::new(),
        }
    }

    /// Internal failure while reading the request body.
    #[must_use]
    pub fn body_read(error: impl std::fmt::Display) -> Self {
        Self::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            &error_uri(canonical_type::INTERNAL),
            "Internal Error",
            format!("failed to read request body: {error}"),
        )
    }

    fn internal(detail: impl Into<String>) -> Self {
        Self::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            &error_uri(canonical_type::INTERNAL),
            "Internal Error",
            detail,
        )
    }
}

impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::Validation { detail } => Self::problem(
                StatusCode::BAD_REQUEST,
                &error_uri(gts_helpers::ERR_VALIDATION),
                "Validation Error",
                detail,
            ),
            DomainError::NotFound { detail, .. } => Self::problem(
                StatusCode::NOT_FOUND,
                &error_uri(canonical_type::NOT_FOUND),
                "Not Found",
                detail,
            ),
            DomainError::AlreadyExists { detail } => Self::problem(
                StatusCode::CONFLICT,
                &error_uri(canonical_type::ALREADY_EXISTS),
                "Already Exists",
                detail,
            ),
            DomainError::Aborted { detail } => Self::problem(
                StatusCode::CONFLICT,
                &error_uri(canonical_type::ABORTED),
                "Operation Aborted",
                detail,
            ),
            DomainError::CrossTenantDenied { detail } => Self::problem(
                StatusCode::FORBIDDEN,
                &error_uri(canonical_type::PERMISSION_DENIED),
                "Permission Denied",
                detail,
            ),
            DomainError::PluginInUse {
                detail,
                referenced_by,
                ..
            } => {
                let mut this = Self::problem(
                    StatusCode::CONFLICT,
                    &error_uri(gts_helpers::ERR_PLUGIN_IN_USE),
                    "Plugin In Use",
                    detail,
                );
                this.context =
                    serde_json::to_value(referenced_by).unwrap_or_else(|_| json!({}));
                this
            }
            DomainError::ResolutionFailed { detail } | DomainError::Internal(detail) => {
                Self::internal(detail)
            }
            DomainError::SecretNotFound { detail } => Self::problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                &error_uri(gts_helpers::ERR_SECRET_NOT_FOUND),
                "Secret Not Found",
                detail,
            ),
            DomainError::ServiceUnavailable {
                detail,
                retry_after,
            } => {
                let mut this = Self::problem(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &error_uri(canonical_type::SERVICE_UNAVAILABLE),
                    "Service Unavailable",
                    detail,
                );
                if let Some(after) = retry_after {
                    install_retry_after(&mut this.headers, after.as_secs());
                }
                this
            }
            DomainError::UnsupportedOperation { detail } => Self::problem(
                StatusCode::NOT_IMPLEMENTED,
                &error_uri(canonical_type::UNIMPLEMENTED),
                "Not Implemented",
                detail,
            ),
        }
    }
}

impl From<DataPlaneError> for ApiError {
    fn from(err: DataPlaneError) -> Self {
        let status =
            StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut this = Self::problem(
            status,
            &error_uri(err.error_type()),
            err.title(),
            err.to_string(),
        );
        if let Ok(extensions) = serde_json::to_value(err.extensions())
            && !extensions.is_null()
        {
            this.context = extensions;
        }
        match &err {
            DataPlaneError::RateLimitExceeded {
                retry_after_seconds, ..
            } => install_retry_after(&mut this.headers, *retry_after_seconds),
            DataPlaneError::LinkUnavailable {
                retry_after: Some(after),
                ..
            } => install_retry_after(&mut this.headers, after.as_secs()),
            _ => {}
        }
        this
    }
}

fn install_retry_after(headers: &mut HeaderMap, seconds: u64) {
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        headers.insert(RETRY_AFTER, value);
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({
                "type": self.problem_type,
                "title": self.title,
                "status": self.status.as_u16(),
                "detail": self.detail,
                "context": self.context,
            })),
        )
            .into_response();
        response.headers_mut().insert(
            HeaderName::from_static(X_OAGW_ERROR_SOURCE),
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        if !self.headers.is_empty() {
            response.headers_mut().extend(self.headers);
        }
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

/// Attach `X-OAGW-Error-Source: gateway` (ADR 0007) to a management-plane
/// success response. (Proxy responses carry their own source header set by
/// the proxy pipeline.)
#[must_use]
pub fn gateway_source(response: Response) -> Response {
    let mut response = response;
    response.headers_mut().insert(
        HeaderName::from_static(X_OAGW_ERROR_SOURCE),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use crate::domain::error::ErrorExtensions;
    use uuid::Uuid;

    #[test]
    fn validation_error_maps_to_problem() {
        let error: ApiError = DomainError::validation("alias must be lowercase").into();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
        assert_eq!(
            response.headers()["content-type"],
            "application/problem+json"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&tokio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(to_bytes(response.into_body(), 1024))
                .expect("bytes"))
            .expect("problem json");
        assert_eq!(
            body["type"],
            "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(body["status"], 400);
        assert!(body["context"].is_object());
    }

    #[test]
    fn rate_limit_exceeded_carries_retry_after() {
        let error: ApiError = DataPlaneError::RateLimitExceeded {
            retry_after_seconds: 30,
            extensions: ErrorExtensions::default()
                .with_alias("api.openai.com")
                .with_path("/v1/chat"),
        }
        .into();
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        let response = error.into_response();
        assert_eq!(response.headers()["retry-after"], "30");
        assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    }

    #[test]
    fn plugin_in_use_context_keeps_references() {
        let error: ApiError = DomainError::plugin_in_use(
            Uuid::new_v4(),
            crate::domain::error::PluginReferences {
                upstreams: vec!["u".to_owned()],
                routes: vec!["r".to_owned()],
            },
        )
        .into();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(
            error.problem_type,
            "gts://gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
        );
        assert_eq!(error.context["routes"][0], "r");
    }

    #[test]
    fn management_not_found_uses_canonical_type() {
        let error: ApiError =
            DomainError::not_found("upstream", "no such upstream").into();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    }
}
