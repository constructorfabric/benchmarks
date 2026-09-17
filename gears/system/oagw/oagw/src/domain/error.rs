//! Domain error type for the OAGW gear.
//!
//! Errors are mapped to RFC 9457 problem+json responses in
//! `crate::api::rest::error` with the GTS identifiers from the DESIGN error
//! table and an `X-OAGW-Error-Source: gateway` header.

use thiserror::Error;
use uuid::Uuid;

use crate::domain::model::PluginKind;

/// A gateway error with full response context (status + GTS id + extras).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemSpec {
    /// GTS instance id used as the Problem `type`.
    pub gts_type: &'static str,
    /// HTTP status code.
    pub status: u16,
    /// RFC 9457 title.
    pub title: &'static str,
    /// Human-readable detail.
    pub detail: String,
    /// Request context / extension fields.
    pub context: Vec<(String, String)>,
    /// Retry guidance (seconds) for retriable errors.
    pub retry_after_seconds: Option<u64>,
}

#[derive(Debug, Error)]
pub enum DomainError {
    /// Request/DTO validation failed.
    #[error("{0}")]
    Validation(String),

    /// Resolved upstream is disabled (503 LinkUnavailable).
    #[error("upstream '{0}' is disabled")]
    UpstreamDisabled(String),

    /// No matching route found.
    #[error("no route matched upstream '{0}' for method {1} path {2}")]
    RouteNotFound(String, String, String),

    /// Upstream not found by alias during resolution.
    #[error("no upstream found for alias '{0}'")]
    AliasNotFound(String),

    /// Upstream failed alias-uniqueness / binding constraints.
    #[error("alias '{0}' conflicts with an existing upstream (sharing mode: {1:?})")]
    AliasConflict(String, crate::domain::model::SharingMode),

    /// Alias derivation / enforcement rule violation.
    #[error("alias violation: {0}")]
    AliasViolation(String),

