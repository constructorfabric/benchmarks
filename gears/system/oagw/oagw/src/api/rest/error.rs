//! Error surface for the OAGW REST layer.
//!
//! The `DomainError → CanonicalError` mapping (400/403/404/409/500) lives in
//! `crate::domain::error`; this module re-exports it so handlers can convert
//! with `?`.

pub use crate::domain::error::{DomainError, code};
