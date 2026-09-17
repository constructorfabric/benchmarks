//! Data-plane proxy engine: outbound HTTP client, rate limiting, CORS
//! enforcement, and the `DataPlaneService` implementation.

pub mod client;
pub mod cors;
pub mod ratelimit;
pub mod service;
