//! Control-plane service: upstream configuration management (DESIGN §3.2).
//!
//! All operations are strictly scoped to the calling tenant (DESIGN §3.3
//! "Tenant Scoping"); ancestor resources are invisible to descendants. The
//! service is synchronous because the store is in-process — handlers call it
//! directly, so no lock is ever held across an `await`.
//!
//! Tenant resolution **fails closed**: a caller without a tenant is an
//! authentication failure, never a request against a hard-coded default tenant.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard};
use serde_json::json;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::storage::{PluginStore, RouteStore, UpstreamStore};
use crate::domain::types::{Plugin, PluginSpec, Route, RouteSpec, Upstream, UpstreamSpec};
use crate::error::OagwError;
use crate::tenant_context::CallerContext;

/// Resolves the tenant that owns a request from the authenticated caller.
///
/// Injected so tests (and later the data-plane slice, which walks the tenant
/// hierarchy) can supply a custom resolution strategy. A resolver **must** fail
/// closed: a caller it cannot bind to a tenant yields
/// [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed),
/// not a default tenant.
pub type TenantResolver = Arc<dyn Fn(&CallerContext) -> Result<Uuid, OagwError> + Send + Sync>;

/// `00000000-df51-5b42-9538-d2b56b7ee953` — the single tenant configured by
/// `/app/config/e2e-local.yaml`.
///
/// Kept as a named constant so tests and host deployments can *name* the
/// single-tenant principal; request handling never substitutes it for a missing
/// caller.
pub const DEFAULT_TENANT_ID: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);

/// Current Unix time in seconds.
///
/// `0` is returned when the system clock is before the epoch; the value is only
/// used for `$orderby` and audit metadata, so a clock that far off is not worth
/// failing a request over.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Control-plane service: configuration management for upstreams, routes and
/// plugins.
///
/// The stores are behind `Arc`s so the data plane can share them without
/// copying configuration: a change made through this service is visible to the
/// very next proxy request.
pub struct ControlPlaneService {
    config: OagwConfig,
    upstreams: Arc<UpstreamStore>,
    routes: Arc<RouteStore>,
    plugins: Arc<PluginStore>,
    resolve_tenant: TenantResolver,
    /// Serializes the control plane's multi-record writes (DESIGN §3.6
    /// "Multi-table updates are atomic"); see [`Self::write_section`].
    writes: Mutex<()>,
}

impl std::fmt::Debug for ControlPlaneService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlPlaneService")
            .field("config", &self.config)
            .field("upstreams", &self.upstreams.len())
            .field("routes", &self.routes.len())
            .field("plugins", &self.plugins.len())
            .finish_non_exhaustive()
    }
}

