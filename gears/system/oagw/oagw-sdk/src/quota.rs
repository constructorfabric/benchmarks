//! Wire `subject` vocabulary for quota violations under
//! [`CanonicalError::ResourceExhausted`].
//!
//! Each constant lands in
//! `CanonicalError::ResourceExhausted.ctx.violations[].subject` when
//! the OAGW data-plane rejects a request because a quota was exceeded.
//!
//! [`CanonicalError::ResourceExhausted`]: toolkit_canonical_errors::CanonicalError::ResourceExhausted

/// Per-route or per-upstream rate-limit budget exceeded. Inspect the
/// `Retry-After` HTTP header (REST callers) or the canonical
/// `retry_after_seconds` context field (in-process callers, once the
/// canonical context type grows the field) for the recommended delay.
pub const RATE_LIMIT: &str = "rate_limit";
