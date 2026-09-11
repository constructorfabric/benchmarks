//! `RouteManagement` — the route-management aggregate of entry 2.3 (FEATURE
//! `route-management`, flows `create`/`list`/`get`/`replace`/`delete`/
//! `enable-disable`).
//!
//! The service is the *only* writer of route records on the management
//! surface. It shares the storage, the actor type, the error type, the
//! authorization seam and the configuration-write seam with the upstream
//! aggregate of [`crate::domain::services::management`], and adds exactly one
//! port of its own: the plugin-catalog binding-time resolvability check, which
//! the write path *executes* through [`PluginBindingResolver`] and never
//! implements (its logic is owned by `cpt-cf-oagw-feature-plugin-system`).
//!
//! # Ordering of one write
//!
//! Every mutating operation ends in the same order as an upstream write
//! (`inst-rm-create-14`, `inst-rm-replace-13`, `inst-rm-del-7`): **store write
//! → Control Plane L1 invalidation → Data Plane hot-config flush → success**,
//! with the structured audit event of DESIGN §4.3 handed to the same hook.
//!
//! # The record the caller hands over
//!
//! A create and a full replacement both receive a
//! [`crate::domain::dto::Route`], with two conventions:
//!
//! * `id` left at the nil UUID means *server-generated*;
//! * `tenant_id` left at the nil UUID means *server-assigned from the actor*;
//! * on a **replacement** `upstream_id` left at the nil UUID means *not
//!   supplied, retain the stored one* — `upstream_id` is not part of the
//!   update DTO, and a replacement body that supplies it is rejected before
//!   the service is reached.
//!
//! `enabled` is a scalar field of the replacement body: a body that omits it
//! materializes the declared default `true`, so omitting it **re-enables** a
//! disabled route (`inst-rm-enab-3`). The same holds for `priority`, whose
//! declared default `0` is materialized by the transport DTO.
// @cpt-flow:cpt-cf-oagw-flow-route-management-enable-disable:p1
// @cpt-state:cpt-cf-oagw-state-route-management-route-lifecycle:p1

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::dto::Route;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    PERM_ROUTE_CREATE, PERM_ROUTE_DELETE, PERM_ROUTE_OVERRIDE, PERM_ROUTE_READ, ROUTE_BASE_TYPE,
    route_resource_id,
};
use crate::domain::list_query::ListQuery;
use crate::domain::repo::{PluginBinding, RouteRecord as StoredRoute, RouteRepository, UpstreamRepository};
use crate::domain::services::management::{
    Actor, ConfigWriteHook, ConfigWriteNotification, ManagementAuthorizer, ManagementError,
    RouteWriteKeys,
    NoopConfigWriteHook,
};

// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-1
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-2
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-3
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-4
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-5
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-6
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-7
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-8
// @cpt-begin:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-9
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-1
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-2
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-3
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-4
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-5
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-6
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-7
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-8
// @cpt-begin:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-9
/// The plugin-catalog binding-time resolvability boundary of the route write
/// path (`cpt-cf-oagw-dod-route-management-route-overrides`).
///
/// The resolvability *logic* is owned by `cpt-cf-oagw-feature-plugin-system`
/// (entry 2.6), which consults the plugin registry the foundation entry
/// delivered; the write path only reaches it through this port, so this entry
//
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-9
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-8
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-7
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-6
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-5
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-4
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-3
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-2
// @cpt-end:cpt-cf-oagw-flow-route-management-enable-disable:p1:inst-rm-enab-1
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-9
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-8
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-7
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-6
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-5
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-4
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-3
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-2
// @cpt-end:cpt-cf-oagw-state-route-management-route-lifecycle:p1:inst-rm-st-1
//
/// executes the check instead of implementing it.
pub trait PluginBindingResolver: Send + Sync {
    /// Resolve every entry of a route `plugins.items[]`, in order, to a
    /// bindable plugin row.
    ///
    /// # Errors
    ///
    /// A reference that does not resolve at binding time — a catalog-only
    /// identifier such as `cors.v1`, `timeout.v1` or `basic.v1`, an unknown
    /// one, or a custom plugin identifier the registry does not hold — is a
    /// validation rejection naming the offending entry. No interim
    /// unresolved-binding state is ever stored.
    fn resolve(&self, tenant_id: Uuid, references: &[String]) -> Result<Vec<PluginBinding>, DomainError>;
}