impl ControlPlaneService {
    /// Build a service with the default (fail-closed) tenant resolver.
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self::with_tenant_resolver(config, Arc::new(default_tenant_id))
    }

    /// Build a service with an injected tenant resolver.
    #[must_use]
    pub fn with_tenant_resolver(config: OagwConfig, resolve_tenant: TenantResolver) -> Self {
        Self::with_stores(
            config,
            Arc::new(UpstreamStore::new()),
            Arc::new(RouteStore::new()),
            resolve_tenant,
        )
    }

    /// Build a service over caller-supplied stores.
    ///
    /// The data plane shares these very stores, so proxy traffic sees a
    /// configuration change immediately. The plugin store is created empty here;
    /// nothing shares it yet, so it is attached with
    /// [`Self::with_plugin_store`] when a caller needs to.
    #[must_use]
    pub fn with_stores(
        config: OagwConfig,
        upstreams: Arc<UpstreamStore>,
        routes: Arc<RouteStore>,
        resolve_tenant: TenantResolver,
    ) -> Self {
        Self {
            config,
            upstreams,
            routes,
            plugins: Arc::new(PluginStore::new()),
            resolve_tenant,
            writes: Mutex::new(()),
        }
    }

    /// Replace the plugin store, returning the service.
    ///
    /// A later slice (plugin execution) shares this store with the plugin
    /// engine, the way the data plane shares the upstream and route stores.
    #[must_use]
    pub fn with_plugin_store(mut self, plugins: Arc<PluginStore>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Gear configuration.
    #[must_use]
    pub const fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// Upstream store (read-only access for the data plane).
    #[must_use]
    pub fn upstreams(&self) -> &UpstreamStore {
        &self.upstreams
    }

    /// The shared upstream store handle.
    #[must_use]
    pub const fn upstream_store(&self) -> &Arc<UpstreamStore> {
        &self.upstreams
    }

    /// The shared route store handle.
    ///
    /// Read by the data-plane route walk and written by
    /// `/oagw/v1/routes`, so a configuration change is visible to the very next
    /// proxy request.
    #[must_use]
    pub const fn route_store(&self) -> &Arc<RouteStore> {
        &self.routes
    }

    /// The shared plugin store handle.
    ///
    /// Holds the custom (tenant-defined) plugins only: built-in plugins are
    /// resolved from the in-process registry at proxy time and never persisted.
    #[must_use]
    pub const fn plugin_store(&self) -> &Arc<PluginStore> {
        &self.plugins
    }

    /// Resolve the tenant owning the current request.
    ///
    /// # Errors
    /// [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    /// when the caller has no tenant (no authenticated context, or an anonymous
    /// one) — requests are never served against a default tenant.
    pub fn tenant_of(&self, caller: &CallerContext) -> Result<Uuid, OagwError> {
        (self.resolve_tenant)(caller)
    }

    /// Hold the write section of a multi-record configuration update.
    ///
    /// Three operations touch more than one record and must not interleave
    /// (DESIGN §3.6 "Multi-table updates are atomic"):
    ///
    /// * [`Self::delete_plugin`] — the reference scan and the delete are one
    ///   decision, and the writes that can *introduce* a plugin reference take
    ///   the same section, so a bind can never land between the scan and the
    ///   delete. Without it, a bind in that window survives the delete and the
    ///   data plane resolves a plugin that no longer exists (ADR-0001 "Plugin
    ///   Deletion Behavior").
    /// * [`Self::delete_upstream`] — the routes of the upstream are cascaded away
    ///   with it (`oagw_route | FK: upstream_id (cascade)`, DESIGN §3.6).
    /// * [`Self::create_upstream`], [`Self::replace_upstream`],
    ///   [`Self::create_route`] and [`Self::replace_route`] — each may write a
    ///   plugin reference, which is what the first bullet must not miss.
    ///
    /// The section is always taken **before** any store lock and is never held
    /// across an `await` (the service is synchronous), so no store-lock ordering
    /// can invert: no code path acquires this mutex while holding one.
    fn write_section(&self) -> MutexGuard<'_, ()> {
        self.writes.lock()
    }

    /// Create an upstream from a validated body.
    ///
    /// The server generates the `id`; the alias is derived from the endpoints
    /// when omitted and must be unique per tenant.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::ValidationError`] — body fails the wire schema.
    /// * [`OagwErrorKind::AliasConflict`] — the alias is already taken.
    pub fn create_upstream(
        &self,
        caller: &CallerContext,
        body: &UpstreamSpec,
    ) -> Result<Upstream, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        // A plugin chain is written with the record, so the write belongs to the
        // section `delete_plugin` scans under.
        let _section = self.write_section();
        let spec = body.validate_for_create()?;
        let alias = spec
            .alias
            .clone()
            .ok_or_else(|| OagwError::validation("`alias` could not be resolved"))?;

        let now = unix_now();
        let record = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            created_at: now,
            updated_at: now,
            spec,
        };
        let stored = self.upstreams.insert(record)?;

        audit("upstream.created", &stored, caller, "upstream created");

        Ok((*stored).clone())
    }

    /// Fetch an upstream by id.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::UpstreamNotFound`] when the id is unknown to the tenant.
    pub fn get_upstream(&self, caller: &CallerContext, id: Uuid) -> Result<Upstream, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        self.upstreams
            .get(tenant_id, id)
            .map(|record| (*record).clone())
            .ok_or_else(|| not_found(id))
    }

    /// Fetch an upstream by alias.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::UpstreamNotFound`] when the alias is unknown to the tenant.
    pub fn get_upstream_by_alias(
        &self,
        caller: &CallerContext,
        alias: &str,
    ) -> Result<Upstream, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        self.upstreams
            .get_by_alias(tenant_id, alias)
            .map(|record| (*record).clone())
            .ok_or_else(|| {
                OagwError::upstream_not_found(format!(
                    "no upstream with alias '{alias}' for this tenant"
                ))
            })
    }

    /// List upstreams of a tenant, ordered by insertion.
    ///
    /// Records are handed out as shared handles: the list path never deep-copies
    /// configuration, so paging a small page costs nothing extra.
    ///
    /// # Errors
    /// [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    /// — the caller has no tenant.
    pub fn list_upstreams(&self, caller: &CallerContext) -> Result<Vec<Arc<Upstream>>, OagwError> {
        let tenant_id = self.tenant_of(caller)?;

        Ok(self.upstreams.list(tenant_id))
    }

    /// Replace an upstream (full replacement, DESIGN §3.3 "PUT (Replace)").
    ///
    /// `id`, `tenant_id` and `alias` are immutable; endpoints may change only
    /// when the recomputed alias is unchanged.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::UpstreamNotFound`] — unknown id.
    /// * [`OagwErrorKind::ValidationError`] — invalid body or alias change.
    pub fn replace_upstream(
        &self,
        caller: &CallerContext,
        id: Uuid,
        body: &UpstreamSpec,
    ) -> Result<Upstream, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        // The replacement may bind plugins, so it shares `delete_plugin`'s
        // section: a reference is never written while a delete is deciding.
        let _section = self.write_section();
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .ok_or_else(|| not_found(id))?;
        body.validate_alias_update(&existing.alias)?;

        let spec = body.validate()?;
        let updated = self.upstreams.replace(tenant_id, id, spec, unix_now())?;

        audit("upstream.replaced", &updated, caller, "upstream replaced");

        Ok((*updated).clone())
    }

    /// Delete an upstream, cascading its routes away with it.
    ///
    /// `oagw_route` declares `FK: upstream_id (cascade)` (DESIGN §3.6), so a
    /// route never outlives its upstream: an orphan would keep matching traffic
    /// against configuration that no longer exists and keep pinning plugins with
    /// a 409 its operator can no longer release. The upstream is deleted first —
    /// an unknown id is a 404 that touches no route — and the cascade then
    /// removes the routes the deleted upstream owned, which cannot fail, so the
    /// operation leaves no half-deleted state behind.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::UpstreamNotFound`] when the id is unknown to the tenant.
    pub fn delete_upstream(&self, caller: &CallerContext, id: Uuid) -> Result<Upstream, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        // The upstream and its routes are one update (DESIGN §3.6), and dropping
        // references can only shrink the set a concurrent `delete_plugin` sees,
        // so the section serializes the two multi-record writes.
        let _section = self.write_section();
        let deleted = self.upstreams.delete(tenant_id, id)?;

        for route in self.routes.delete_by_upstream(tenant_id, deleted.id) {
            audit_route(
                "route.deleted",
                &route,
                caller,
                "route cascaded with its upstream",
            );
        }

        audit("upstream.deleted", &deleted, caller, "upstream deleted");

        Ok((*deleted).clone())
    }

    // -----------------------------------------------------------------------
    // Routes
    // -----------------------------------------------------------------------

    /// Create a route for an upstream of the calling tenant.
    ///
    /// The server generates the `id`; the `upstream_id` must name an upstream the
    /// caller owns. Match-rule uniqueness within the upstream is enforced by the
    /// store: an intersecting method set on the same path is a
    /// [`OagwErrorKind::MatchConflict`], whether the conflicting route is enabled
    /// or not.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::ValidationError`] — body fails the wire schema.
    /// * [`OagwErrorKind::UpstreamNotFound`] — the upstream is unknown to the
    ///   tenant (an ancestor upstream is not addressable either, DESIGN §3.3).
    /// * [`OagwErrorKind::MatchConflict`] — the match rule is already taken.
    pub fn create_route(
        &self,
        caller: &CallerContext,
        body: &RouteSpec,
    ) -> Result<Route, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        // A plugin chain is written with the record, so the write belongs to the
        // section `delete_plugin` scans under.
        let _section = self.write_section();
        let spec = body.validate()?;
        let upstream = self
            .upstreams
            .get(tenant_id, spec.upstream_id)
            .ok_or_else(|| upstream_not_found(spec.upstream_id))?;

        let now = unix_now();
        let record = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id: upstream.id,
            created_at: now,
            updated_at: now,
            spec,
        };
        let stored = self.routes.insert(record)?;

        audit_route("route.created", &stored, caller, "route created");

        Ok((*stored).clone())
    }

    /// Fetch a route by id.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::RouteNotFound`] when the id is unknown to the tenant.
    pub fn get_route(&self, caller: &CallerContext, id: Uuid) -> Result<Route, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        self.routes
            .get(tenant_id, id)
            .map(|record| (*record).clone())
            .ok_or_else(|| route_not_found(id))
    }

    /// List the routes of a tenant, ordered by insertion.
    ///
    /// # Errors
    /// [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    /// — the caller has no tenant.
    pub fn list_routes(&self, caller: &CallerContext) -> Result<Vec<Arc<Route>>, OagwError> {
        let tenant_id = self.tenant_of(caller)?;

        Ok(self.routes.list(tenant_id))
    }

    /// Replace a route (full replacement, DESIGN §3.3 "PUT (Replace)").
    ///
    /// `id`, `tenant_id` and `upstream_id` are immutable: the schema still
    /// requires `upstream_id`, so it is accepted and rejected only when it
    /// differs from the stored one.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::RouteNotFound`] — unknown id.
    /// * [`OagwErrorKind::ValidationError`] — invalid body, or the replacement
    ///   moves the route to another upstream.
    /// * [`OagwErrorKind::MatchConflict`] — the match rule is already taken.
    pub fn replace_route(
        &self,
        caller: &CallerContext,
        id: Uuid,
        body: &RouteSpec,
    ) -> Result<Route, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        // The replacement may bind plugins, so it shares `delete_plugin`'s
        // section: a reference is never written while a delete is deciding.
        let _section = self.write_section();
        let existing = self
            .routes
            .get(tenant_id, id)
            .ok_or_else(|| route_not_found(id))?;
        let spec = body.validate()?;
        if spec.upstream_id != existing.upstream_id {
            return Err(immutable_field(
                "upstream_id",
                format!(
                    "`upstream_id` is an immutable field: this route belongs to upstream {}",
                    existing.upstream_id
                ),
            ));
        }

        let updated = self.routes.replace(tenant_id, id, spec, unix_now())?;

        audit_route("route.replaced", &updated, caller, "route replaced");

        Ok((*updated).clone())
    }

    /// Delete a route.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::RouteNotFound`] when the id is unknown to the tenant.
    pub fn delete_route(&self, caller: &CallerContext, id: Uuid) -> Result<Route, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        let deleted = self.routes.delete(tenant_id, id)?;

        audit_route("route.deleted", &deleted, caller, "route deleted");

        Ok((*deleted).clone())
    }

    // -----------------------------------------------------------------------
    // Plugins
    // -----------------------------------------------------------------------

    /// Create a custom (tenant-defined) plugin.
    ///
    /// The server generates the `id`, stamps the tenant and leaves
    /// `last_used_at`/`gc_eligible_at` unset until the plugin engine runs it. The
    /// name must be unique per tenant.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::ValidationError`] — body fails the plugin contract.
    /// * [`OagwErrorKind::AliasConflict`] — the name is already taken.
    pub fn create_plugin(
        &self,
        caller: &CallerContext,
        body: &PluginSpec,
    ) -> Result<Plugin, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        let spec = body.validate()?;

        let record = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: spec.plugin_type,
            name: spec.name,
            config_schema: spec.config_schema,
            source_code: spec.source_code,
            last_used_at: None,
            gc_eligible_at: None,
        };
        let stored = self.plugins.insert(record)?;

        audit_plugin("plugin.created", &stored, caller, "plugin created");

        Ok((*stored).clone())
    }

    /// Fetch a custom plugin by id.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::PluginNotFound`] when the id is unknown to the tenant.
    pub fn get_plugin(&self, caller: &CallerContext, id: Uuid) -> Result<Plugin, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        self.plugins
            .get(tenant_id, id)
            .map(|record| (*record).clone())
            .ok_or_else(|| plugin_not_found(id))
    }

    /// List the custom plugins of a tenant, ordered by insertion.
    ///
    /// # Errors
    /// [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    /// — the caller has no tenant.
    pub fn list_plugins(&self, caller: &CallerContext) -> Result<Vec<Arc<Plugin>>, OagwError> {
        let tenant_id = self.tenant_of(caller)?;

        Ok(self.plugins.list(tenant_id))
    }

    /// Delete a custom plugin.
    ///
    /// Deletion fails with [`OagwErrorKind::PluginInUse`] while any upstream or
    /// route — of **any** tenant, because a descendant may have bound the plugin
    /// — still references it (ADR-0001 "Plugin Deletion Behavior").
    ///
    /// The lookup, the reference scan and the delete are **one** write section
    /// ([`Self::write_section`]), and the writes that can bind a plugin take the
    /// same section: the decision is therefore made on the same state that is
    /// deleted, and a reference is never written into the gap between the scan
    /// and the delete.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::PluginNotFound`] when the id is unknown to the tenant.
    /// * [`OagwErrorKind::PluginInUse`] — the plugin is still referenced.
    pub fn delete_plugin(&self, caller: &CallerContext, id: Uuid) -> Result<Plugin, OagwError> {
        let tenant_id = self.tenant_of(caller)?;
        let _section = self.write_section();

        let plugin = self
            .plugins
            .get(tenant_id, id)
            .ok_or_else(|| plugin_not_found(id))?;

        let upstreams = self.upstreams.list_referencing_plugin(&plugin);
        let routes = self.routes.list_referencing_plugin(&plugin);
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(plugin_in_use(&plugin, &upstreams, &routes));
        }

        let deleted = self.plugins.delete(tenant_id, id)?;

        audit_plugin("plugin.deleted", &deleted, caller, "plugin deleted");

        Ok((*deleted).clone())
    }

    /// The Starlark source of a custom plugin.
    ///
    /// # Errors
    /// * [`OagwErrorKind::AuthenticationFailed`](crate::error::OagwErrorKind::AuthenticationFailed)
    ///   — the caller has no tenant.
    /// * [`OagwErrorKind::PluginNotFound`] when the id is unknown to the tenant.
    pub fn get_plugin_source(&self, caller: &CallerContext, id: Uuid) -> Result<String, OagwError> {
        Ok(self.get_plugin(caller, id)?.source_code)
    }
}

