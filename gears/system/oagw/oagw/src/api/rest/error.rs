//! Error rendering for the management API.
//!
//! Handlers return `Result<_, DomainError>` directly (rather than the toolkit
//! `CanonicalError`) so the RFC 9457 body carries the OAGW GTS `type`
//! identifiers from DESIGN §3.3 verbatim and always carries
//! `X-OAGW-Error-Source: gateway`.

pub use crate::domain::error::{DomainError, DomainResult, ErrorKind};

/// Success wrapper used by handlers so the error type is always
/// [`DomainError`].
pub type HandlerResult<T> = Result<T, DomainError>;
