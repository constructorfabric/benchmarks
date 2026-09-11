//! Data Plane infrastructure: transport, header/CORS handling, resilience.

pub mod circuit;
pub mod connector;
pub mod cors;
pub mod headers;
pub mod service;

pub use circuit::CircuitBreakerRegistry;
pub use connector::UpstreamConnector;
pub use service::{DataPlaneService, ProxyRequest, problem_response};
