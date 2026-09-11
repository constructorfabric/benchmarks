// Updated: 2026-09-01 by Constructor Tech
//! Domain errors for the OAGW gear.
//!
//! The Control Plane speaks [`DomainError`]; the REST layer projects it into
//! RFC 9457 problem documents carrying the OAGW GTS error identifiers
//! ([`crate::gts::ERR_PREFIX`]). The Data Plane speaks
//! [`GatewayError`](crate::infra::proxy::error::GatewayError), which is a
//! different concern (it answers *downstream* callers) and is not projected
//! from here.

use serde::Serialize;
use uuid::Uuid;

/// One field-level validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationIssue {
    /// Dot path of the offending field, e.g. `server.endpoints[0].port`.
    pub field: String,
    /// Human-readable explanation, safe to echo to the caller.
    pub message: String,
}

impl ValidationIssue {
    #[must_use]
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Resources that reference a plugin, as reported by [`DomainError::PluginInUse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReferencedBy {
    /// Upstream GTS identifiers referencing the plugin.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub upstreams: Vec<String>,
    /// Route GTS identifiers referencing the plugin.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub routes: Vec<String>,
}

impl ReferencedBy {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

/// Control Plane domain error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    /// The payload failed schema or semantic validation.
    #[error("validation failed: {}", .0.iter().map(|i| format!("{}: {}", i.field, i.message)).collect::<Vec<_>>().join("; "))]
    Validation(Vec<ValidationIssue>),
    /// A single field was rejected. Convenience for [`DomainError::Validation`].
    #[error("validation failed on {field}: {message}")]
    InvalidField { field: String, message: String },
    /// The referenced resource does not exist (or belongs to another tenant).
    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: String },
    /// The write would violate a uniqueness or immutability rule.
    #[error("{kind} conflict: {message}")]
    Conflict { kind: &'static str, message: String },
    /// The plugin is still referenced by at least one upstream or route.
    #[error("plugin {plugin_id} is still in use")]
    PluginInUse {
        plugin_id: String,
        referenced_by: ReferencedBy,
    },
    /// The plugin being written already exists.
    #[error("plugin already exists: {0}")]
    PluginAlreadyExists(String),
    /// No valid tenant could be resolved from the security context.
    #[error("no tenant context available")]
    NoTenant,
    /// Authenticated but not permitted.
    #[error("access denied")]
    Forbidden,
    /// Malformed identifier in the request path.
    #[error("invalid identifier: {0}")]
    BadIdentifier(String),
    /// Anything unexpected. Never carries secrets.
    #[error("internal error")]
    Internal(String),
}

impl DomainError {
    /// Convenience constructor for a one-field validation failure.
    #[must_use]
    pub fn invalid(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidField {
            field: field.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn not_found(kind: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            kind,
            id: id.to_string(),
        }
    }

    #[must_use]
    pub fn upstream_not_found(id: impl std::fmt::Display) -> Self {
        Self::not_found("upstream", id)
    }

    #[must_use]
    pub fn route_not_found(id: impl std::fmt::Display) -> Self {
        Self::not_found("route", id)
    }

    #[must_use]
    pub fn plugin_not_found(id: impl std::fmt::Display) -> Self {
        Self::not_found("plugin", id)
    }

    /// Helper for building the aggregated `Validation` variant.
    #[must_use]
    pub fn from_issues(issues: Vec<ValidationIssue>) -> Self {
        Self::Validation(issues)
    }
}

impl From<Vec<ValidationIssue>> for DomainError {
    fn from(issues: Vec<ValidationIssue>) -> Self {
        Self::Validation(issues)
    }
}

/// Merge helper used by validators that collect every issue rather than the
/// first one.
pub struct IssueCollector(Vec<ValidationIssue>);

impl IssueCollector {
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Record a failure unless `cond` holds.
    pub fn require(&mut self, cond: bool, field: &str, message: &str) {
        if !cond {
            self.0.push(ValidationIssue::new(field, message));
        }
    }

    /// Record a failure when `cond` holds.
    pub fn reject(&mut self, cond: bool, field: &str, message: &str) {
        if cond {
            self.0.push(ValidationIssue::new(field, message));
        }
    }

    pub fn push(&mut self, issue: ValidationIssue) {
        self.0.push(issue);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Finish the validation, returning `Err` when anything was collected.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] when at least one issue was recorded.
    pub fn finish(self) -> Result<(), DomainError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DomainError::Validation(self.0))
        }
    }
}

impl Default for IssueCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract a bare [`Uuid`] from a path segment that may be either a raw UUID
/// or a GTS instance identifier (`gts.cf.core.oagw.upstream.v1~<uuid>`).
///
/// # Errors
///
/// [`DomainError::BadIdentifier`] when the segment names neither form.
pub fn parse_resource_id(raw: &str, kind: &'static str) -> Result<Uuid, DomainError> {
    let candidate = raw.rsplit('~').next().unwrap_or(raw);
    Uuid::parse_str(candidate)
        .map_err(|_| DomainError::BadIdentifier(format!("'{raw}' is not a valid {kind} id")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resource_id_accepts_bare_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(parse_resource_id(&id.to_string(), "upstream").unwrap(), id);
    }

    #[test]
    fn parse_resource_id_accepts_gts_form() {
        let id = Uuid::new_v4();
        let gts = format!("gts.cf.core.oagw.upstream.v1~{id}");
        assert_eq!(parse_resource_id(&gts, "upstream").unwrap(), id);
    }

    #[test]
    fn parse_resource_id_rejects_garbage() {
        let err = parse_resource_id("not-an-id", "route").unwrap_err();
        assert!(matches!(err, DomainError::BadIdentifier(_)));
    }

    #[test]
    fn collector_reports_every_issue() {
        let mut c = IssueCollector::new();
        c.require(false, "a", "must be set");
        c.reject(true, "b", "must not be set");
        assert!(!c.is_empty());
        let err = c.finish().unwrap_err();
        match err {
            DomainError::Validation(issues) => {
                assert_eq!(issues.len(), 2);
                assert_eq!(issues[0].field, "a");
                assert_eq!(issues[1].field, "b");
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn collector_passes_when_clean() {
        let mut c = IssueCollector::new();
        c.require(true, "a", "must be set");
        assert!(c.finish().is_ok());
    }
}