/// The route-management aggregate.
#[async_trait]
pub trait RouteManagement: Send + Sync {
    /// Create a route under an existing upstream of the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `400` on a shape or validation failure or on an
    /// unresolvable upstream reference, `409` on a match-rule collision.
    async fn create(&self, actor: Actor, route: Route) -> Result<Route, ManagementError>;

    /// List the routes of the calling tenant under an OData query.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `400` on an unsupported or out-of-range parameter.
    async fn list(&self, actor: Actor, query: &ListQuery) -> Result<Vec<Route>, ManagementError>;

    /// Read one route of the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `404` for a missing, foreign or removed identifier.
    async fn get(&self, actor: Actor, id: Uuid) -> Result<Route, ManagementError>;

    /// Apply the full replacement, which is also the enable and disable path.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `400` on a shape or validation failure or on a
    /// supplied `upstream_id`, `404` for a missing, foreign or removed
    /// identifier, `409` on a match-rule collision.
    async fn replace(&self, actor: Actor, id: Uuid, replacement: Route) -> Result<Route, ManagementError>;

    /// Delete a route and cascade to its child rows.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `404` for a missing, foreign or removed identifier.
    async fn delete(&self, actor: Actor, id: Uuid) -> Result<(), ManagementError>;
}

/// The route-management service over the same store the upstream aggregate
/// writes to.
pub struct RouteManagementService {
    routes: Arc<dyn RouteRepository>,
    upstreams: Arc<dyn UpstreamRepository>,
    resolver: Arc<dyn PluginBindingResolver>,
    authorizer: Arc<dyn ManagementAuthorizer>,
    hook: RwLock<Arc<dyn ConfigWriteHook>>,
}

impl RouteManagementService {
    /// Build the service over the repositories, the binding resolver and the
    /// authorization port.
    #[must_use]
    pub fn new(
        routes: Arc<dyn RouteRepository>,
        upstreams: Arc<dyn UpstreamRepository>,
        resolver: Arc<dyn PluginBindingResolver>,
        authorizer: Arc<dyn ManagementAuthorizer>,
    ) -> Self {
        Self {
            routes,
            upstreams,
            resolver,
            authorizer,
            hook: RwLock::new(Arc::new(NoopConfigWriteHook)),
        }
    }

    /// Install the post-write hook (entry 2.9).
    pub fn set_config_write_hook(&self, hook: Arc<dyn ConfigWriteHook>) {
        *self.hook.write() = hook;
    }

    /// The post-write hook, for a test to assert the ordering and the audit
    /// fields with.
    #[must_use]
    pub fn config_write_hook(&self) -> Arc<dyn ConfigWriteHook> {
        Arc::clone(&self.hook.read().clone())
    }

    /// The route repository, so a test can build a second service over the
    /// same store.
    #[must_use]
    pub fn route_repository(&self) -> Arc<dyn RouteRepository> {
        Arc::clone(&self.routes)
    }

    /// The write-ordering epilogue: the store write is already done, then the
    /// CP L1 invalidation, then the DP flush, then the audit event, then
    /// success.
    async fn notify_written(&self, notification: ConfigWriteNotification) -> Result<(), ManagementError> {
        let hook = Arc::clone(&self.hook.read().clone());
        hook.on_route_written(notification).await.map_err(ManagementError::Domain)
    }

    /// The per-operation permission gate (`inst-rm-create-13`,
    /// `inst-rm-list-8`, `inst-rm-replace-12`, `inst-rm-del-6`,
    /// `inst-rm-enab-9`), evaluated before any store access.
    // @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-13
    // `inst-rm-create-13`, `inst-rm-list-8`, `inst-rm-replace-12`,
    // `inst-rm-del-6`, `inst-rm-enab-9`: the caller's security context is
    // authorized against the route permission set
    // `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` through the
    // `authz_resolver` seam entry 2.2 established, before the service acts.
    async fn authorize(&self, actor: &Actor, permission: &str) -> Result<(), ManagementError> {
        let resource = format!("{ROUTE_BASE_TYPE}:{}", permission.rsplit(':').next().unwrap_or(""));
        self.authorizer
            .authorize(actor, permission, &resource)
            .await
            .map_err(ManagementError::Authorization)
    }
    // @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-13

