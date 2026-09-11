//! Domain services: the Control Plane and the configuration resolution the
//! Data Plane drives it with.

pub mod management;
pub mod resolve;

pub use management::ControlPlane;
