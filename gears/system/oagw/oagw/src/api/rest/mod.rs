//! Transport layer: DTOs, handlers, route registration and error rendering.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod query;
pub mod routes;

pub use routes::register_routes;