    /// Resource not found.
    #[error("{resource} '{id}' not found")]
    NotFound { resource: &'static str, id: String },

    /// Route match uniqueness violation (path + priority + method).
    #[error("route match conflict: {0}")]
    RouteConflict(String),

    /// Custom plugin is referenced by an upstream/route (deletion blocked).
    #[error("plugin '{0}' is in use")]
    PluginInUse(String),

    /// Custom plugin not found in the registry.
    #[error("plugin '{0}' not found")]
    PluginNotFound(String),

    /// Referenced secret is missing from the credential store.
    #[error("secret '{0}' not found")]
    SecretNotFound(String),

    /// Access control denial (upstream binding / proxy invoke).
    #[error("access denied: {0}")]
    AccessDenied(String),

    /// External service request failure (auth token fetch, upstream I/O).
    #[error("external request failed: {0}")]
    Upstream(String),

    /// Protocol-level error.
    #[error("protocol error: {0}")]
    ProtocolError(String),

    /// Internal / unexpected error (surfaced as 500).
    #[error("internal error: {0}")]
    Internal(String),

    /// A fully-formed gateway problem (custom status/type), carried through
    /// `DomainError` for uniformity (e.g. target-host and CORS rejections).
    #[error("gateway problem: {0:?}")]
    Problem(ProblemSpec),
}

impl DomainError {
    /// Validation shortcut.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// Whether this error is safe to retry from a client standpoint.
    #[must_use]
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            Self::UpstreamDisabled(_)
                | Self::AliasNotFound(_)
                | Self::RouteNotFound(..)
                | Self::Upstream(_)
        )
    }

    /// Map this domain error onto a [`ProblemSpec`] (gateway error).
    #[must_use]
    pub fn to_problem(&self) -> ProblemSpec {
        use crate::domain::gts as g;
        match self {
            Self::Validation(msg) => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 400,
                title: "Validation Error",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::AliasViolation(msg) => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 400,
                title: "Validation Error",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::RouteConflict(msg) => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 400,
                title: "Route Error",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::AccessDenied(msg) => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 403,
                title: "Forbidden",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::AliasNotFound(alias) => ProblemSpec {
                gts_type: g::ERR_ROUTE_NOT_FOUND,
                status: 404,
                title: "Route Not Found",
                detail: format!("no upstream found for alias '{alias}'"),
                context: vec![("alias".to_owned(), alias.clone())],
                retry_after_seconds: None,
            },
            Self::RouteNotFound(alias, method, path) => ProblemSpec {
                gts_type: g::ERR_ROUTE_NOT_FOUND,
                status: 404,
                title: "Route Not Found",
                detail: format!("no route matched upstream '{alias}' for {method} {path}"),
                context: vec![
                    ("alias".to_owned(), alias.clone()),
                    ("path".to_owned(), path.clone()),
                ],
                retry_after_seconds: None,
            },
            Self::NotFound { resource, id } => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 404,
                title: "Not Found",
                detail: format!("{resource} '{id}' not found"),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::PluginInUse(name) => ProblemSpec {
                gts_type: g::ERR_PLUGIN_IN_USE,
                status: 409,
                title: "Plugin In Use",
                detail: format!("plugin '{name}' is referenced by an upstream or route"),
                context: vec![("plugin".to_owned(), name.clone())],
                retry_after_seconds: None,
            },
            Self::PluginNotFound(name) => ProblemSpec {
                gts_type: g::ERR_PLUGIN_NOT_FOUND,
                status: 503,
                title: "Plugin Not Found",
                detail: format!("plugin '{name}' not found"),
                context: vec![("plugin".to_owned(), name.clone())],
                retry_after_seconds: None,
            },
            Self::AliasConflict(alias, mode) => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 409,
                title: "Conflict",
                detail: format!(
                    "alias '{alias}' already exists in this tenant or conflicts with an ancestor (sharing: {mode:?})"
                ),
                context: vec![("alias".to_owned(), alias.clone())],
                retry_after_seconds: None,
            },
            Self::UpstreamDisabled(alias) => ProblemSpec {
                gts_type: g::ERR_LINK_UNAVAILABLE,
                status: 503,
                title: "Link Unavailable",
                detail: format!("upstream '{alias}' is disabled"),
                context: vec![("alias".to_owned(), alias.clone())],
                retry_after_seconds: Some(5),
            },
            Self::SecretNotFound(ref_name) => ProblemSpec {
                gts_type: g::ERR_SECRET_NOT_FOUND,
                status: 500,
                title: "Secret Not Found",
                detail: format!("referenced secret '{ref_name}' not found"),
                context: vec![("secret_ref".to_owned(), ref_name.clone())],
                retry_after_seconds: None,
            },
            Self::ProtocolError(msg) => ProblemSpec {
                gts_type: g::ERR_PROTOCOL_ERROR,
                status: 502,
                title: "Protocol Error",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::Upstream(msg) => ProblemSpec {
                gts_type: g::ERR_DOWNSTREAM_ERROR,
                status: 502,
                title: "Downstream Error",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: Some(1),
            },
            Self::Internal(msg) => ProblemSpec {
                gts_type: g::ERR_VALIDATION,
                status: 500,
                title: "Internal Error",
                detail: msg.clone(),
                context: Vec::new(),
                retry_after_seconds: None,
            },
            Self::Problem(problem) => problem.clone(),
        }
    }

    /// Convenience: build a not-found for models.
    #[must_use]
    pub fn not_found(resource: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource,
            id: id.into(),
        }
    }

    /// Wrap a fully-formed gateway problem.
    #[must_use]
    pub fn problem(problem: ProblemSpec) -> Self {
        Self::Problem(problem)
    }
}

/// Trait helper to tag plugin kinds in errors.
#[allow(dead_code)]
pub(crate) fn plugin_kind_label(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Auth => "auth",
        PluginKind::Guard => "guard",
        PluginKind::Transform => "transform",
    }
}

