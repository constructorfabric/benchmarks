//! Management API: request DTOs, handlers, and route registration.

pub mod dto;
mod handlers;
mod routes;

pub use routes::register_routes;
