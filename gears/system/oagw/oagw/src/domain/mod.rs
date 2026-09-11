//! Domain model of the `oagw` gear.
//!
//! Domain model of the `oagw` gear.
//!
//! The domain layer owns the gear's closed error vocabulary, the `Upstream`,
//! `Route` and `Plugin` aggregates with their value objects, the validation of
//! both, the alias contract of `cpt-cf-oagw-feature-alias-resolution`, the
//! pure rate-limit decision core of `cpt-cf-oagw-feature-rate-limiting`, the
//! pure proxy data-plane decisions of
//! `cpt-cf-oagw-feature-proxy-pipeline`, and the tenant-scoped repository
//! traits the storage layer implements, and the pure streaming decisions of
//! `cpt-cf-oagw-feature-streaming-proxy`, and the pure observability
//! vocabulary of `cpt-cf-oagw-feature-observability`.
// @cpt-begin:cpt-cf-oagw-dod-domain-types:p1:inst-full

pub mod alias;
pub mod error;
pub mod model;
pub mod observability;
pub mod plugin;
pub mod proxy;
pub mod ratelimit;
pub mod repo;
pub mod resolution;
pub mod streaming;
pub mod validation;

// @cpt-end:cpt-cf-oagw-dod-domain-types:p1:inst-full
