//! REST transport: DTO mapping, handlers and route registration.

pub mod dto;
pub mod handlers;
pub mod query;
pub mod routes;

pub use routes::register_routes;
