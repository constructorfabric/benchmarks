//! Wire `subject` vocabulary for quota violations under
//! [`CanonicalError::ResourceExhausted`].
//!
//! AM emits exactly one `ResourceExhausted` subject today (the
//! hierarchy-integrity single-flight gate), so this is a single plain
//! constant with no typed sub-enum. The impl crate's single
//! `From<DomainError> for CanonicalError` ladder references it; the
//! round-trip test in [`crate::error`] pins it to its `Problem` JSON
//! path (`context.violations[].subject`).
//!
//! [`CanonicalError::ResourceExhausted`]: toolkit_canonical_errors::CanonicalError::ResourceExhausted

/// `violations[].subject` for the hierarchy-integrity check: a check is
/// already in progress and the single-flight gate is held (HTTP 429).
pub const INTEGRITY_CHECK: &str = "integrity_check";
