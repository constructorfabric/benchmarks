//! REST error mapping: [`DomainError`] → RFC 9457 `application/problem+json`.
//!
//! The OAGW catalogue (`docs/DESIGN.md` §3.3 "Error Response Format") pins
//! GTS identifiers per error class, so the gear renders its own `Problem`
//! envelope instead of letting the canonical category decide the `type`.
//! Platform-standard context members (`resource_type`, `resource_name`,
//! `violations`) are still emitted, and every OAGW extension member travels
//! inside `context`, because the canonical error middleware round-trips
//! problem bodies through `toolkit_canonical_errors::Problem` (which keeps
//! `type` / `title` / `status` / `detail` / `context` and drops unknown
//! top-level members).
//!
//! Two catalogue entries do not exist in `DESIGN.md`'s table — management
//! 404s and management 409s other than `PluginInUse`. They use the platform's
//! canonical `not_found` / `already_exists` identifiers, whose semantics
//! match exactly; the table's only 404 entry (`cf.oagw.route.not_found.v1`)
//! describes proxy route matching, which the management plane never does.
use axum::{
    body::Body,
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;
use toolkit_canonical_errors::{CanonicalError, Problem};
use toolkit_gts::gts_id;

use crate::domain::error::DomainError;
use crate::domain::gts;

/// Error-source response header (`docs/ADR/0007-error-source-distinction.md`).
pub const ERROR_SOURCE_HEADER: HeaderName = HeaderName::from_static("x-oagw-error-source");
/// Value of [`ERROR_SOURCE_HEADER`] for management-plane errors.
pub const GATEWAY_SOURCE: &str = "gateway";
/// Media type of an RFC 9457 problem document.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Request payload validation failure.
pub const VALIDATION_TYPE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
/// Addressed resource does not exist for the calling tenant.
pub const NOT_FOUND_TYPE: &str = gts_id!("cf.core.errors.err.v1~cf.core.err.not_found.v1");
/// Alias / match-key uniqueness violation.
pub const ALREADY_EXISTS_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.core.err.already_exists.v1");
/// Plugin still referenced by an upstream or a route.
pub const PLUGIN_IN_USE_TYPE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
/// Opaque internal failure.
pub const INTERNAL_TYPE: &str = gts_id!("cf.core.errors.err.v1~cf.core.err.internal.v1");

/// Handler result type. `T` must be [`axum::response::IntoResponse`].
pub type ApiResult<T> = Result<T, ApiError>;

/// An error ready to be rendered on the wire.
///
/// The problem document is boxed: handlers return `Result<_, ApiError>`
/// everywhere, and a boxed error keeps the `Result` small enough to pass
/// through the generated handler signatures.
#[derive(Debug, Clone)]
pub struct ApiError {
    problem: Box<Problem>,
}

impl ApiError {
    /// A 400 validation problem carrying `detail` verbatim.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::with_problem(
            StatusCode::BAD_REQUEST.as_u16(),
            VALIDATION_TYPE,
            "Validation Error",
            detail,
            json!({}),
        )
    }

    /// A 404 problem naming the missing resource.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::with_problem(
            StatusCode::NOT_FOUND.as_u16(),
            NOT_FOUND_TYPE,
            "Not Found",
            detail,
            json!({}),
        )
    }

    /// A 409 conflict problem.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::with_problem(
            StatusCode::CONFLICT.as_u16(),
            ALREADY_EXISTS_TYPE,
            "Conflict",
            detail,
            json!({}),
        )
    }

    /// A 500 problem with an opaque wire detail.
    #[must_use]
    pub fn internal() -> Self {
        Self::with_problem(
            StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            INTERNAL_TYPE,
            "Internal Error",
            "internal error",
            json!({}),
        )
    }

    fn with_problem(
        status: u16,
        problem_type: &str,
        title: &str,
        detail: impl Into<String>,
        context: serde_json::Value,
    ) -> Self {
        Self {
            problem: Box::new(Problem {
                problem_type: problem_type.to_owned(),
                title: title.to_owned(),
                status,
                detail: detail.into(),
                instance: None,
                trace_id: None,
                context,
                error_code: None,
                error_domain: None,
            }),
        }
    }
}

