//! `oagw` — the Constructor Fabric **outbound API gateway**.
//!
//! OAGW proxies requests from inside the platform to third-party upstreams
//! (OpenAI, Stripe, …), applying per-tenant configuration: alias resolution,
//! route matching, outbound authentication, guard and transform plugins.
//!
//! # Layout (DDD-Light)
//!
//! * [`api`] — transport. `api/rest/` is the management API
//!   (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`).
//! * [`domain`] — business logic: models, errors, the control-plane service
//!   and the plugin traits. Depends on nothing below it.
//! * [`infra`] — implementations: the in-memory store, the plugin registries
//!   and the built-in plugins, and the types-registry provisioning.
//! * [`gear`] — the gear registration and lifecycle.
//!
//! # Part boundaries
//!
//! This module is the **control plane** (part 1): configuration storage and
//! the management REST API. The data plane — `infra/proxy/`, the actual
//! request proxying — is added by a later part and hangs off the same
//! [`domain`] models and [`config::OagwConfig`].

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;

/// Re-export the plugin traits so data-plane code (and tests) can implement
/// them without reaching into the domain module tree.
pub mod prelude {
    pub use crate::config::OagwConfig;
    pub use crate::domain::error::{DomainError, ErrorSource, FieldViolation, PluginReferences};
    pub use crate::domain::plugin::{
        AuthPlugin, GuardDecision, GuardPlugin, PluginConfig, PluginError, PluginRegistryKind,
        PluginResult, RequestContext, ResponseContext, TransformPhase, TransformPlugin,
    };
    pub use crate::domain::services::{
        ControlPlaneStore, ListResult, ManagementService, PluginDraft, RouteDraft, UpstreamDraft,
    };
    pub use crate::infra::plugin::{
        AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
    };
}
