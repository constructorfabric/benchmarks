//! Management-plane REST transport layer.
//!
//! * [`dto`] — wire DTOs and their domain projections.
//! * [`error`] — [`DomainError`] → RFC 9457 `application/problem+json`.
//! * [`params`] — the OData-ish list parameter set.
//! * [`handlers`] — one handler per operation.
//! * [`routes`] — `OperationBuilder` registration and the OpenAPI document.
pub mod dto;
pub mod error;
pub mod handlers;
pub mod params;
pub mod routes;