/// Emit the audit record of a configuration change (DESIGN §4.3).
///
/// Carries the tenant and the authenticated principal so a config change can be
/// attributed to the caller that made it.
fn audit(event: &str, record: &Upstream, caller: &CallerContext, message: &'static str) {
    tracing::info!(
        target: "oagw.audit",
        event,
        upstream_id = %record.id,
        tenant_id = %record.tenant_id,
        principal_id = %caller.subject_id(),
        alias = %record.alias,
        message
    );
}

/// [`audit`] for a route configuration change.
fn audit_route(event: &str, record: &Route, caller: &CallerContext, message: &'static str) {
    tracing::info!(
        target: "oagw.audit",
        event,
        route_id = %record.id,
        upstream_id = %record.upstream_id,
        tenant_id = %record.tenant_id,
        principal_id = %caller.subject_id(),
        message
    );
}

/// [`audit`] for a plugin configuration change.
fn audit_plugin(event: &str, record: &Plugin, caller: &CallerContext, message: &'static str) {
    tracing::info!(
        target: "oagw.audit",
        event,
        plugin_id = %record.gts_id(),
        plugin_type = %record.plugin_type,
        name = %record.name,
        tenant_id = %record.tenant_id,
        principal_id = %caller.subject_id(),
        message
    );
}

