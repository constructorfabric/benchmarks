//! REST/SSE surface (DESIGN §3.3, ADR-0004).

pub mod dto;
pub mod error;
pub mod handlers;
pub mod routes;
pub mod sse;

pub use routes::register_routes;
