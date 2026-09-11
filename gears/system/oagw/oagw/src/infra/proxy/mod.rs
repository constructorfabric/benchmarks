//! Proxy infrastructure: connector, breaker, WebSocket relay and the service
//! that ties them to the control plane.

pub mod circuit_breaker;
pub mod connector;
pub mod service;
pub mod websocket;

pub use circuit_breaker::CircuitBreakers;
pub use connector::{UpstreamConnector, UpstreamExchange, UpstreamRequest};
pub use service::{OagwDataPlane, ProxyPlan};
