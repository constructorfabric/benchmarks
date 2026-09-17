//! Transport layer of the OAGW gear (DESIGN.md §1.3).
//!
//! This layer owns HTTP concerns only: route registration, request parsing,
//! response serialization and the wire documents the management API speaks.
//! It maps between those documents and the validated domain model of
//! [`crate::domain`], and renders every rejection as an RFC 9457
//! `application/problem+json` document through [`crate::error`].
//!
//! The layer never validates or stores anything itself: every rule lives in
//! the domain layer, and the only state the handlers touch is the
//! [`ManagementService`] handed to the router.
//!
//! ## Module map
//!
//! - [`rest`] — the management REST API: routes, handlers, DTOs and the
//!   control-plane service it is served from

// === REST API ===
pub mod rest;

// === RE-EXPORTS ===
pub use rest::{ManagementService, register_routes};
