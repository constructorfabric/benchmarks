//! Control-plane error type and canonical mapping.
//!
//! Data-plane errors are emitted directly as RFC 9457 Problem responses with
//! OAGW-specific GTS identifiers (see [`crate::proxy`]); management-API
//! errors use the canonical catalog via [`OagwError`].

use toolkit_canonical_errors::{CanonicalError, resource_error};

/// Resource error builders for OAGW management operations.
#[resource_error(gts_id!("cf.core.oagw.gateway.v1~"))]
pub struct OagwError;

impl OagwError {
    /// Not-found for an OAGW management resource.
    pub fn not_found_resource(resource: impl Into<String>) -> CanonicalError {
        OagwError::not_found("resource not found")
            .with_resource(resource)
            .create()
    }

    /// Duplicate-alias / duplicate-match / plugin-in-use conflict (409).
    pub fn conflict(detail: impl Into<String>, resource: impl Into<String>) -> CanonicalError {
        OagwError::already_exists(detail)
            .with_resource(resource)
            .create()
    }

    /// Validation failure with a single field violation (400).
    pub fn field_violation(
        field: impl Into<String>,
        detail: impl Into<String>,
        reason: impl Into<String>,
    ) -> CanonicalError {
        OagwError::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create()
    }

    /// Validation failure with multiple field violations (400).
    pub fn violations(
        violations: Vec<(String, String, String)>,
    ) -> CanonicalError {
        let mut iter = violations.into_iter();
        let Some((field, detail, reason)) = iter.next() else {
            return OagwError::invalid_argument().with_constraint("validation failed").create();
        };
        let mut builder =
            OagwError::invalid_argument().with_field_violation(field, detail, reason);
        for (field, detail, reason) in iter {
            builder = builder.with_field_violation(field, detail, reason);
        }
        builder.create()
    }

    /// Bind / enforce permission failure (403).
    pub fn permission(why: impl Into<String>) -> CanonicalError {
        OagwError::permission_denied().with_reason(why).create()
    }
}

/// Convenience constructor for internal failures.
pub fn internal(detail: impl Into<String>) -> CanonicalError {
    CanonicalError::internal(detail).create()
}
