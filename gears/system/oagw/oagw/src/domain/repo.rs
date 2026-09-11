// Updated: 2026-09-01 by Constructor Tech
//! Repository traits for the OAGW Control Plane.
//!
//! The gear ships an in-memory implementation (`infra/storage/memory.rs`);
//! these traits exist so the service layer does not know that, and so a
//! durable backend can be dropped in without touching the API or the Data
//! Plane.
//!
//! Every lookup is tenant-scoped: an OAGW resource belongs to exactly one
//! tenant and is invisible to every other tenant, including ancestors and
//! descendants. The hierarchical view (walking the tenant chain, shadowing,
//! enforcement) is built *on top of* these flat lookups by the service layer.

use std::time::SystemTime;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, Upstream};
use crate::domain::error::DomainError;

/// An upstream as stored: the operator's payload plus the alias OAGW resolved
/// for it and the tenant that owns it.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRecord {
    pub tenant_id: Uuid,
    /// Always populated — the service layer derives it before storing.
    pub upstream: Upstream,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

impl UpstreamRecord {
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.upstream.id.expect("stored upstream always has an id")
    }

    #[must_use]
    pub fn alias(&self) -> &str {
        self.upstream
            .alias
            .as_deref()
            .expect("stored upstream always has an alias")
    }

    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::gts::instance_id(crate::gts::UPSTREAM_TYPE, self.id())
    }
}

/// A route as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRecord {
    pub tenant_id: Uuid,
    pub route: Route,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

impl RouteRecord {
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.route.id.expect("stored route always has an id")
    }

    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::gts::instance_id(crate::gts::ROUTE_TYPE, self.id())
    }
}

/// A custom plugin as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRecord {
    pub tenant_id: Uuid,
    pub plugin: Plugin,
    /// Set once nothing references the plugin any more; the GC collects it
    /// after the TTL has elapsed (DESIGN §Plugin Lifecycle Management).
    pub gc_eligible_at: Option<SystemTime>,
    pub created_at: SystemTime,
}

impl PluginRecord {
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.plugin.id.expect("stored plugin always has an id")
    }

    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::gts::instance_id(self.plugin.kind.base_type(), self.id())
    }
}

/// Storage for upstreams.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Store a new upstream. Fails with [`DomainError::Conflict`] when the
    /// tenant already owns an upstream with this alias.
    async fn insert(&self, record: UpstreamRecord) -> Result<(), DomainError>;

    /// Fetch one of this tenant's upstreams by id.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<UpstreamRecord>;

    /// Fetch one of this tenant's upstreams by alias.
    async fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<UpstreamRecord>;

    /// Fetch an upstream by alias from *any* tenant. Used by the Data Plane,
    /// which then checks that the caller may reach it.
    async fn get_by_alias_any_tenant(&self, alias: &str) -> Option<UpstreamRecord>;

    /// Every upstream owned by the tenant.
    async fn list(&self, tenant_id: Uuid) -> Vec<UpstreamRecord>;

    /// Replace an existing upstream in place.
    async fn update(&self, record: UpstreamRecord) -> Result<(), DomainError>;

    /// Remove an upstream.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Upstreams referencing a plugin, used for the `PluginInUse` check.
    async fn referencing_plugin(&self, plugin_id: Uuid) -> Vec<UpstreamRecord>;
}

/// Storage for routes.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    async fn insert(&self, record: RouteRecord) -> Result<(), DomainError>;

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<RouteRecord>;

    /// Every route owned by the tenant.
    async fn list(&self, tenant_id: Uuid) -> Vec<RouteRecord>;

    /// Every route pointing at one upstream, across all tenants. The Data
    /// Plane uses this to match an incoming request.
    async fn list_for_upstream(&self, upstream_id: Uuid) -> Vec<RouteRecord>;

    async fn update(&self, record: RouteRecord) -> Result<(), DomainError>;

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    async fn referencing_plugin(&self, plugin_id: Uuid) -> Vec<RouteRecord>;
}

/// Storage for custom plugins.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    async fn insert(&self, record: PluginRecord) -> Result<(), DomainError>;

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginRecord>;

    /// Fetch a plugin regardless of owner. Used when a binding references a
    /// plugin by UUID and the caller's own tenant does not hold it.
    async fn get_any(&self, id: Uuid) -> Option<PluginRecord>;

    async fn list(&self, tenant_id: Uuid) -> Vec<PluginRecord>;

    async fn update(&self, record: PluginRecord) -> Result<(), DomainError>;

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// `(owner, id)` for every stored plugin, across all tenants.
    async fn all_tenants(&self) -> Vec<(Uuid, Uuid)>;

    /// Every plugin whose GC eligibility deadline has passed.
    async fn collectible(&self, now: SystemTime) -> Vec<PluginRecord>;

    /// Clear the GC marker: the plugin became referenced again.
    async fn unmark_for_gc(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;
}
