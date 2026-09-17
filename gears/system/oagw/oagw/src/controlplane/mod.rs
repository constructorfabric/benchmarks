// Created: 2026-09-04 by Constructor Tech
//! Control plane of the OAGW gear: the authority the data plane resolves
//! configurations from (`docs/ADR/0006-state-management.md`).
//!
//! * [`store`] — the in-memory, tenant-scoped source of truth;
//! * [`service`] — the synchronous service enforcing every management
//!   invariant (validation, alias uniqueness, referential integrity, plugin
//!   use, immutability, enable/disable).
//!
//! The layer owns no HTTP transport: `crate::api` maps its outcomes onto the
//! `/oagw/v1` management surface.

pub mod service;
pub mod store;

pub use service::{
    BUILT_IN_PLUGIN_CATALOG, BuiltInPlugin, ControlPlaneService, PluginDescriptor, PluginSource,
    RouteUpdate, plugin_instance_id,
};
