//! The two internal services: the Control Plane owns configuration data, and
//! the resolution helpers here are what the Data Plane calls into on the hot
//! path (ADR-0001).

pub mod endpoint;
pub mod management;
pub mod resolve;

pub use management::{ControlPlaneService, PluginInput, RouteInput, UpstreamInput};
pub use resolve::{EffectiveConfig, ResolvedUpstream};
