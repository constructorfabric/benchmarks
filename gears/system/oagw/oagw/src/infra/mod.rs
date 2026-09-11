//! Infrastructure adapters of the `oagw` gear.
//!
//! `type_provisioning` provisions the GTS identifier families through the
//! types-registry SDK client; `dependency` resolves the platform dependencies
//! the gear needs at startup; `storage` is the in-memory implementation of the
//! domain repository traits; `plugin` holds the plugin registries, the
//! execution-plan resolution and the built-in plugins; `proxy` holds the
//! data-plane limiter registry of the rate-limiting feature; `observability`
//! holds the instrument registry, the audit sink and the emission facade of the
//! observability feature.

pub mod dependency;
pub mod observability;
pub mod plugin;
pub mod proxy;
pub mod resolution;
pub mod storage;
pub mod type_provisioning;