/// 400 — a replacement tries to change a field the wire schema still requires
/// but the design marks immutable (DESIGN §3.3 "Immutable fields").
fn immutable_field(field: &str, detail: String) -> OagwError {
    OagwError::validation(detail).with_extension("field", json!(field))
}

/// 409 — the plugin is still referenced (ADR-0001 "Plugin Deletion Behavior").
///
/// The payload names the plugin by its GTS id and enumerates the referencing
/// upstreams and routes by theirs, so an operator can release the plugin without
/// a second lookup. The identifiers are all the scan hands out: the referencing
/// configuration itself stays in the store, because it may belong to another
/// tenant and must never be copied into an error payload.
fn plugin_in_use(plugin: &Plugin, upstreams: &[String], routes: &[String]) -> OagwError {
    OagwError::plugin_in_use(format!(
        "Plugin is referenced by {} upstream(s) and {} route(s)",
        upstreams.len(),
        routes.len()
    ))
    .with_extension("plugin_id", json!(plugin.gts_id()))
    .with_extension(
        "referenced_by",
        json!({
            "upstreams": upstreams,
            "routes": routes,
        }),
    )
}

/// Default tenant resolution: the caller's tenant, or an authentication failure
/// when the caller has none.
///
/// There is deliberately no default-tenant fallback: an unauthenticated request
/// must never be served against the single-tenant default.
fn default_tenant_id(caller: &CallerContext) -> Result<Uuid, OagwError> {
    caller
        .tenant_id()
        .ok_or_else(OagwError::authentication_required)
}

fn not_found(id: Uuid) -> OagwError {
    OagwError::upstream_not_found(format!("no upstream with id '{id}' for this tenant"))
}

fn upstream_not_found(id: Uuid) -> OagwError {
    OagwError::upstream_not_found(format!("no upstream with id '{id}' for this tenant"))
}

fn route_not_found(id: Uuid) -> OagwError {
    OagwError::route_not_found(format!("no route with id '{id}' for this tenant"))
}