/// Compile-time guard preventing accidental use of `uuid` without purpose.
#[allow(dead_code)]
pub(crate) fn _use_uuid(_id: Uuid) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts;

    /// Assert the error maps to the expected status/GTS type per the DESIGN
    /// §3.3 error table.
    fn check(e: DomainError, status: u16, gts_type: &'static str) {
        let p = e.to_problem();
        assert_eq!(p.status, status, "status for {p:?}");
        assert_eq!(p.gts_type, gts_type, "gts type for {p:?}");
    }

    #[test]
    fn validation_errors_map_to_400_validation() {
        check(DomainError::validation("bad"), 400, gts::ERR_VALIDATION);
        check(DomainError::AliasViolation("x".into()), 400, gts::ERR_VALIDATION);
        check(DomainError::RouteConflict("x".into()), 400, gts::ERR_VALIDATION);
    }

    #[test]
    fn access_denied_maps_to_403() {
        check(DomainError::AccessDenied("nope".into()), 403, gts::ERR_VALIDATION);
    }

    #[test]
    fn not_found_variants_map_to_404() {
        check(DomainError::AliasNotFound("a".into()), 404, gts::ERR_ROUTE_NOT_FOUND);
        check(
            DomainError::RouteNotFound("a".into(), "GET".into(), "/p".into()),
            404,
            gts::ERR_ROUTE_NOT_FOUND,
        );
        check(DomainError::not_found("upstream", "id"), 404, gts::ERR_VALIDATION);
    }

    #[test]
    fn conflict_variants_map_to_409() {
        check(
            DomainError::PluginInUse("p".into()),
            409,
            gts::ERR_PLUGIN_IN_USE,
        );
        check(
            DomainError::AliasConflict("a".into(), crate::domain::model::SharingMode::Enforce),
            409,
            gts::ERR_VALIDATION,
        );
    }

    #[test]
    fn downstream_and_protocol_map_to_502() {
        check(DomainError::ProtocolError("x".into()), 502, gts::ERR_PROTOCOL_ERROR);
        check(DomainError::Upstream("boom".into()), 502, gts::ERR_DOWNSTREAM_ERROR);
    }

    #[test]
    fn unavailable_maps_to_503_with_retry_after() {
        let p = DomainError::UpstreamDisabled("a".into()).to_problem();
        assert_eq!(p.status, 503);
        assert_eq!(p.gts_type, gts::ERR_LINK_UNAVAILABLE);
        assert_eq!(p.retry_after_seconds, Some(5));
        check(DomainError::PluginNotFound("p".into()), 503, gts::ERR_PLUGIN_NOT_FOUND);
    }

    #[test]
    fn internal_and_secret_maps_to_500() {
        check(
            DomainError::SecretNotFound("cred://x".into()),
            500,
            gts::ERR_SECRET_NOT_FOUND,
        );
        check(DomainError::Internal("boom".into()), 500, gts::ERR_VALIDATION);
    }

    #[test]
    fn problem_passthrough_keeps_shape() {
        let spec = ProblemSpec {
            gts_type: gts::ERR_RATE_LIMIT_EXCEEDED,
            status: 429,
            title: "Rate Limit Exceeded",
            detail: "slow down".into(),
            context: vec![("alias".into(), "a".into())],
            retry_after_seconds: Some(2),
        };
        let p = DomainError::Problem(spec.clone()).to_problem();
        assert_eq!(p, spec);
    }

    #[test]
    fn retriable_flag_matches_design_table() {
        assert!(DomainError::AliasNotFound("a".into()).is_retriable());
        assert!(DomainError::RouteNotFound("a".into(), "GET".into(), "/p".into()).is_retriable());
        assert!(DomainError::UpstreamDisabled("a".into()).is_retriable());
        assert!(DomainError::Upstream("boom".into()).is_retriable());
        assert!(!DomainError::validation("bad").is_retriable());
        assert!(!DomainError::SecretNotFound("cred://x".into()).is_retriable());
        assert!(!DomainError::Internal("boom".into()).is_retriable());
    }
}
