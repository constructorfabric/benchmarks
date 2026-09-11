//! REST transport of the OAGW gear: route registration, handlers and wire
//! DTOs.
//!
//! `register_routes` is the single entry point used by
//! [`crate::OagwGear::register_rest`](crate::OagwGear).

mod dto;
mod handlers;
mod routes;

pub use routes::register_routes;
