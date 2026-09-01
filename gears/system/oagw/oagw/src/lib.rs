//! OAGW — Outbound API Gateway gear.
//!
//! The gear is split into a **control plane** (management CRUD for upstreams,
//! routes and plugins) and a **data plane** (proxy execution), following the
//! DDD-Light layering described in `docs/DESIGN.md` §1.3:
//!
//! * [`domain`] — business logic, entities, error catalogue and repository
//!   contracts. Must never depend on [`api`] or [`infra`].
//! * [`api`] — transport layer; maps HTTP ↔ domain.
//! * [`infra`] — implements the domain repository contracts and hosts the
//!   proxy engine seam.
//!
//! Only the control-plane slices are implemented here; the data-plane modules
//! expose the extension points (`infra/proxy`, `domain/plugin`) without
//! implementing the proxy itself.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![forbid(unsafe_code)]

// === MODULE DEFINITION ===
pub mod gear;
pub mod config;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;

// === PUBLIC CONTRACTS ===
pub use config::OagwConfig;
pub use domain::alias::{
    AliasDerivation, AliasError, alias_is_valid, derive_alias, enforce_alias_update,
    enforce_alias_update_with, normalize_alias, validate_hostname,
};
pub use domain::error::{DomainError, ErrorMeta, ReferencedBy};
pub use domain::models::{
    Plugin, Route, Upstream, AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, HTTP_PROTOCOL, GRPC_PROTOCOL,
    ROUTE_TYPE, TRANSFORM_PLUGIN_TYPE, UPSTREAM_TYPE,
};
pub use domain::plugin::{
    AuthPlugin, GuardPlugin, PluginKind, PluginRegistry, TransformPlugin,
};
pub use domain::service::ControlPlaneService;
pub use gear::OagwGear;
