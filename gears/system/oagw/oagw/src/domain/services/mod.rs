//! Internal services: the Control Plane owns configuration, the Data Plane
//! executes proxy requests (`ADR/0001-request-routing.md`).

pub mod management;
pub mod proxy;
pub mod tenancy;

pub use management::{ControlPlaneService, ControlPlaneServiceImpl};
pub use proxy::{DataPlaneService, ProxyRequest, ProxyResponse};
pub use tenancy::TenantHierarchy;
