//! Repository traits for the `oagw` control plane.
//!
//! The traits are object-safe and `async_trait`-based so the in-memory
//! implementation used by this build can be swapped for a durable one without
//! touching the service layer.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::dto::{PluginRef, Route, Upstream};
use crate::domain::error::{DomainError, ReferencedBy};

/// Which plugin schema type a stored plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    /// The GTS base type of the plugin kind.
    pub fn gts_type(self) -> &'static str {
        match self {
            PluginKind::Auth => crate::domain::gts_helpers::AUTH_PLUGIN_TYPE,
            PluginKind::Guard => crate::domain::gts_helpers::GUARD_PLUGIN_TYPE,
            PluginKind::Transform => crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Parses a plugin kind from a GTS base type.
    pub fn from_gts_type(t: &str) -> Option<Self> {
        match t {
            crate::domain::gts_helpers::AUTH_PLUGIN_TYPE => Some(PluginKind::Auth),
            crate::domain::gts_helpers::GUARD_PLUGIN_TYPE => Some(PluginKind::Guard),
            crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE => Some(PluginKind::Transform),
            _ => None,
        }
    }
}

/// A tenant-defined, persisted plugin.
#[derive(Debug, Clone)]
pub struct PluginRecord {
    /// Plugin id (UUID instance part).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Which plugin schema type the plugin implements.
    pub kind: PluginKind,
    /// Human-readable name, unique per tenant.
    pub name: String,
    /// Declarative plugin configuration.
    pub config: serde_json::Value,
    /// The plugin source (Starlark), returned by `GET /plugins/{id}/source`.
    pub source: String,
}

/// A stored upstream together with its owning tenant.
#[derive(Debug, Clone)]
pub struct UpstreamRecord {
    pub tenant_id: Uuid,
    pub upstream: Upstream,
}

/// A stored route together with its owning tenant.
#[derive(Debug, Clone)]
pub struct RouteRecord {
    pub tenant_id: Uuid,
    pub route: Route,
}

/// Persistence for tenant-owned upstreams.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Inserts a new upstream. Fails with [`DomainError::Conflict`] when
    /// `(tenant_id, alias)` already exists.
    async fn insert(&self, record: UpstreamRecord) -> Result<(), DomainError>;

    /// Replaces the stored upstream.
    async fn update(&self, record: UpstreamRecord) -> Result<(), DomainError>;

    /// Fetches an upstream owned by `tenant_id`.
    async fn get_by_id(&self, tenant_id: Uuid, id: &str) -> Result<Option<Upstream>, DomainError>;

    /// Fetches an upstream owned by `tenant_id` by its alias.
    async fn get_by_alias(&self, tenant_id: Uuid, alias: &str)
        -> Result<Option<Upstream>, DomainError>;

    /// Lists the upstreams owned by `tenant_id` in insertion order.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Deletes an upstream, returning `true` when a row was removed.
    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<bool, DomainError>;

    /// Finds the closest upstream with `alias` in a tenant chain ordered
    /// descendant-first.
    async fn find_in_chain(
        &self,
        chain: &[Uuid],
        alias: &str,
    ) -> Result<Option<(Uuid, Upstream)>, DomainError>;
}

/// Persistence for tenant-owned routes.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Inserts a new route. Fails with [`DomainError::Conflict`] when the
    /// match rule already exists for the upstream.
    async fn insert(&self, record: RouteRecord) -> Result<(), DomainError>;

    /// Replaces the stored route.
    async fn update(&self, record: RouteRecord) -> Result<(), DomainError>;

    /// Fetches a route owned by `tenant_id`.
    async fn get_by_id(&self, tenant_id: Uuid, id: &str) -> Result<Option<Route>, DomainError>;

    /// Lists the routes owned by `tenant_id` in insertion order.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Lists every route in a tenant chain ordered descendant-first.
    async fn list_in_chain(&self, chain: &[Uuid]) -> Result<Vec<RouteRecord>, DomainError>;

    /// Deletes a route, returning `true` when a row was removed.
    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<bool, DomainError>;
}

/// Persistence for tenant-defined plugins.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Inserts a new plugin. Fails with [`DomainError::Conflict`] when the
    /// tenant already has a plugin with the same name.
    async fn insert(&self, record: PluginRecord) -> Result<(), DomainError>;

    /// Fetches a plugin owned by `tenant_id`.
    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Option<PluginRecord>, DomainError>;

    /// Lists the plugins owned by `tenant_id`.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<PluginRecord>, DomainError>;

    /// Deletes an unreferenced plugin, returning `true` when a row was removed.
    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<bool, DomainError>;

    /// Reports the upstreams and routes that still bind `plugin_id`.
    async fn referenced_by(
        &self,
        scope: &[Uuid],
        plugin_id: &str,
    ) -> Result<ReferencedBy, DomainError>;
}

/// Scope key of a rate-limit counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateKey {
    /// Bucket identity (upstream id, route id, or ancestor-enforced marker).
    pub bucket: String,
    /// Counter scope value (tenant id, user id, client ip, or `global`).
    pub scope: String,
}

/// Result of a rate-limit attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitOutcome {
    /// The request was admitted.
    Acquired(crate::domain::error::RateLimitSnapshot),
    /// The request was refused.
    Exceeded(crate::domain::error::RateLimitSnapshot),
}

impl RateLimitOutcome {
    /// The snapshot either way.
    pub fn snapshot(&self) -> &crate::domain::error::RateLimitSnapshot {
        match self {
            RateLimitOutcome::Acquired(s) | RateLimitOutcome::Exceeded(s) => s,
        }
    }

    /// `true` when the request was admitted.
    pub fn is_acquired(&self) -> bool {
        matches!(self, RateLimitOutcome::Acquired(_))
    }
}

/// Stores the token buckets of the data plane (ADR 0003).
#[async_trait]
pub trait RateLimitStore: Send + Sync {
    /// Takes `cost` tokens from the named bucket, reporting the outcome.
    async fn try_take(
        &self,
        key: RateKey,
        capacity: u64,
        refill_rate: f64,
        cost: u64,
    ) -> Result<RateLimitOutcome, DomainError>;

    /// Drops every counter (used on configuration invalidation).
    async fn clear(&self) -> Result<(), DomainError>;
}

/// A plugin binding as it appears in a stored configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// 0-based position in the chain.
    pub position: usize,
    /// The reference as written on the wire.
    pub reference: PluginRef,
}

/// Returns the plugin references bound to a route in position order.
pub fn route_bindings(route: &Route) -> Vec<Binding> {
    route
        .plugins
        .as_ref()
        .map(|p| {
            p.items
                .iter()
                .enumerate()
                .filter_map(|(position, r)| {
                    crate::domain::dto::parse_plugin_ref(r).map(|reference| Binding { position, reference })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Returns the plugin references bound to an upstream in position order.
pub fn upstream_bindings(upstream: &Upstream) -> Vec<Binding> {
    upstream
        .plugin_refs()
        .into_iter()
        .enumerate()
        .map(|(position, reference)| Binding { position, reference })
        .collect()
}