    /// Validate the body, resolve the plugin bindings at binding time and
    /// apply the server-assigned fields.
    ///
    /// # Errors
    ///
    /// A shape or match failure names the offending field and stores nothing;
    /// an unresolvable plugin reference is rejected before any row is written.
    // @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-2
    // `inst-rm-create-2`/`-3`, `inst-rm-mv-1` .. `-7`, `inst-rm-mv-7b`: the
    // payload is validated against the published route schema shapes plus the
    // closed set of schema-external API fields (`priority`, `enabled`, `cors`),
    // every field outside that union having been rejected by the closed
    // request DTO, and the declared defaults are applied. The match block is
    // validated by the same routine, which derives `match_type`.
    fn prepare(
        &self,
        actor: &Actor,
        route: Route,
    ) -> Result<(Route, Vec<PluginBinding>), ManagementError> {
        let mut validated = crate::domain::validation::validate_route(&route).map_err(ManagementError::Domain)?;
        // `inst-rm-create-11b`: the server-generated identifier and the
        // server-assigned tenant.
        if validated.id.is_nil() {
            validated.id = Uuid::new_v4();
        }
        validated.tenant_id = actor.tenant_id;
        // `inst-rm-enab-3`: `enabled` and `priority` are stored as supplied;
        // the transport DTO materialized the declared defaults `true` and `0`.
        let references = validated.plugins.as_ref().map_or_else(Vec::new, |plugins| plugins.items.clone());
        let bindings = self.bindings_of(actor.tenant_id, &references)?;
        Ok((validated, bindings))
    }
    // @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-2

    /// The ordered plugin bindings of a route write, resolved through the
    /// plugin-catalog boundary.
    ///
    /// # Errors
    ///
    /// A catalog-only or otherwise unresolvable reference is rejected at
    /// binding time, so no interim unresolved-binding state is stored.
    // @cpt-dod:cpt-cf-oagw-dod-route-management-route-overrides:p1
    // `cpt-cf-oagw-dod-route-management-route-overrides`: the write path
    // invokes the plugin-catalog binding-time resolvability check through the
    // plugin registry boundary, whose logic is owned by the plugin-system
    // entry and is executed here, not implemented. The positions are the list
    // indices, contiguous from zero, and `plugin_ref` is always stored.
    fn bindings_of(&self, tenant_id: Uuid, references: &[String]) -> Result<Vec<PluginBinding>, ManagementError> {
        let bindings = self.resolver.resolve(tenant_id, references).map_err(ManagementError::Domain)?;
        if bindings.iter().enumerate().any(|(position, binding)| binding.position as usize != position) {
            return Err(ManagementError::validation(
                "plugins.items",
                "plugin binding positions must be contiguous from zero",
            ));
        }
        Ok(bindings)
    }

    /// Resolve the owning upstream within the calling tenant
    /// (`inst-rm-create-3`, `inst-rm-ur-1` .. `-6`).
    ///
    /// # Errors
    ///
    /// A `upstream_id` that resolves to nothing, to a foreign tenant or to an
    /// ancestor tenant is not-found and discloses none of the three cases.
    // @cpt-begin:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-1
    // `inst-rm-create-3` .. `-5`, `inst-rm-ur-1` .. `-6`: the `upstream_id` is
    // looked up with the caller's tenant bound to the lookup, so an
    // ancestor-tenant, foreign-tenant or missing upstream is one
    // indistinguishable not-found that returns no information about the
    // foreign record. The resolved upstream is the route's owning target, and
    // its `protocol` stays the request-time match-strategy key, which the
    // request-proxy entry owns.
    async fn resolve_upstream(&self, actor: &Actor, upstream_id: Uuid) -> Result<(), ManagementError> {
        self.upstreams
            .get(actor.tenant_id, upstream_id)
            .map(|_| ())
            .map_err(|_| ManagementError::route_not_found())
    }
    // @cpt-end:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-1

