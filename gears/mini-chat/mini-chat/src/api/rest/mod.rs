//! REST API of the gear.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod routes;
pub mod sse;
mod upload;

pub use routes::{ApiServices, register_routes};
