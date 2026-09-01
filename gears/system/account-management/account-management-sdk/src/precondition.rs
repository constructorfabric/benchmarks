//! Wire `subject` / `type` vocabulary for precondition violations under
//! [`CanonicalError::FailedPrecondition`].
//!
//! Each `*_SUBJECT` constant is the stable `subject` discriminator that
//! lands in `CanonicalError::FailedPrecondition.ctx.violations[].subject`
//! — the field consumers dispatch on. Each `*_TYPE` constant is the
//! accompanying `violations[].type` token.
//!
//! AM emits several distinct precondition families that all share the
//! `FailedPrecondition` category; the **subject** is the discriminator
//! (`tenant`, `depth`, `conversion_request`, …), mirroring the
//! `resource-group-sdk` `precondition::Subject` convention. The impl
//! crate's single `From<DomainError> for CanonicalError` ladder
//! references these constants; the round-trip tests in [`crate::error`]
//! pin each to its `Problem` JSON path.
//!
//! [`CanonicalError::FailedPrecondition`]: toolkit_canonical_errors::CanonicalError::FailedPrecondition

use core::fmt;

// ---------------------------------------------------------------------------
// `violations[].subject` discriminators.
// ---------------------------------------------------------------------------

/// Tenant-type placement rejected for the requested parent / depth.
pub const TENANT_TYPE_SUBJECT: &str = "tenant_type";

/// Hierarchy depth budget exceeded.
pub const DEPTH_SUBJECT: &str = "depth";

/// Tenant-lifecycle guard (still has children / still owns resources).
pub const TENANT_SUBJECT: &str = "tenant";

/// Generic request-state guard (tenant deleted, type immutable, …).
pub const REQUEST_SUBJECT: &str = "request";

/// Deployment-level feature-gate guard.
pub const CONFIGURATION_SUBJECT: &str = "configuration";

/// Conversion-request lifecycle guard (invalid actor / already resolved).
pub const CONVERSION_REQUEST_SUBJECT: &str = "conversion_request";

// ---------------------------------------------------------------------------
// `violations[].type` tokens.
//
// Not the dispatch discriminator (that is the subject) but a finer code
// per emission site. Extracted to consts (ADR 0005 Rule 6) so the impl
// ladder and SDK vocabulary cannot drift; pinned by the round-trip tests.
// ---------------------------------------------------------------------------

/// `tenant_type` subject — type not allowed for placement.
pub const TYPE_NOT_ALLOWED_TYPE: &str = "TYPE_NOT_ALLOWED";

/// `depth` subject — hierarchy depth exceeded.
pub const TENANT_DEPTH_EXCEEDED_TYPE: &str = "TENANT_DEPTH_EXCEEDED";

/// `tenant` subject — tenant still has child tenants.
pub const TENANT_HAS_CHILDREN_TYPE: &str = "TENANT_HAS_CHILDREN";

/// `tenant` subject — tenant still owns active resource references.
pub const TENANT_HAS_RESOURCES_TYPE: &str = "TENANT_HAS_RESOURCES";

/// `request` subject — generic precondition failure.
pub const PRECONDITION_FAILED_TYPE: &str = "PRECONDITION_FAILED";

/// `configuration` subject — feature gate disabled.
pub const FEATURE_DISABLED_TYPE: &str = "FEATURE_DISABLED";

/// `conversion_request` subject — approver/rejecter side mismatch.
pub const INVALID_ACTOR_FOR_TRANSITION_TYPE: &str = "INVALID_ACTOR_FOR_TRANSITION";

/// `conversion_request` subject — request already in a terminal state.
pub const ALREADY_RESOLVED_TYPE: &str = "ALREADY_RESOLVED";

// ---------------------------------------------------------------------------
// Typed view of the `violations[].subject` discriminators.
// ---------------------------------------------------------------------------

/// Typed view of the AM `FailedPrecondition` `subject` strings declared
/// above.
///
/// Carried by [`crate::AccountManagementError::FailedPrecondition::subject`].
/// `from_wire` returns `Self` (not `Option`) with an [`Self::Unknown`]
/// catch-all because every `subject` AM emits under `FailedPrecondition`
/// is one of the modeled values — the catch-all only fires for a future
/// subject, keeping the projection forward-compatible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// See [`TENANT_TYPE_SUBJECT`].
    TenantType,
    /// See [`DEPTH_SUBJECT`].
    Depth,
    /// See [`TENANT_SUBJECT`].
    Tenant,
    /// See [`REQUEST_SUBJECT`].
    Request,
    /// See [`CONFIGURATION_SUBJECT`].
    Configuration,
    /// See [`CONVERSION_REQUEST_SUBJECT`].
    ConversionRequest,
    /// Unmodeled / future subject — preserves the raw wire string.
    Unknown(String),
}

impl Subject {
    /// Project a wire `violations[].subject` string into the typed
    /// discriminator. Any unmodeled value is preserved in
    /// [`Self::Unknown`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            TENANT_TYPE_SUBJECT => Self::TenantType,
            DEPTH_SUBJECT => Self::Depth,
            TENANT_SUBJECT => Self::Tenant,
            REQUEST_SUBJECT => Self::Request,
            CONFIGURATION_SUBJECT => Self::Configuration,
            CONVERSION_REQUEST_SUBJECT => Self::ConversionRequest,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Render the discriminator back to its wire `subject` string.
    /// Inverse of [`Self::from_wire`] for the modeled variants.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::TenantType => TENANT_TYPE_SUBJECT,
            Self::Depth => DEPTH_SUBJECT,
            Self::Tenant => TENANT_SUBJECT,
            Self::Request => REQUEST_SUBJECT,
            Self::Configuration => CONFIGURATION_SUBJECT,
            Self::ConversionRequest => CONVERSION_REQUEST_SUBJECT,
            Self::Unknown(s) => s.as_str(),
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_round_trips_each_constant() {
        for (wire, expected) in [
            (TENANT_TYPE_SUBJECT, Subject::TenantType),
            (DEPTH_SUBJECT, Subject::Depth),
            (TENANT_SUBJECT, Subject::Tenant),
            (REQUEST_SUBJECT, Subject::Request),
            (CONFIGURATION_SUBJECT, Subject::Configuration),
            (CONVERSION_REQUEST_SUBJECT, Subject::ConversionRequest),
        ] {
            assert_eq!(Subject::from_wire(wire), expected);
            assert_eq!(expected.as_wire(), wire);
        }
    }

    #[test]
    fn subject_preserves_unknown_wire_string() {
        let raw = "future_subject";
        let s = Subject::from_wire(raw);
        assert_eq!(s, Subject::Unknown(raw.to_owned()));
        assert_eq!(s.as_wire(), raw);
    }
}