    /// Load one route of the calling tenant, or not-found.
    ///
    /// # Errors
    ///
    /// A missing, foreign-tenant, ancestor-tenant or removed identifier is
    /// not-found without disclosing which.
    // @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-3a
    // `inst-rm-list-3a`, `inst-rm-replace-2`, `inst-rm-del-2`,
    // `inst-rm-enab-2`: the record is resolved within the caller's tenant, and
    // a foreign, ancestor or removed record is indistinguishable from a
    // missing one.
    async fn stored(&self, actor: &Actor, id: Uuid) -> Result<StoredRoute, ManagementError> {
        self.routes.get(actor.tenant_id, id).map_err(ManagementError::Domain)
    }
    // @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-3a

    /// The audit notification of one accepted write (DESIGN §4.3).
    fn notification(
        &self,
        actor: &Actor,
        event: &'static str,
        status: u16,
        stored: &Route,
    ) -> ConfigWriteNotification {
        ConfigWriteNotification {
            event,
            tenant_id: actor.tenant_id,
            principal_id: actor.principal_id,
            resource_id: route_resource_id(stored.id),
            upstream_id: Some(stored.upstream_id),
            upstream_alias: None,
            route: Some(RouteWriteKeys::of(stored)),
            plugin_id: None,
            status,
            outcome: "accepted",
        }
    }
}

