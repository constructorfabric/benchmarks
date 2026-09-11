//! Data Plane: upstream connections, header transformation, endpoint
//! selection, protocol upgrades and the proxy orchestration itself.

pub mod connector;
pub mod endpoint;
pub mod headers;
pub mod service;
pub mod upgrade;

pub use connector::UpstreamConnector;
pub use endpoint::{EndpointSelector, SelectionMethod};
pub use service::DataPlaneServiceImpl;
