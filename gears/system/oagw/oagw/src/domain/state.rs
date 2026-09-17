//! Process-wide gear state shared with the REST surface.
//!
//! [`GearState`] is constructed once during `Gear::init` and handed to every
//! REST handler through an axum `Extension` layer (flow
//! `cpt-cf-oagw-flow-gear-foundation-boot`, step `inst-gf-boot-deps`; DoD
//! `cpt-cf-oagw-dod-gear-foundation-register`).  The state is immutable after
//! initialization: all mutation happens through the repository traits behind
//! `Arc`, so handlers can share it freely across threads.

use std::sync::Arc;

use crate::domain::service::{ControlPlaneService, DataPlaneService};
use crate::infra::MetricsRegistry;

/// The OAGW gear's process-wide state: one Control Plane, one Data Plane,
/// and the shared observability registry (feature
/// `cpt-cf-oagw-feature-observability-audit`).
pub struct GearState {
    /// Control Plane boundary (`/upstreams`, `/routes`, `/plugins`).
    pub control: ControlPlaneService,
    /// Data Plane boundary (`/proxy`).
    pub data: DataPlaneService,
    /// The DESIGN §4.2 metrics registry, fed by both planes and rendered at
    /// the admin `/metrics` surface (DoD
    /// `cpt-cf-oagw-dod-observability-audit-metrics`).
    pub metrics: Arc<MetricsRegistry>,
}