// @cpt-begin:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-2
// @cpt-begin:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-3
// @cpt-begin:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-4
// @cpt-begin:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-5
// @cpt-begin:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-6
#[async_trait]
impl RouteManagement for RouteManagementService {
    // @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-1
    // `inst-rm-create-1`: the create receives `POST /oagw/v1/routes` with the
    // caller's security context and the route payload; every following step is
    // the flow of the FEATURE document.
    async fn create(&self, actor: Actor, route: Route) -> Result<Route, ManagementError> {
        self.authorize(&actor, PERM_ROUTE_CREATE).await?;
        // `inst-rm-create-2` .. `-8`: the payload is validated, the match block
        // is checked and the plugin bindings are resolved before any row is
        // written.
        let (route, bindings) = self.prepare(&actor, route)?;
        // `inst-rm-create-3`: the owning upstream must exist in the caller's
        // tenant.
        self.resolve_upstream(&actor, route.upstream_id).await?;
        // `inst-rm-create-9` .. `-11`: the match-rule uniqueness check and the
        // persist are one critical section inside the repository write path, so
        // a detected collision leaves the store unchanged.
        let stored = self
            .routes
            .create(actor.tenant_id, StoredRoute { plugin_bindings: bindings, route: route.clone() })
            .map_err(|error| {
                if error.is_conflict() {
                    ManagementError::route_match_conflict()
                } else {
                    ManagementError::Domain(error)
                }
            })?
            .route;
        // `inst-rm-create-14`: store write, then CP L1 invalidation, then the
        // DP flush, then the audit event, then success.
        self.notify_written(self.notification(&actor, "route.create", 201, &stored))
            .await?;
        Ok(stored)
        // @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-1
    // `inst-rm-list-1`/`-2`: the list receives the query string and the
    // caller's tenant identifier, and the OData interpretation is the one
    // `domain::list_query` parser entry 2.2 delivered.
    async fn list(&self, actor: Actor, query: &ListQuery) -> Result<Vec<Route>, ManagementError> {
        self.authorize(&actor, PERM_ROUTE_READ).await?;
        // `inst-rm-list-2`: the query is bound to the caller's tenant before
        // any filtering is applied; `inst-rm-list-4` .. `-6`: filter, select,
        // ordering, offset, page.
        let records = self
            .routes
            .list(actor.tenant_id)
            .map_err(ManagementError::Domain)?
            .into_iter()
            .map(|record| record.route)
            .collect::<Vec<_>>();
        Ok(query.apply_routes(&records).into_iter().cloned().collect())
        // @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-3
    // `inst-rm-list-3`/`-3a`: a single route is addressed by identifier within
    // the caller's tenant.
    async fn get(&self, actor: Actor, id: Uuid) -> Result<Route, ManagementError> {
        self.authorize(&actor, PERM_ROUTE_READ).await?;
        Ok(self.stored(&actor, id).await?.route)
        // @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-3
    }

    // @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-1
    // `inst-rm-replace-1`: the replacement receives `PUT /oagw/v1/routes/{id}`
    // with the caller's tenant identifier and the replacement payload, and is
    // also the enable and disable path (`inst-rm-enab-1`).
    async fn replace(&self, actor: Actor, id: Uuid, replacement: Route) -> Result<Route, ManagementError> {
        // `inst-rm-replace-12`/`inst-rm-enab-9`: the override permission gates
        // the full replacement.
        self.authorize(&actor, PERM_ROUTE_OVERRIDE).await?;
        // `inst-rm-replace-2`: the record is resolved first, before any
        // validation runs.
        let stored = self.stored(&actor, id).await?.route;
        // `inst-rm-replace-4`/`-5`: `upstream_id` is not part of the update
        // DTO, so a replacement body that supplies it is an immutable-field
        // violation regardless of the supplied value. The transport DTO
        // rejects it; the nil convention is the only value the service accepts.
        if !replacement.upstream_id.is_nil() {
            return Err(ManagementError::validation(
                "upstream_id",
                "upstream_id is immutable and is not part of the update payload",
            ));
        }
        // `inst-rm-replace-6`/`-7`/`-7b`: the replacement match block and the
        // route-level overrides are validated, and a failure leaves the stored
        // record unchanged.
        let (mut replacement, bindings) = self.prepare(&actor, replacement)?;
        // `inst-rm-replace-3`: the immutable fields are retained from the
        // stored record.
        replacement.id = stored.id;
        replacement.tenant_id = stored.tenant_id;
        replacement.upstream_id = stored.upstream_id;
        replacement.match_type = stored.match_type;
        // `inst-rm-replace-8` .. `-9b`, `inst-rm-uniq-3b`: the uniqueness
        // re-check excludes the route under replacement, so a re-enable that
        // would collide is rejected.
        let replaced = self
            .routes
            .replace(actor.tenant_id, StoredRoute { plugin_bindings: bindings, route: replacement })
            .map_err(|error| {
                if error.is_conflict() {
                    ManagementError::route_match_conflict()
                } else {
                    ManagementError::Domain(error)
                }
            })?
            .route;
        // `inst-rm-replace-13`: store write, then CP L1 invalidation, then the
        // DP flush, then the audit event, then success.
        self.notify_written(self.notification(&actor, "route.replace", 200, &replaced))
            .await?;
        Ok(replaced)
        // @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-1
    // `inst-rm-del-1` .. `-5`: the delete removes the record together with its
    // match, method, tag and plugin-binding child rows in one atomic write.
    async fn delete(&self, actor: Actor, id: Uuid) -> Result<(), ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-6
        self.authorize(&actor, PERM_ROUTE_DELETE).await?;
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-6
        // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-2
        let stored = self.stored(&actor, id).await?.route;
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-2
        // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-3
        // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-4
        // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-4b
        self.routes.delete(actor.tenant_id, id).map_err(ManagementError::Domain)?;
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-4b
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-4
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-3
        // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-7
        // `inst-rm-del-7`: store write, then CP L1 invalidation, then the DP
        // flush, then the audit event, then success.
        self.notify_written(self.notification(&actor, "route.delete", 204, &stored))
            .await
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-7
        // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-1
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-6
// @cpt-end:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-5
// @cpt-end:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-4
// @cpt-end:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-3
// @cpt-end:cpt-cf-oagw-algo-route-management-upstream-reference:p1:inst-rm-ur-2
//

// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-10
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-11
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-12
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-13
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-2
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-3
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-4
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-5
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-6
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-7
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-7b
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-8
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-9
// @cpt-begin:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-9b
#[cfg(test)]
#[path = "route_management_tests.rs"]
mod tests;
//
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-9b
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-9
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-8
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-7b
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-7
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-6
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-5
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-4
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-3
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-2
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-13
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-12
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-11
// @cpt-end:cpt-cf-oagw-flow-route-management-replace-route:p1:inst-rm-replace-10
//
