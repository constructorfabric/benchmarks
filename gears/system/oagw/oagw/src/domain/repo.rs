//! Repository contracts.
//!
//! The Control Plane owns the configuration data and reaches it only through
//! these traits, so the storage technology is an infrastructure decision
//! (`cpt-cf-oagw-design-layers`). Every method is tenant-scoped by signature
//! — there is no way to ask a repository for "all upstreams" without naming
//! a tenant (`cpt-cf-oagw-principle-tenant-scope`).

use async_trait::async_trait;
use uuid::Uuid;

use super::error::OagwResult;
use super::model::{PluginRecord, Route, Upstream};

/// Upstream persistence.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream, failing with `409` when `(tenant_id, alias)`
    /// is taken.
    ///
    /// # Errors
    ///
    /// `409` on alias conflict.
    async fn create(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Fetch an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>>;

    /// Replace an upstream in place. The alias is not changed by this call.
    ///
    /// # Errors
    ///
    /// `404` when the upstream is not visible to `tenant_id`.
    async fn replace(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Delete an upstream and everything that cascades from it.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool>;

    /// All upstreams owned by `tenant_id`, in creation order.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>>;

    /// Look up one upstream by its routing alias within a single tenant.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>>;
}

/// Route persistence.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    ///
    /// `409` when an equivalent match rule already exists on the upstream.
    async fn create(&self, route: Route) -> OagwResult<Route>;

    /// Fetch a route owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>>;

    /// Replace a route in place.
    ///
    /// # Errors
    ///
    /// `404` when the route is not visible to `tenant_id`.
    async fn replace(&self, route: Route) -> OagwResult<Route>;

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool>;

    /// All routes owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>>;

    /// All routes attached to `upstream_id`, regardless of owner. The
    /// upstream itself is already tenant-resolved by the caller.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn list_by_upstream(&self, upstream_id: Uuid) -> OagwResult<Vec<Route>>;
}

/// Custom-plugin persistence.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin.
    ///
    /// # Errors
    ///
    /// `409` when `(tenant_id, name)` is taken.
    async fn create(&self, plugin: PluginRecord) -> OagwResult<PluginRecord>;

    /// Fetch a plugin owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<PluginRecord>>;

    /// Fetch a plugin by id without tenant scoping — the proxy path resolves
    /// bindings that may point at an ancestor's plugin.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn get_unscoped(&self, id: Uuid) -> OagwResult<Option<PluginRecord>>;

    /// Delete a plugin.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool>;

    /// All plugins owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<PluginRecord>>;

    /// Mark a plugin eligible for garbage collection at `at`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn set_gc_eligible(&self, id: Uuid, at: Option<u64>) -> OagwResult<()>;
}

/// Which upstreams and routes reference a plugin.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PluginUsage {
    /// GTS identifiers of referencing upstreams.
    pub upstreams: Vec<String>,
    /// GTS identifiers of referencing routes.
    pub routes: Vec<String>,
}

impl PluginUsage {
    /// Whether nothing references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

/// Cross-table plugin bookkeeping: "is this plugin in use?" and the GC sweep.
///
/// Separate from [`PluginRepository`] because the answer lives in the
/// *binding* tables (`oagw_upstream_plugin`, `oagw_route_plugin` and the
/// `auth_plugin_uuid` column), not in the plugin table itself.
#[async_trait]
pub trait PluginUsageRepository: Send + Sync {
    /// Everything currently referencing `plugin_ref`.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn references(&self, plugin_ref: &str) -> OagwResult<PluginUsage>;

    /// The store's monotonic clock, in the same unit as
    /// [`PluginRepository::set_gc_eligible`].
    fn clock(&self) -> u64;

    /// Delete plugin rows whose GC deadline has passed. Returns how many.
    ///
    /// # Errors
    ///
    /// Storage failures.
    async fn collect_garbage(&self, now: u64) -> OagwResult<usize>;
}
