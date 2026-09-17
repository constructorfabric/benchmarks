//! Repository contracts for the OAGW domain (feature
//! `cpt-cf-oagw-feature-domain-model-repositories`).
//!
//! The [`UpstreamRepository`], [`RouteRepository`], and [`PluginRepository`]
//! traits mirror the §3.7 relational table shapes (server-generated UUIDs,
//! `(tenant_id, alias)` / `(tenant_id, name)` uniqueness, nullable
//! `plugin_uuid` for named plugins) so a future SeaORM/`toolkit-db` swap
//! satisfies the same traits (DoD
//! `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`, flow
//! `cpt-cf-oagw-flow-domain-model-repositories-persist`,
//! `cpt-cf-oagw-flow-domain-model-repositories-effective`).
//!
//! Every operation is scoped to the calling tenant via an explicit
//! `tenant_id` bound from the `SecurityContext` propagated by
//! `tenant_resolver` (algorithm `cpt-cf-oagw-algo-domain-model-repositories-tenant-scope`).
//! Rows owned by another tenant are simply not addressable: there is no
//! cross-tenant access path.

pub mod plugin;
pub mod route;
pub mod upstream;

pub use plugin::{CatalogEntry, PluginRepository};
pub use route::{RouteRepository, http_match_of};
pub use upstream::UpstreamRepository;

/// Shared result alias for repository operations.
pub type RepoResult<T> = std::result::Result<T, crate::domain::error::DomainError>;