fn plugin_not_found(id: Uuid) -> OagwError {
    OagwError::plugin_not_found(format!("no plugin with id '{id}' for this tenant"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{
        Endpoint, HttpMatch, PathSuffixMode, PluginRef, PluginsConfig, RouteMatch, RouteMethod,
        Scheme, ServerConfig, SharingMode,
    };
    use crate::error::OagwErrorKind;

    fn upstream_body(host: &str, alias: Option<&str>) -> UpstreamSpec {
        UpstreamSpec {
            alias: alias.map(ToOwned::to_owned),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: host.to_owned(),
                    port: 443,
                }],
            },
            ..UpstreamSpec::default()
        }
    }

    /// An HTTP route body for `upstream_id`.
    fn route_body(upstream_id: Uuid, path: &str, methods: &[RouteMethod]) -> RouteSpec {
        RouteSpec {
            upstream_id,
            match_rules: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.to_vec(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            enabled: true,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
        }
    }

    /// A plugin body.
    fn plugin_body(name: &str, plugin_type: &str) -> PluginSpec {
        PluginSpec {
            name: name.to_owned(),
            plugin_type: plugin_type.to_owned(),
            config_schema: None,
            source_code: "def on_request(ctx):\n    pass\n".to_owned(),
        }
    }

    /// A caller authenticated for `tenant` (no subject id: the REST transport
    /// supplies it from the `SecurityContext`).
    fn caller(tenant: Uuid) -> CallerContext {
        CallerContext::new(tenant)
    }

    #[test]
    fn default_tenant_id_matches_the_e2e_single_tenant() {
        assert_eq!(
            DEFAULT_TENANT_ID.to_string(),
            "00000000-df51-5b42-9538-d2b56b7ee953"
        );
        assert_eq!(
            Uuid::parse_str("00000000-df51-5b42-9538-d2b56b7ee953").ok(),
            Some(DEFAULT_TENANT_ID)
        );
    }

    #[test]
    fn tenant_of_fails_closed_for_an_unauthenticated_caller() {
        let service = ControlPlaneService::new(OagwConfig::default());

        let err = service
            .tenant_of(&CallerContext::new(Uuid::nil()))
            .expect_err("no tenant");
        assert_eq!(err.status().as_u16(), 401);
        assert_eq!(err.kind(), OagwErrorKind::AuthenticationFailed);
        assert_eq!(err.detail(), "authentication required");
    }

    #[test]
    fn tenant_of_uses_the_caller_tenant_when_present() {
        let service = ControlPlaneService::new(OagwConfig::default());
        let caller = caller(Uuid::new_v4());

        assert_eq!(
            service.tenant_of(&caller).expect("tenant"),
            caller.tenant_id().expect("tenant")
        );
    }

    #[test]
    fn tenant_resolution_is_injectable() {
        let injected = Uuid::new_v4();
        let service = ControlPlaneService::with_tenant_resolver(
            OagwConfig::default(),
            Arc::new(move |_| Ok(injected)),
        );

        assert_eq!(
            service.tenant_of(&caller(Uuid::nil())).expect("tenant"),
            injected
        );
    }

    #[test]
    fn create_generates_the_id_and_derives_the_alias() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());

        let created = service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect("create");

        assert_ne!(created.id, Uuid::nil(), "the server generates the id");
        assert_eq!(created.tenant_id, tenant);
        assert_eq!(created.alias, "api.openai.com");
        assert!(created.is_enabled());
        assert_eq!(created.created_at, created.updated_at);
    }

    #[test]
    fn unauthenticated_callers_cannot_mutate_configuration() {
        let service = ControlPlaneService::new(OagwConfig::default());
        let anonymous = caller(Uuid::nil());

        let err = service
            .create_upstream(&anonymous, &upstream_body("api.openai.com", None))
            .expect_err("anonymous create");
        assert_eq!(err.status().as_u16(), 401);
        assert!(service.upstreams().is_empty(), "nothing was written");
    }

    #[test]
    fn create_is_tenant_scoped_and_enforces_alias_uniqueness() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());

        service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect("create");

        let err = service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect_err("duplicate alias");
        assert_eq!(err.status().as_u16(), 409);

        // The same alias is fine for another tenant.
        let other = service
            .create_upstream(
                &caller(Uuid::new_v4()),
                &upstream_body("api.openai.com", None),
            )
            .expect("another tenant");
        assert_eq!(other.alias, "api.openai.com");
    }

    #[test]
    fn create_rejects_an_invalid_body() {
        let service = ControlPlaneService::new(OagwConfig::default());
        let mut body = upstream_body("api.openai.com", None);
        body.tags = vec!["Invalid Tag".to_owned()];

        let err = service
            .create_upstream(&caller(Uuid::new_v4()), &body)
            .expect_err("invalid body");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
    }

    #[test]
    fn get_is_tenant_scoped() {
        let owner = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let created = service
            .create_upstream(&caller(owner), &upstream_body("api.openai.com", None))
            .expect("create");

        assert!(service.get_upstream(&caller(owner), created.id).is_ok());

        let stranger = Uuid::new_v4();
        let err = service
            .get_upstream(&caller(stranger), created.id)
            .expect_err("ancestor/foreign resources are invisible");
        assert_eq!(err.status().as_u16(), 404);
        assert_eq!(err.kind(), OagwErrorKind::UpstreamNotFound);
    }

    #[test]
    fn list_is_tenant_scoped_and_hands_out_shared_records() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());

        service
            .create_upstream(&caller(tenant), &upstream_body("a.openai.com", None))
            .expect("create");
        service
            .create_upstream(&caller(tenant), &upstream_body("b.openai.com", None))
            .expect("create");
        service
            .create_upstream(
                &caller(Uuid::new_v4()),
                &upstream_body("c.openai.com", None),
            )
            .expect("create");

        let listed = service.list_upstreams(&caller(tenant)).expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].alias, "a.openai.com");
        assert_eq!(listed[1].alias, "b.openai.com");
    }

    #[test]
    fn replace_is_a_full_replacement() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let created = service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect("create");

        let mut body = upstream_body("api.openai.com", Some("api.openai.com"));
        body.enabled = false;
        body.tags = vec!["maintenance".to_owned()];

        let replaced = service
            .replace_upstream(&caller(tenant), created.id, &body)
            .expect("replace");

        assert_eq!(replaced.id, created.id);
        assert_eq!(replaced.alias, created.alias);
        assert!(
            !replaced.is_enabled(),
            "omitted optional fields are cleared"
        );
        assert_eq!(replaced.spec.tags, vec!["maintenance".to_owned()]);
        assert!(replaced.updated_at >= created.created_at);
    }

    #[test]
    fn replace_rejects_an_alias_change() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let created = service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect("create");

        let body = upstream_body("api.openai.com:8443", None);
        let err = service
            .replace_upstream(&caller(tenant), created.id, &body)
            .expect_err("the derived alias would change");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn replace_of_an_unknown_id_is_404() {
        let service = ControlPlaneService::new(OagwConfig::default());

        let err = service
            .replace_upstream(
                &caller(Uuid::new_v4()),
                Uuid::new_v4(),
                &upstream_body("api.openai.com", None),
            )
            .expect_err("unknown id");
        assert_eq!(err.status().as_u16(), 404);
    }

    #[test]
    fn delete_removes_the_record() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let created = service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect("create");

        let deleted = service
            .delete_upstream(&caller(tenant), created.id)
            .expect("delete");
        assert_eq!(deleted.id, created.id);
        assert!(service.get_upstream(&caller(tenant), created.id).is_err());
        assert!(
            service
                .list_upstreams(&caller(tenant))
                .expect("list")
                .is_empty()
        );

        let err = service
            .delete_upstream(&caller(tenant), created.id)
            .expect_err("second delete is a 404");
        assert_eq!(err.status().as_u16(), 404);
    }

    #[test]
    fn alias_lookup_supports_the_data_plane() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        service
            .create_upstream(&caller(tenant), &upstream_body("api.openai.com", None))
            .expect("create");

        assert!(
            service
                .get_upstream_by_alias(&caller(tenant), "api.openai.com")
                .is_ok()
        );
        let err = service
            .get_upstream_by_alias(&caller(tenant), "missing.example.com")
            .expect_err("unknown alias");
        assert_eq!(err.status().as_u16(), 404);
    }

    #[test]
    fn config_is_exposed_for_the_data_plane() {
        let config = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let service = ControlPlaneService::new(config);

        assert!(service.config().allows_http_upstream());
        assert_eq!(service.config().proxy_timeout_secs, 30);
    }

    // -- Routes (slice S2b) -------------------------------------------------

    /// An upstream of `tenant`, so routes have something to hang off.
    fn seeded_upstream(service: &ControlPlaneService, tenant: Uuid, host: &str) -> Upstream {
        service
            .create_upstream(&caller(tenant), &upstream_body(host, None))
            .expect("create upstream")
    }

    #[test]
    fn route_create_scopes_the_tenant_and_rejects_a_foreign_upstream() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");

        let created = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("create route");

        assert_ne!(created.id, Uuid::nil(), "the server generates the id");
        assert_eq!(created.tenant_id, tenant);
        assert_eq!(created.upstream_id, upstream.id);
        assert!(created.is_enabled(), "routes default to enabled");
        assert_eq!(created.created_at, created.updated_at);

        // A foreign upstream is not addressable, so the route cannot be created.
        let stranger = Uuid::new_v4();
        let other = seeded_upstream(&service, stranger, "api.openai.com");
        let err = service
            .create_route(
                &caller(tenant),
                &route_body(other.id, "/v1", &[RouteMethod::Get]),
            )
            .expect_err("foreign upstream");
        assert_eq!(err.status().as_u16(), 404);
        assert_eq!(err.kind(), OagwErrorKind::UpstreamNotFound);
        assert!(
            service.list_routes(&caller(tenant)).expect("list").len() == 1,
            "the rejected route was not written"
        );
    }

    #[test]
    fn route_create_enforces_match_uniqueness_within_the_upstream() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");
        let body = route_body(upstream.id, "/v1", &[RouteMethod::Get, RouteMethod::Post]);

        service
            .create_route(&caller(tenant), &body)
            .expect("first route");

        // An intersecting method set on the same path conflicts.
        let err = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect_err("intersecting rule");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(err.kind(), OagwErrorKind::MatchConflict);

        // The same rule on another upstream of the tenant is a distinct rule.
        let second = seeded_upstream(&service, tenant, "eu.openai.com");
        service
            .create_route(
                &caller(tenant),
                &route_body(second.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("another upstream is a distinct rule");
    }

    #[test]
    fn route_read_and_list_are_tenant_scoped() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");
        let created = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("create route");

        assert_eq!(
            service
                .get_route(&caller(tenant), created.id)
                .expect("get")
                .id,
            created.id
        );
        assert_eq!(
            service.list_routes(&caller(tenant)).expect("list").len(),
            1,
            "the tenant sees its own route"
        );

        let stranger = Uuid::new_v4();
        let err = service
            .get_route(&caller(stranger), created.id)
            .expect_err("foreign route");
        assert_eq!(err.status().as_u16(), 404);
        assert_eq!(err.kind(), OagwErrorKind::RouteNotFound);
        assert!(
            service
                .list_routes(&caller(stranger))
                .expect("list")
                .is_empty(),
            "lists never leak another tenant's routes"
        );
    }

    #[test]
    fn route_replace_is_a_full_replacement_and_keeps_the_upstream() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");
        let created = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("create route");

        // Moving the route to another upstream is a 400 naming the field.
        let other = seeded_upstream(&service, tenant, "eu.openai.com");
        let err = service
            .replace_route(
                &caller(tenant),
                created.id,
                &route_body(other.id, "/v2", &[RouteMethod::Post]),
            )
            .expect_err("immutable upstream_id");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        assert_eq!(
            err.extensions().get("field"),
            Some(&serde_json::json!("upstream_id"))
        );

        // A same-upstream replacement succeeds and refreshes updated_at.
        let mut body = route_body(upstream.id, "/v2", &[RouteMethod::Post]);
        body.tags = vec!["chat".to_owned()];
        let replaced = service
            .replace_route(&caller(tenant), created.id, &body)
            .expect("replace");

        assert_eq!(replaced.id, created.id, "the id is preserved");
        assert_eq!(replaced.upstream_id, upstream.id, "the parent is preserved");
        assert_eq!(
            replaced.created_at, created.created_at,
            "created_at is preserved"
        );
        assert!(
            replaced.updated_at >= created.created_at,
            "updated_at is refreshed"
        );
        assert_eq!(
            replaced.spec.match_rules, body.match_rules,
            "the body is replaced"
        );
        assert_eq!(replaced.spec.tags, vec!["chat".to_owned()]);
    }

    #[test]
    fn route_delete_removes_the_record() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");
        let created = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("create route");

        let deleted = service
            .delete_route(&caller(tenant), created.id)
            .expect("delete");
        assert_eq!(deleted.id, created.id);
        assert!(service.get_route(&caller(tenant), created.id).is_err());
        assert!(
            service
                .list_routes(&caller(tenant))
                .expect("list")
                .is_empty()
        );

        let err = service
            .delete_route(&caller(tenant), created.id)
            .expect_err("second delete");
        assert_eq!(err.status().as_u16(), 404);

        // Another tenant cannot delete the route either: the id is not theirs.
        let created = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("recreate route");
        let err = service
            .delete_route(&caller(Uuid::new_v4()), created.id)
            .expect_err("foreign delete");
        assert_eq!(err.status().as_u16(), 404);
    }

    // -- Plugins (slice S2b) ------------------------------------------------

    #[test]
    fn plugin_create_scopes_the_tenant_and_enforces_name_uniqueness() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());

        let created = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");

        assert_ne!(created.id, Uuid::nil(), "the server generates the id");
        assert_eq!(created.tenant_id, tenant);
        assert_eq!(created.plugin_type, "guard_plugin");
        assert_eq!(created.last_used_at, None, "not used yet");
        assert_eq!(
            created.gc_eligible_at, None,
            "never garbage collected on create"
        );
        assert_eq!(
            created.gts_id(),
            format!("gts.cf.core.oagw.guard_plugin.v1~{}", created.id)
        );

        // The name is unique per tenant.
        let err = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect_err("duplicate name");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(err.kind(), OagwErrorKind::AliasConflict);
        assert_eq!(
            err.extensions().get("field"),
            Some(&serde_json::json!("name"))
        );

        // The same name is free for another tenant.
        let other = service
            .create_plugin(
                &caller(Uuid::new_v4()),
                &plugin_body("validator", "guard_plugin"),
            )
            .expect("another tenant");
        assert_eq!(other.name, "validator");
    }

    #[test]
    fn plugin_create_rejects_an_unknown_plugin_type() {
        let service = ControlPlaneService::new(OagwConfig::default());

        for plugin_type in ["middleware", ""] {
            let err = service
                .create_plugin(&caller(Uuid::new_v4()), &plugin_body("odd", plugin_type))
                .expect_err("unknown plugin type");
            assert_eq!(err.status().as_u16(), 400);
            assert!(
                err.detail().contains("auth_plugin"),
                "the detail names the allowed set: {}",
                err.detail()
            );
        }

        assert!(service.plugin_store().is_empty(), "nothing was written");
    }

    #[test]
    fn plugin_read_and_list_are_tenant_scoped() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let created = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");

        assert_eq!(
            service
                .get_plugin(&caller(tenant), created.id)
                .expect("get")
                .id,
            created.id
        );
        assert_eq!(
            service.list_plugins(&caller(tenant)).expect("list").len(),
            1
        );

        let stranger = Uuid::new_v4();
        let err = service
            .get_plugin(&caller(stranger), created.id)
            .expect_err("foreign plugin");
        // `PluginNotFound` maps to a 503 in the DESIGN §3.3 error table, and it
        // is never a 403: tenant isolation does not disclose existence.
        assert_eq!(err.status().as_u16(), 503);
        assert_eq!(err.kind(), OagwErrorKind::PluginNotFound);
        assert!(
            service
                .list_plugins(&caller(stranger))
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn plugin_source_is_returned_verbatim() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let created = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");

        assert_eq!(
            service
                .get_plugin_source(&caller(tenant), created.id)
                .expect("source"),
            created.source_code
        );

        let err = service
            .get_plugin_source(&caller(Uuid::new_v4()), created.id)
            .expect_err("foreign plugin");
        assert_eq!(err.status().as_u16(), 503);
    }

    #[test]
    fn plugin_delete_is_a_409_with_the_reference_list() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let plugin = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");

        // The upstream references the plugin by bare UUID, the route by GTS id:
        // both spellings must count as a reference.
        let mut upstream_spec = upstream_body("api.openai.com", None);
        upstream_spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new(plugin.id.to_string())],
        });
        service
            .replace_upstream(&caller(tenant), upstream.id, &upstream_spec)
            .expect("reference the plugin from the upstream");

        let created_route = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("create route");
        let mut route_spec = created_route.spec;
        route_spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new(plugin.gts_id())],
        });
        let route = service
            .replace_route(&caller(tenant), created_route.id, &route_spec)
            .expect("reference the plugin from the route");

        let err = service
            .delete_plugin(&caller(tenant), plugin.id)
            .expect_err("referenced plugin");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(err.kind(), OagwErrorKind::PluginInUse);
        assert_eq!(
            err.detail(),
            "Plugin is referenced by 1 upstream(s) and 1 route(s)"
        );
        assert_eq!(
            err.extensions().get("plugin_id"),
            Some(&serde_json::json!(plugin.gts_id()))
        );
        assert_eq!(
            err.extensions().get("referenced_by"),
            Some(&serde_json::json!({
                "upstreams": [format!("gts.cf.core.oagw.upstream.v1~{}", upstream.id)],
                "routes": [format!("gts.cf.core.oagw.route.v1~{}", route.id)],
            }))
        );
        assert!(
            service.get_plugin(&caller(tenant), plugin.id).is_ok(),
            "a referenced plugin is not deleted"
        );

        // Dropping the references frees the plugin.
        service
            .delete_route(&caller(tenant), route.id)
            .expect("delete route");
        service
            .delete_upstream(&caller(tenant), upstream.id)
            .expect("delete upstream");

        let deleted = service
            .delete_plugin(&caller(tenant), plugin.id)
            .expect("delete");
        assert_eq!(deleted.id, plugin.id);
        assert!(service.get_plugin(&caller(tenant), plugin.id).is_err());
    }

    #[test]
    fn deleting_an_upstream_cascades_its_routes_away() {
        let tenant = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let plugin = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");
        let upstream = seeded_upstream(&service, tenant, "api.openai.com");
        let created = service
            .create_route(
                &caller(tenant),
                &route_body(upstream.id, "/v1", &[RouteMethod::Get]),
            )
            .expect("create route");
        let mut spec = created.spec;
        spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new(plugin.id.to_string())],
        });
        service
            .replace_route(&caller(tenant), created.id, &spec)
            .expect("reference the plugin from the route");

        // The route is the plugin's only reference, so the delete is a 409 and
        // names the route.
        let err = service
            .delete_plugin(&caller(tenant), plugin.id)
            .expect_err("referenced by the route");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(
            err.extensions().get("referenced_by"),
            Some(&serde_json::json!({
                "upstreams": [],
                "routes": [format!("gts.cf.core.oagw.route.v1~{}", created.id)],
            }))
        );

        // `oagw_route | FK: upstream_id (cascade)` (DESIGN §3.6): the routes of
        // the upstream go with it, so deleting the upstream releases the plugin.
        service
            .delete_upstream(&caller(tenant), upstream.id)
            .expect("delete upstream");
        assert!(
            service.get_route(&caller(tenant), created.id).is_err(),
            "the cascaded route is gone"
        );
        assert!(
            service
                .list_routes(&caller(tenant))
                .expect("list")
                .is_empty()
        );

        // The released plugin is deletable again.
        let deleted = service
            .delete_plugin(&caller(tenant), plugin.id)
            .expect("delete the released plugin");
        assert_eq!(deleted.id, plugin.id);
    }

    #[test]
    fn plugin_delete_enumerates_references_across_tenants() {
        let owner = Uuid::new_v4();
        let service = ControlPlaneService::new(OagwConfig::default());
        let plugin = service
            .create_plugin(&caller(owner), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");

        // A *descendant* tenant inherits the plugin and references it there.
        let descendant = Uuid::new_v4();
        let upstream = seeded_upstream(&service, descendant, "api.openai.com");
        let mut spec = upstream_body("api.openai.com", None);
        spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new(plugin.id.to_string())],
        });
        service
            .replace_upstream(&caller(descendant), upstream.id, &spec)
            .expect("reference the plugin");

        let err = service
            .delete_plugin(&caller(owner), plugin.id)
            .expect_err("referenced from another tenant");
        assert_eq!(err.status().as_u16(), 409);
        assert!(
            err.detail().contains("1 upstream(s)"),
            "the scan is not tenant-scoped: {}",
            err.detail()
        );
    }

    /// An upstream body for `host` that binds `plugin_id`.
    ///
    /// Each contending binder needs its own upstream: the derived alias is
    /// unique per tenant, and the delete's 409 must name distinct records.
    fn referencing_upstream_body(host: &str, plugin_id: &str) -> UpstreamSpec {
        let mut spec = upstream_body(host, None);
        spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new(plugin_id)],
        });
        spec
    }

    /// Whether any record of `records` still binds `plugin_id`.
    fn binds_plugin(records: &[Upstream], plugin_id: Uuid) -> bool {
        records.iter().any(|record| {
            record.spec.plugins.as_ref().is_some_and(|plugins| {
                plugins
                    .items
                    .iter()
                    .any(|reference| reference.as_str() == plugin_id.to_string().as_str())
            })
        })
    }

    /// A bind never completes while a delete decides.
    ///
    /// The write section is what makes the reference scan and the delete one
    /// decision ([`Self::write_section`]): the writes that can introduce a
    /// reference take the same section, so a bind can never land in the gap
    /// between the scan and the delete. The test holds that section — the same
    /// guard the delete holds — and asserts a bind neither lands under it nor is
    /// lost once it is released.
    #[test]
    fn a_bind_waits_for_the_write_section_of_a_delete() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let tenant = Uuid::new_v4();
        let service = Arc::new(ControlPlaneService::new(OagwConfig::default()));
        let plugin = service
            .create_plugin(&caller(tenant), &plugin_body("validator", "guard_plugin"))
            .expect("create plugin");

        // `bound` is the binder's completion marker; polling it while the
        // section is held is what catches a bind that ignores the section.
        let bound = Arc::new(AtomicBool::new(false));
        let binder = {
            let bound = Arc::clone(&bound);
            let service = Arc::clone(&service);
            let plugin_id = plugin.id.to_string();
            std::thread::spawn(move || {
                let created = service.create_upstream(
                    &caller(tenant),
                    &referencing_upstream_body("api.openai.com", &plugin_id),
                );
                bound.store(true, Ordering::SeqCst);
                created
            })
        };

        // While the delete's section is held, the bind must not land: a bind
        // that did would survive the delete and leave the data plane resolving
        // a plugin that no longer exists (ADR-0001 "Plugin Deletion Behavior").
        let section = service.write_section();
        for _ in 0..64 {
            assert!(
                !bound.load(Ordering::SeqCst),
                "the bind completed while the write section was held"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(section);

        // Releasing the section lets the bind through, so no request is lost.
        let created = binder.join().expect("the binder joins").expect("the bind");
        assert_eq!(created.spec.plugins.as_ref().expect("chain").items.len(), 1);
    }

    /// Binds and a delete race; the outcome always agrees with the store.
    ///
    /// Every round hands the plugin to [`CONCURRENT_BINDS`] binders and to a
    /// deleter at the same barrier. Two outcomes are possible, and both must
    /// leave the store consistent (ADR-0001 "Plugin Deletion Behavior"):
    ///
    /// * the delete commits — the plugin is gone, and no record still binds it.
    ///   A bind that lands *after* the delete is not a reference the scan could
    ///   have seen, so its own binder releases the upstream it created (the
    ///   management API accepts a chain naming an unknown plugin; the data plane
    ///   resolves it at request time).
    /// * the delete is a 409 — the plugin survives, and the references it names
    ///   are records that are still there and still bind it.
    #[test]
    fn a_bind_and_a_delete_race_without_losing_a_reference() {
        const ROUNDS: usize = 8;
        const CONCURRENT_BINDS: usize = 4;

        let tenant = Uuid::new_v4();
        let service = Arc::new(ControlPlaneService::new(OagwConfig::default()));

        for round in 0..ROUNDS {
            let plugin = service
                .create_plugin(
                    &caller(tenant),
                    &plugin_body(&format!("contended-{round}"), "guard_plugin"),
                )
                .expect("create plugin");
            let plugin_id = plugin.id.to_string();

            // Every binder owns an upstream, so the references it introduces are
            // attributable to it and can be released again.
            let mut binders = Vec::with_capacity(CONCURRENT_BINDS);
            let barrier = Arc::new(std::sync::Barrier::new(CONCURRENT_BINDS + 1));

            for binder in 0..CONCURRENT_BINDS {
                let service = Arc::clone(&service);
                let barrier = Arc::clone(&barrier);
                binders.push({
                    let plugin_id = plugin_id.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        let host = format!("{round}-{binder}.contended.example.com");
                        let created = service
                            .create_upstream(
                                &caller(tenant),
                                &referencing_upstream_body(&host, &plugin_id),
                            )
                            .expect("the bind lands");

                        // A bind that landed once the plugin was gone is not a
                        // reference the delete could have seen, so the binder
                        // releases its own record again.
                        if service.get_plugin(&caller(tenant), plugin.id).is_err() {
                            service
                                .delete_upstream(&caller(tenant), created.id)
                                .expect("the late bind is released");
                        }

                        created
                    })
                });
            }

            let deleter = {
                let service = Arc::clone(&service);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    service.delete_plugin(&caller(tenant), plugin.id)
                })
            };

            let deleted = deleter.join().expect("the deleter joins");
            let binds = binders
                .into_iter()
                .map(|handle| handle.join().expect("the binder joins"))
                .collect::<Vec<_>>();
            assert_eq!(binds.len(), CONCURRENT_BINDS, "round {round}");

            // The delete's outcome and the store agree.
            let gone = service.get_plugin(&caller(tenant), plugin.id).is_err();
            assert_eq!(gone, deleted.is_ok(), "round {round}");

            // No record of the tenant binds a plugin that is not there.
            let upstreams = service
                .list_upstreams(&caller(tenant))
                .expect("list upstreams")
                .iter()
                .map(|record| (**record).clone())
                .collect::<Vec<_>>();
            assert_eq!(
                binds_plugin(&upstreams, plugin.id),
                !gone,
                "round {round}: a surviving record binds a plugin that is gone"
            );

            if let Err(err) = deleted {
                assert_eq!(err.status().as_u16(), 409, "round {round}");

                // A round whose delete is a 409 never deletes the plugin, so no
                // binder released its upstream: every reference the 409 names is
                // a record that is still there and still binds the plugin.
                let named = err
                    .extensions()
                    .get("referenced_by")
                    .and_then(serde_json::Value::as_object)
                    .expect("the 409 names its references");
                let ids = named
                    .values()
                    .flat_map(serde_json::Value::as_array)
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .count();
                assert!(ids > 0, "a 409 names its references: round {round}");
                assert!(
                    named
                        .get("routes")
                        .and_then(serde_json::Value::as_array)
                        .expect("routes")
                        .is_empty(),
                    "no route of the round binds a plugin: round {round}"
                );

                let live = upstreams
                    .iter()
                    .map(|record| record.gts_id())
                    .collect::<std::collections::BTreeSet<_>>();
                for id in named
                    .get("upstreams")
                    .and_then(serde_json::Value::as_array)
                    .expect("upstreams")
                {
                    assert!(
                        live.contains(id.as_str().expect("a GTS id")),
                        "{id} is a surviving upstream: round {round}"
                    );
                }
            }

            // The next round starts from an empty tenant, so its counts are its
            // own.
            for upstream in upstreams {
                service
                    .delete_upstream(&caller(tenant), upstream.id)
                    .expect("the round is torn down");
            }
        }
    }
}