impl From<DomainError> for ApiError {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::Validation { detail } => Self::validation(detail),
            DomainError::FieldViolation {
                field,
                reason,
                detail,
            } => Self::with_problem(
                StatusCode::BAD_REQUEST.as_u16(),
                VALIDATION_TYPE,
                "Validation Error",
                detail.clone(),
                json!({
                    "field": field,
                    "reason": reason,
                    "violations": [{
                        "field": field,
                        "description": detail,
                        "reason": reason,
                    }],
                }),
            ),
            DomainError::NotFound { detail } => Self::not_found(detail),
            DomainError::AliasConflict { alias, tenant_id } => Self::with_problem(
                StatusCode::CONFLICT.as_u16(),
                ALREADY_EXISTS_TYPE,
                "Conflict",
                format!("alias '{alias}' is already used by another upstream of this tenant"),
                json!({
                    "resource_type": resource_type(gts::UPSTREAM_TYPE),
                    "resource_name": alias,
                    "alias": alias,
                    "tenant_id": tenant_id.to_string(),
                }),
            ),
            DomainError::Conflict { detail } => Self::conflict(detail),
            DomainError::RouteMatchConflict { detail } => Self::with_problem(
                StatusCode::CONFLICT.as_u16(),
                ALREADY_EXISTS_TYPE,
                "Conflict",
                detail,
                json!({
                    "resource_type": resource_type(gts::ROUTE_TYPE),
                    "reason": "route_match_conflict",
                }),
            ),
            DomainError::PluginInUse {
                plugin_id,
                references,
            } => Self::with_problem(
                StatusCode::CONFLICT.as_u16(),
                PLUGIN_IN_USE_TYPE,
                "Plugin In Use",
                format!("plugin is referenced by {}", references.describe()),
                json!({
                    "plugin_id": plugin_id,
                    "referenced_by": {
                        "upstreams": references.upstreams,
                        "routes": references.routes,
                    },
                }),
            ),
            DomainError::UnknownPluginRef { detail } => Self::with_problem(
                StatusCode::BAD_REQUEST.as_u16(),
                VALIDATION_TYPE,
                "Validation Error",
                detail,
                json!({ "reason": crate::domain::reason::PLUGIN_UNKNOWN }),
            ),
            DomainError::AliasRule { detail } => Self::validation(detail),
            DomainError::ImmutableField { field } => Self::with_problem(
                StatusCode::BAD_REQUEST.as_u16(),
                VALIDATION_TYPE,
                "Validation Error",
                format!("immutable field '{field}' cannot be changed"),
                json!({
                    "field": field,
                    "reason": crate::domain::reason::IMMUTABLE,
                }),
            ),
            DomainError::Internal { diagnostic, .. } => {
                // The diagnostic stays server-side: it is logged here and the
                // wire detail stays opaque (`DESIGN.md` §3.6).
                tracing::error!(diagnostic = %diagnostic, "oagw internal error");
                Self::internal()
            }
        }
    }
}

impl From<CanonicalError> for ApiError {
    fn from(error: CanonicalError) -> Self {
        Self {
            problem: Box::new(Problem::from(error)),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = match serde_json::to_vec(&self.problem) {
            Ok(bytes) => bytes,
            // `Problem` serializes to JSON infallibly in practice; fall back
            // to the canonical renderer rather than emitting a broken body.
            Err(error) => {
                tracing::error!(error = %error, "failed to serialize problem document");
                return CanonicalError::internal("failed to render error response")
                    .create()
                    .into_response();
            }
        };
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        // A problem document must carry the problem media type, not the
        // generic `application/json` an axum `Json` body would get.
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(PROBLEM_JSON),
        );
        response.headers_mut().insert(
            ERROR_SOURCE_HEADER,
            header::HeaderValue::from_static(GATEWAY_SOURCE),
        );
        response
    }
}

/// The anonymous GTS resource type used in `context.resource_type`
/// (`gts.cf.core.oagw.upstream.v1~`).
#[must_use]
pub fn resource_type(type_id: &str) -> String {
    format!("gts.{type_id}~")
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
