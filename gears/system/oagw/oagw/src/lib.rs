// Created: 2026-09-08 by Constructor Tech
// @cpt-begin:cpt-cf-oagw-dod-crate-layout:p1:inst-full
//! `cf-gears-oagw` — the API-egress gateway gear.
//!
//! This slice of the gear implements the wiring and error contract:
//!
//! * the closed 22-row error mapping table and the RFC 9457 problem+json
//!   error-response contract (`cpt-cf-oagw-algo-error-mapping`,
//!   `cpt-cf-oagw-flow-error-response`);
//! * the closed `OagwConfig` surface and its validation
//!   (`cpt-cf-oagw-algo-config-load`);
//! * platform-dependency wiring and GTS type provisioning
//!   (`cpt-cf-oagw-algo-type-provisioning`);
//! * the gear registration, `/oagw/v1` route shell and lifecycle state machine
//!   (`cpt-cf-oagw-flow-gear-bootstrap`, `cpt-cf-oagw-state-gear-lifecycle`);
//! * the `Upstream`, `Route` and `Plugin` aggregates, their validation, the
//!   tenant-scoped repository traits and the in-memory stores behind them
//!   (`cpt-cf-oagw-flow-resource-validation`, `cpt-cf-oagw-flow-tenant-scoped-lookup`,
//!   `cpt-cf-oagw-algo-endpoint-validation`, `cpt-cf-oagw-algo-shape-validation`,
//!   `cpt-cf-oagw-algo-inmemory-repository`, `cpt-cf-oagw-state-resource-lifecycle`);
//! * the alias contract that turns upstream endpoints into routing keys —
//!   derivation, normalization, per-tenant uniqueness and the immutable-alias
//!   update table (`cpt-cf-oagw-flow-alias-derivation`,
//!   `cpt-cf-oagw-flow-alias-update-enforcement`, `cpt-cf-oagw-algo-alias-derivation`,
//!   `cpt-cf-oagw-algo-alias-normalization`, `cpt-cf-oagw-state-alias-binding`);
//! * the management control plane: the 15 `/oagw/v1` CRUD, list and source
//!   endpoints, their DTO and OData-subset boundaries, their conflict statuses
//!   and the `enabled` flag of the resources they store
//!   (`cpt-cf-oagw-feature-management-api`).
//!
//! The gear mounts at `/oagw/v1` and never owns a listener: it is mounted by
//! the host that declares the `rest_host` capability.

// === MODULE DEFINITION ===
pub mod gear;
pub use gear::{LifecycleState, OagwGear};

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;

// @cpt-end:cpt-cf-oagw-dod-crate-layout:p1:inst-full
