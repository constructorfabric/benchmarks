//! REST API layer for the OAGW management + proxy planes.
//!
//! * [`rest::dto`] — request DTOs and wire entity views.
//! * [`rest::error`] — RFC 9457 problem+json mapping (DOCS §8).
//! * [`rest::handlers`] — axum handlers for CRUD and the proxy route.
//! * [`rest::routes`] — route registration via `OperationBuilder`.

pub mod rest;
