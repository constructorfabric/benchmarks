// Created: 2026-09-04 by Constructor Tech
//! HTTP surface of the OAGW gear: the `/oagw/v1` management REST API.
//!
//! * [`dto`] — wire DTOs mirroring `docs/schemas/*.v1.schema.json`;
//! * [`handlers`] — the request → service → response projection;
//! * [`routes`] — the `OperationBuilder` registration (gear-relative paths).

pub mod dto;
pub mod handlers;
pub mod routes;

pub use routes::register_routes;
