//! Proxy engine: outbound transport and data-plane orchestration.

pub mod service;
pub mod transport;

pub use service::{DataPlaneService, ProxyBody, ProxyOutcome};
