//! Transport layer of the `oagw` gear.
//!
//! The only module in the crate allowed to touch `axum` and `http`. Domain
//! failures arrive here as [`crate::domain::DomainError`] and leave as either
//! an RFC 9457 problem document or a preserved upstream response.

pub mod rest;
