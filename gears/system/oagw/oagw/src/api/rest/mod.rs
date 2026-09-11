//! REST surface: the management API (control plane) and the proxy API (data plane).

pub mod dto;
pub mod error;
pub mod handlers;
pub mod proxy;
pub mod routes;

#[cfg(test)]
mod routes_tests;

pub use error::OagwError;
pub use routes::register_routes;
