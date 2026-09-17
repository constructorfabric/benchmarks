//! Control-plane error hierarchy and the `DomainError → CanonicalError`
//! ladder for the OAGW REST API.
//!
//! The data plane has its own error vocabulary (GTS-typed RFC 9457 Problems,
//! see `crate::infra::proxy::problem`); this module is strictly the control
//! plane (upstream / route / plugin CRUD).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::gts;

/// Canonical resource marker for OAGW control-plane entities. Every managed
/// resource error carries a resource type in `{upstream, route, plugin}`.
#[resource_error(gts_id!("cf.core.oagw.config.v1~"))]
pub struct OagwConfigError;

/// Codified (not free-text) violation codes for field-level violations.
pub mod code {
    /// `alias` field.
    pub const ALIAS_FIELD: &str = "alias";
    /// Server endpoints field.
    pub const SERVER_FIELD: &str = "server";
    /// Route match field.
    pub const MATCH_FIELD: &str = "match";
    /// Plugin binding field.
    pub const PLUGINS_FIELD: &str = "plugins";
    /// Auth configuration field.
    pub const AUTH_FIELD: &str = "auth";
    /// CORS configuration field.
    pub const CORS_FIELD: &str = "cors";
    /// Rate limit configuration field.
    pub const RATE_LIMIT_FIELD: &str = "rateLimit";
    /// Upstream reference on a route.
    pub const UPSTREAM_ID_FIELD: &str = "upstreamId";

    // Violation codes.
    pub const INVALID_FORMAT: &str = "INVALID_FORMAT";
    pub const INVALID_VALUE: &str = "INVALID_VALUE";
    pub const MISSING: &str = "MISSING";
    pub const UNSUPPORTED: &str = "UNSUPPORTED";
    pub const CONFLICT: &str = "CONFLICT";
    pub const IMMUTABLE: &str = "IMMUTABLE";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const ALIAS_IN_USE: &str = "ALIAS_IN_USE";
    pub const INVALID_ALIAS: &str = "INVALID_ALIAS";
    pub const ALIAS_CHANGE: &str = "ALIAS_CHANGE";
}

/// Control-plane domain error.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// A field-level validation failure → 400.
    #[error("Invalid value for field '{field}': {detail}")]
    Validation {
        field: &'static str,
        detail: String,
        code: &'static str,
    },

    /// Resource-level validation with no single field → 400.
    #[error("{0}")]
    Invalid(String),

    /// A referenced resource does not exist or is not visible → 404.
    #[error("No {kind} with id {id}")]
    NotFound { kind: &'static str, id: String },

    /// A uniqueness constraint was violated → 409.
    #[error("{0}")]
    Conflict(String),

    /// Alias derivation is impossible (IP endpoints / nil common suffix) and
    /// no explicit alias was provided → 400.
    #[error("{0}")]
    AliasMissing(String),

    /// The provided alias does not equal the derived alias for hostname
    /// endpoints → 422 semantics via 400.
    #[error("{0}")]
    AliasMismatch(String),

    /// Attempting to modify an immutable field → 400.
    #[error("{0}")]
    Immutable(String),

    /// An ancestor's sharing mode (`enforce`) forbids the change → 403.
    #[error("{0}")]
    Forbidden(String),

    /// Internal/unexpected failure → 500.
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl DomainError {
    /// Validation error for a specific field.
    #[must_use]
    pub fn validation(
        field: &'static str,
        detail: impl Into<String>,
        code: &'static str,
    ) -> Self {
        Self::Validation {
            field,
            detail: detail.into(),
            code,
        }
    }

    /// `404` for a missing resource.
    #[must_use]
    pub fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            id: id.into(),
        }
    }

    /// `409` for a uniqueness conflict.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict(detail.into())
    }
}

impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::Validation {
                field,
                detail,
                code,
            } => OagwConfigError::invalid_argument()
                .with_field_violation(field, detail, code)
                .create(),
            DomainError::Invalid(detail) => OagwConfigError::invalid_argument()
                .with_field_violation("request", detail, code::INVALID_VALUE)
                .create(),
            DomainError::NotFound { kind, id } => {
                let resource_type = match kind {
                    crate::domain::models::Upstream::KIND => gts::UPSTREAM_RESOURCE_TYPE,
                    crate::domain::models::Route::KIND => gts::ROUTE_RESOURCE_TYPE,
                    crate::domain::models::Plugin::KIND => gts::PLUGIN_RESOURCE_TYPE,
                    _ => gts::UPSTREAM_RESOURCE_TYPE,
                };
                OagwConfigError::not_found(format!("No {kind} with id {id}"))
                    .with_resource(format!("{resource_type}{id}"))
                    .create()
            }
            DomainError::Conflict(detail) => OagwConfigError::already_exists(detail.clone())
                .with_resource(detail)
                .create(),
            DomainError::AliasMissing(detail) => OagwConfigError::invalid_argument()
                .with_field_violation(code::ALIAS_FIELD, detail, code::MISSING)
                .create(),
            DomainError::AliasMismatch(detail) => OagwConfigError::invalid_argument()
                .with_field_violation(code::ALIAS_FIELD, detail, code::INVALID_ALIAS)
                .create(),
            DomainError::Immutable(detail) => OagwConfigError::invalid_argument()
                .with_field_violation(code::ALIAS_FIELD, detail, code::IMMUTABLE)
                .create(),
            DomainError::Forbidden(detail) => OagwConfigError::permission_denied()
                .with_reason(format!(
                    "Forbidden by upstream sharing configuration: {detail}"
                ))
                .create(),
            DomainError::Internal(e) => {
                tracing::error!(error = ?e, "oagw control plane internal error");
                CanonicalError::internal(e.to_string()).create()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolkit_canonical_errors::Problem;

    fn problem_from(err: DomainError) -> Problem {
        Problem::from(CanonicalError::from(err))
    }

    #[test]
    fn validation_maps_to_400() {
        let p = problem_from(DomainError::validation("alias", "bad", code::INVALID_ALIAS));
        assert_eq!(p.status, 400);
    }

    #[test]
    fn not_found_maps_to_404() {
        let p = problem_from(DomainError::not_found("upstream", "deadbeef"));
        assert_eq!(p.status, 404);
        assert!(p.detail.contains("upstream"));
    }

    #[test]
    fn conflict_maps_to_409() {
        let p = problem_from(DomainError::conflict("alias 'foo' already exists"));
        assert_eq!(p.status, 409);
    }

    #[test]
    fn internal_maps_to_500() {
        let p = problem_from(DomainError::Internal(anyhow::anyhow!("boom")));
        assert_eq!(p.status, 500);
    }
}
