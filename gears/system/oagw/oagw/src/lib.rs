//! OAGW - outbound API gateway gear.
//!
//! Slice 1 lays the domain and infrastructure foundation the later slices
//! build on:
//!
//! * [`config`] - operator-facing configuration, fail-fast validated;
//! * [`domain::model`] - upstream / route / plugin wire model and GTS ids;
//! * [`domain::error`] - the typed error taxonomy rendered as
//!   `application/problem+json` (DESIGN section 3.3);
//! * [`infra::storage`] - tenant-scoped registry with the ADR-0005/0006 L1
//!   caches;
//! * [`api::rest::error_layer`] - the ADR-0007 `X-OAGW-Error-Source`
//!   middleware.
//!
//! Slice 2 adds the management plane on top of that foundation:
//!
//! * [`domain::validation`] - the [`domain::validation::Validator`] that owns
//!   every create/replace rule (scheme gating, host and port syntax, alias
//!   derivation and immutability, pool consistency, route match shape and
//!   uniqueness, plugin shape) and maps each violation to an
//!   [`domain::error::OagwError`];
//! * [`domain::odata`] - the local `$filter` / `$select` / `$orderby` /
//!   `$top` / `$skip` engine behind every list operation;
//! * [`domain::services`] - the [`domain::services::ControlPlaneService`] that
//!   sequences validation, tenancy, ancestor binding (ADR-0004) and ADR-0001
//!   audit events over the registry;
//! * [`api::rest::dto`] / [`api::rest::handlers`] / [`api::rest::routes`] - the
//!   fifteen `/oagw/v1/...` management operations.
//!
//! Slice 3 adds the plugin system and the pure data-plane policies the proxy
//! engine (slice 4) drives:
//!
//! * [`domain::plugin`] - the ADR-0002 plugin traits, the request / response /
//!   error contexts they operate on, and the deterministic
//!   [`domain::plugin::PluginChain`];
//! * [`infra::plugin`] - the five built-in plugins, the
//!   [`infra::plugin::PluginRegistry`] that constructs them from
//!   `(id, config)` and the [`infra::plugin::secret::SecretResolver`] port to
//!   the credential store;
//! * [`domain::rate_limit`] - the ADR-0003 dual-rate token bucket, the sliding
//!   window and the inheritance rules behind
//!   [`domain::rate_limit::resolve_effective_rate_limit`];
//! * [`domain::cors`] - the ADR-0004 preflight evaluation and actual-request
//!   validation.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use api::rest::error_source_layer;
pub use config::OagwConfig;
pub use domain::error::{ApiResult, OagwError};
pub use gear::OagwGear;
pub use infra::plugin::{
    PluginRegistry, SecretResolverTrait, StaticSecretResolver, UnavailableSecretResolver,
};
pub use infra::storage::{CacheLimits, RegistryStore};
