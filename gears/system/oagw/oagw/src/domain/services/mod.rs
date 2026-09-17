//! Domain services of the `oagw` gear.

pub mod management;

pub use management::{ControlPlaneService, OagwService, ResolvedProxyTarget};
