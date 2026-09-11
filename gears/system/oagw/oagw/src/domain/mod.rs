//! Domain layer for upstream, route, and plugin management
//! (`cpt-cf-oagw-feature-upstream-management`,
//! `cpt-cf-oagw-feature-route-management`,
//! `cpt-cf-oagw-feature-plugin-management`), and for request-time alias
//! resolution, hierarchical configuration merge, and HTTP route matching
//! (`cpt-cf-oagw-feature-config-resolution`).
//!
//! Holds the `Upstream`, `Route`, and `Plugin` entity families (mirroring
//! `docs/schemas/upstream.v1.schema.json`, `docs/schemas/route.v1.schema.json`,
//! and the plugin shape from `DESIGN.md`/`ADR-0002`), the alias-derivation /
//! normalization / immutability algorithms, request validation, plugin
//! reference resolution and in-use detection, list-query parsing, the
//! tenant-scoped CRUD service that ties them all to the in-process
//! [`crate::state::ControlPlaneState`], and the [`resolve`] module that
//! resolves a proxy request's alias, merged configuration, and matching
//! route into one [`resolve::ResolvedPlan`], backed by [`resolve_cache`]'s
//! in-process resolved-configuration cache.

pub mod alias;
pub mod model;
pub mod plugin;
pub mod plugin_resolve;
pub mod plugin_validate;
pub mod proxy;
pub mod query;
pub mod rate_limit;
pub mod resolve;
pub mod resolve_cache;
pub mod route_validate;
pub mod service;
pub mod validate;

pub use model::{Plugin, Route, Upstream};
pub use resolve::{ResolvedPlan, resolve_proxy_target};
