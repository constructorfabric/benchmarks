//! Domain service boundaries — Control Plane and Data Plane.
//!
//! Distinct domain-service boundaries kept inside one deployment unit (DoD
//! `cpt-cf-oagw-dod-gear-foundation-skeleton`): [`control_plane`] serves the
//! `/upstreams`, `/routes`, `/plugins` prefix, [`data_plane`] serves the
//! `/proxy` prefix (flow `cpt-cf-oagw-flow-gear-foundation-plane-routing`).
//!
//! This package intentionally contains no business methods yet: the
//! handlers and their orchestration are delivered by the Control-Plane
//! Management API and Data-Plane Proxy features (phases p3/p5).  Both
//! boundaries carry the shared repository handles and the SDK client
//! allocation each plane needs.

pub mod control_plane;
pub mod data_plane;

pub use control_plane::ControlPlaneService;
pub use data_plane::DataPlaneService;
