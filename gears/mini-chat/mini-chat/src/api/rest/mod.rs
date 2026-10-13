//! REST + SSE endpoints.

pub mod dto;
pub mod handlers;
pub mod routes;
pub mod sse;

pub use routes::register_routes;
