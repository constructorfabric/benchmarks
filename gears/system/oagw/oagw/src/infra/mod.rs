//! Infrastructure layer: platform-facing adapters.
//!
//! - [`type_provisioning`] — GTS catalog registration with the types
//!   registry (`cpt-cf-oagw-feature-type-provisioning`).
//! - [`proxy`] — the data-plane proxy bridge: the pingora-based
//!   [`proxy::DataPlaneService`], the [`proxy::DataPlaneGate`] that runs the
//!   auth/guard/rate-limit/PEP/CORS/SSRF pipeline, and the internal relay
//!   client that forwards ingress requests from the axum surface into the
//!   pingora listener (`cpt-cf-oagw-feature-data-plane`).

pub mod proxy;
pub mod type_provisioning;

pub use type_provisioning::{GtsRegistrationStatus, OAGW_TYPE_ID_PREFIX, register_gts_catalog};
