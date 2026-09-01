//! Control-plane domain errors for the OAGW gear.

use crate::domain::models::{DuplicateKey, ReferencedBy};
use std::fmt;

/// Errors produced by the control-plane service while manipulating upstreams,
/// routes, and plugins.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    /// The requested resource does not exist.
    #[error("{0}")]
    NotFound(ResourceRef),
    /// A uniqueness constraint was violated.
    #[error("{0}")]
    Duplicate(DuplicateKind),
    /// The resource cannot be deleted because something still references it.
    #[error("{0} references it")]
    InUse(ResourceRef, ReferencedBy),
    /// Validation failed for a create/update payload.
    #[error("validation failed: {details}")]
    Validation { details: String },
    /// The alias is immutable after the resource is created.
    #[error("alias is immutable after creation")]
    ImmutableAlias,
    /// The upstream id is immutable after the route is created.
    #[error("upstream_id is immutable after creation")]
    ImmutableUpstreamId,
}

/// Identifies a resource in an error.
#[derive(Debug, Clone)]
pub enum ResourceRef {
    Upstream(String),
    Route(String),
    Plugin(String),
}

impl fmt::Display for ResourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceRef::Upstream(id) => write!(f, "upstream '{id}'"),
            ResourceRef::Route(id) => write!(f, "route '{id}'"),
            ResourceRef::Plugin(id) => write!(f, "plugin '{id}'"),
        }
    }
}

/// Kind of uniqueness violation.
#[derive(Debug, Clone)]
pub enum DuplicateKind {
    /// `(tenant_id, alias)` already taken by another upstream.
    AliasTaken { alias: String, owner: String },
    /// A route with the same id already exists.
    RouteExists { id: String },
    /// A plugin with the same id already exists.
    PluginExists { id: String },
}

impl fmt::Display for DuplicateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DuplicateKind::AliasTaken { alias, owner } => {
                write!(f, "alias '{alias}' is already in use by upstream '{owner}'")
            }
            DuplicateKind::RouteExists { id } => write!(f, "route '{id}' already exists"),
            DuplicateKind::PluginExists { id } => write!(f, "plugin '{id}' already exists"),
        }
    }
}

/// Backing type for [`DuplicateKey`] used by in-memory stores.
impl From<DuplicateKind> for DuplicateKey {
    fn from(kind: DuplicateKind) -> Self {
        match kind {
            DuplicateKind::AliasTaken { alias, owner } => DuplicateKey::Alias(alias, owner),
            DuplicateKind::RouteExists { id } => DuplicateKey::Route(id),
            DuplicateKind::PluginExists { id } => DuplicateKey::Plugin(id),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_reader_friendly() {
        let e = ControlPlaneError::NotFound(ResourceRef::Route("r-1".into()));
        assert!(e.to_string().contains("route 'r-1'"));
        let e = ControlPlaneError::Duplicate(DuplicateKind::AliasTaken {
            alias: "api".into(),
            owner: "u-1".into(),
        });
        assert!(
            e.to_string()
                .contains("alias 'api' is already in use by upstream 'u-1'")
        );
    }
}
