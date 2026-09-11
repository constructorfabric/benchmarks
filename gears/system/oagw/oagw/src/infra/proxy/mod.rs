//! The proxy engine: transport, endpoint selection, plugin chain execution.

pub mod chain;
pub mod connector;
pub mod endpoint;
pub mod headers;
pub mod service;

pub use connector::{ResolvedEndpoint, UpstreamConnector};
pub use endpoint::{EndpointSelector, SelectionMethod};
pub use service::DataPlaneServiceImpl;
