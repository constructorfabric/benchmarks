//! Repository contracts.
//!
//! The Control Plane owns all persistence access through these traits; the
//! concrete implementation lives in `infra::storage`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::OagwResult;
use crate::domain::model::{Plugin, Route, Upstream};

#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// # Errors
    /// Returns a domain error when the write fails.
    async fn insert(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>>;

    /// Tenant-scoped alias lookup (`UNIQUE (tenant_id, alias)`).
    ///
    /// # Errors
    /// Returns a domain error when the read fails.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>>;

    /// # Errors
    /// Returns a domain error when the write fails.
    async fn replace(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Returns `true` when a row was removed.
    ///
    /// # Errors
    /// Returns a domain error when the write fails.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool>;

    /// Every upstream across tenants — used by the plugin "in use" scan.
    ///
    /// # Errors
    /// Returns a domain error when the read fails.
    async fn all(&self) -> OagwResult<Vec<Upstream>>;
}

#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// # Errors
    /// Returns a domain error when the write fails.
    async fn insert(&self, route: Route) -> OagwResult<Route>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>>;

    /// Routes bound to one upstream, regardless of the calling tenant — the
    /// proxy path inherits ancestor routes.
    ///
    /// # Errors
    /// Returns a domain error when the read fails.
    async fn list_by_upstream(&self, upstream_id: Uuid) -> OagwResult<Vec<Route>>;

    /// # Errors
    /// Returns a domain error when the write fails.
    async fn replace(&self, route: Route) -> OagwResult<Route>;

    /// # Errors
    /// Returns a domain error when the write fails.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool>;

    /// Cascade delete for `oagw_route.upstream_id` FK.
    ///
    /// # Errors
    /// Returns a domain error when the write fails.
    async fn delete_by_upstream(&self, upstream_id: Uuid) -> OagwResult<usize>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn all(&self) -> OagwResult<Vec<Route>>;
}

#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// # Errors
    /// Returns a domain error when the write fails.
    async fn insert(&self, plugin: Plugin) -> OagwResult<Plugin>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Plugin>>;

    /// Tenant-agnostic lookup used when resolving a UUID-backed binding at
    /// proxy time (the owning tenant may be an ancestor).
    ///
    /// # Errors
    /// Returns a domain error when the read fails.
    async fn get_any_tenant(&self, id: Uuid) -> OagwResult<Option<Plugin>>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn find_by_name(&self, tenant_id: Uuid, name: &str) -> OagwResult<Option<Plugin>>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Plugin>>;

    /// # Errors
    /// Returns a domain error when the write fails.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool>;

    /// Mark / clear GC eligibility for an unlinked plugin.
    ///
    /// # Errors
    /// Returns a domain error when the write fails.
    async fn set_gc_eligible_at(
        &self,
        id: Uuid,
        gc_eligible_at: Option<String>,
    ) -> OagwResult<()>;

    /// # Errors
    /// Returns a domain error when the read fails.
    async fn all(&self) -> OagwResult<Vec<Plugin>>;
}

/// Tenant hierarchy lookups needed for alias shadowing and effective-config
/// merges. Backed by the `tenant-resolver` gear.
#[async_trait]
pub trait TenantDirectory: Send + Sync {
    /// The tenant chain from `tenant_id` (index 0) up to the root.
    ///
    /// Implementations degrade to `[tenant_id]` when the hierarchy cannot be
    /// resolved, so a resolver outage never turns into a proxy outage for
    /// tenant-local upstreams.
    async fn chain(&self, ctx: &toolkit_security::SecurityContext, tenant_id: Uuid) -> Vec<Uuid>;
}
