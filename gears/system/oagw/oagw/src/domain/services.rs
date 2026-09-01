//! Control-plane services of the OAGW management API.
//!
//! [`ControlPlaneService`] is the single writer of the [`RegistryStore`] and
//! the single place where the three cross-cutting rules of the management API
//! live:
//!
//! * **tenancy** — every resource is owned by the calling tenant
//!   (`SecurityContext::subject_tenant_id`); a resource of another tenant is
//!   indistinguishable from a missing one (404, DESIGN section 3.3);
//! * **hierarchy** — a descendant may bind an upstream alias that an ancestor
//!   already owns, but only while the ancestor does not *enforce* any of its
//!   hierarchical sections (403 otherwise, DESIGN "Hierarchical
//!   Configuration");
//! * **audit** — every accepted mutation emits an ADR-0001 audit line with the
//!   request id, tenant, principal, resource and alias.
//!
//! Handlers stay thin: they translate the wire DTOs into the validator inputs
//! and hand the request id (`x-request-id`) through to the audit trail.

use std::sync::Arc;

use async_trait::async_trait;
use tenant_resolver_sdk::{BarrierMode, GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit;
use crate::domain::error::{OagwError, ReferencedBy};
use crate::domain::model::{
    Plugin, Route, Upstream, enforces_override, format_plugin_id, format_route_id,
    format_upstream_id,
};
use crate::domain::validation::{PluginCatalog, PluginInput, RouteInput, UpstreamInput, Validator};
use crate::infra::storage::RegistryStore;

/// Audit resource type of an upstream.
const UPSTREAM_RESOURCE: &str = "upstream";
/// Audit resource type of a route.
const ROUTE_RESOURCE: &str = "route";
/// Audit resource type of a plugin.
const PLUGIN_RESOURCE: &str = "plugin";

/// Source of the ancestor chain of a tenant (DESIGN "Hierarchical
/// Configuration").
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Ancestor tenant ids of `tenant`, nearest parent first and excluding
    /// `tenant` itself.
    ///
    /// Resolution failures degrade to "no ancestors": a control-plane write
    /// must not fail because a sibling subsystem is unavailable, and the
    /// authoritative alias-uniqueness check in the store still runs.
    async fn ancestors(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid>;
}

/// Hierarchy of a tenant that has no ancestors (root tenants, tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHierarchy;

#[async_trait]
impl TenantHierarchy for NoHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, _tenant: Uuid) -> Vec<Uuid> {
        Vec::new()
    }
}

/// Hierarchy backed by the `tenant_resolver` dependency.
pub struct ResolverHierarchy {
    client: Arc<dyn TenantResolverClient>,
}

impl ResolverHierarchy {
    /// Wraps a resolver client.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TenantHierarchy for ResolverHierarchy {
    async fn ancestors(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        let options = GetAncestorsOptions {
            barrier_mode: BarrierMode::Respect,
        };
        let response = self
            .client
            .get_ancestors(ctx, TenantId(tenant), &options)
            .await;
        response
            .map(|resolved| {
                resolved
                    .ancestors
                    .iter()
                    .map(|ancestor| ancestor.id.0)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Control-plane service: validation, tenancy, hierarchy and audit for the
/// three registry resources.
pub struct ControlPlaneService {
    store: Arc<RegistryStore>,
    validator: Validator,
    hierarchy: Arc<dyn TenantHierarchy>,
}

impl ControlPlaneService {
    /// Builds a service over `store`.
    ///
    /// `hierarchy` supplies the ancestor chain used by the bind rule; pass
    /// [`NoHierarchy`] when the gear runs without a tenant resolver.
    #[must_use]
    pub fn new(
        store: Arc<RegistryStore>,
        validator: Validator,
        hierarchy: Arc<dyn TenantHierarchy>,
    ) -> Self {
        Self {
            store,
            validator,
            hierarchy,
        }
    }

    /// The registry the service writes to.
    #[must_use]
    pub fn store(&self) -> &Arc<RegistryStore> {
        &self.store
    }

    /// The validator the service applies.
    #[must_use]
    pub fn validator(&self) -> &Validator {
        &self.validator
    }

    /// The tenant chain a data-plane request resolves over: the calling tenant
    /// first, then its ancestors nearest parent first (DESIGN "Hierarchical
    /// Configuration").
    ///
    /// Resolution failures degrade to a single-element chain, exactly like the
    /// control-plane writes: a broken tenant resolver must not take the proxy
    /// surface down.
    pub async fn tenant_chain(&self, ctx: &SecurityContext) -> Vec<Uuid> {
        let tenant = ctx.subject_tenant_id();
        let mut chain = vec![tenant];
        chain.extend(self.hierarchy.ancestors(ctx, tenant).await);
        chain
    }

    // -- upstreams ---------------------------------------------------------

    /// Creates an upstream for the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the draft is invalid (including
    /// an unresolvable plugin/auth binding), [`OagwError::BindForbidden`] when
    /// an ancestor enforces the alias, and [`OagwError::Conflict`] when the
    /// calling tenant already owns the alias.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        input: &UpstreamInput,
    ) -> Result<Arc<Upstream>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let mut upstream = self.validator.validate_upstream(input)?;
        self.validator
            .validate_bindings(&upstream, &self.plugin_catalog(tenant_id))?;
        self.check_ancestor_bind(ctx, tenant_id, &upstream.alias)
            .await?;
        upstream.id = Uuid::new_v4();
        upstream.tenant_id = tenant_id;
        let stored = self.store.insert_upstream(upstream)?;
        audit::log_mutation(
            "upstream.created",
            ctx,
            UPSTREAM_RESOURCE,
            &format_upstream_id(stored.id),
            request_id,
            Some(&stored.alias),
            None,
        );
        Ok(stored)
    }

    /// Replaces an upstream of the calling tenant. The alias and the endpoint
    /// pool derived from it are immutable (DESIGN section 3.1).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the replacement is invalid or
    /// would change the alias, [`OagwError::BindForbidden`] when an ancestor
    /// enforces the alias (checked here exactly as on the create path, so a
    /// `PUT` cannot rebind an alias an ancestor has since switched to
    /// `enforce`), and [`OagwError::NotFound`] / [`OagwError::Conflict`] per
    /// the store rules.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        upstream_id: Uuid,
        input: &UpstreamInput,
    ) -> Result<Arc<Upstream>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self
            .store
            .get_upstream(tenant_id, upstream_id)
            .ok_or_else(|| missing_upstream(upstream_id))?;
        let mut replaced = self.validator.validate_upstream_replace(&existing, input)?;
        self.validator
            .validate_bindings(&replaced, &self.plugin_catalog(tenant_id))?;
        self.check_ancestor_bind(ctx, tenant_id, &replaced.alias)
            .await?;
        replaced.id = existing.id;
        replaced.tenant_id = tenant_id;
        let stored = self.store.replace_upstream(replaced)?;
        audit::log_mutation(
            "upstream.replaced",
            ctx,
            UPSTREAM_RESOURCE,
            &format_upstream_id(stored.id),
            request_id,
            Some(&stored.alias),
            None,
        );
        Ok(stored)
    }

    /// Reads one upstream of the calling tenant.
    #[must_use]
    pub fn get_upstream(&self, ctx: &SecurityContext, upstream_id: Uuid) -> Option<Arc<Upstream>> {
        self.store
            .get_upstream(ctx.subject_tenant_id(), upstream_id)
    }

    /// Lists the upstreams of the calling tenant, newest first.
    #[must_use]
    pub fn list_upstreams(&self, ctx: &SecurityContext) -> Vec<Arc<Upstream>> {
        self.store.list_upstreams(&[ctx.subject_tenant_id()])
    }

    /// Deletes an upstream of the calling tenant together with its routes.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::NotFound`] when the calling tenant does not own
    /// the upstream.
    pub fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        upstream_id: Uuid,
    ) -> Result<Arc<Upstream>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let removed = self
            .store
            .get_upstream(tenant_id, upstream_id)
            .ok_or_else(|| missing_upstream(upstream_id))?;
        if !self.store.delete_upstream(tenant_id, upstream_id) {
            return Err(missing_upstream(upstream_id));
        }
        audit::log_mutation(
            "upstream.deleted",
            ctx,
            UPSTREAM_RESOURCE,
            &format_upstream_id(upstream_id),
            request_id,
            Some(&removed.alias),
            None,
        );
        Ok(removed)
    }

    // -- routes ------------------------------------------------------------

    /// Creates a route under an upstream of the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the draft is invalid (including
    /// an unresolvable plugin binding) or the match protocol differs from the
    /// upstream protocol, [`OagwError::NotFound`] when the upstream is not
    /// owned by the calling tenant, and [`OagwError::Conflict`] on a duplicate
    /// match rule.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        upstream_id: Uuid,
        input: &RouteInput,
    ) -> Result<Arc<Route>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let owner = self
            .store
            .get_upstream(tenant_id, upstream_id)
            .ok_or_else(|| missing_upstream(upstream_id))?;
        let mut route = self.validator.validate_route(upstream_id, &owner, input)?;
        self.validator
            .validate_route_bindings(&route, &self.plugin_catalog(tenant_id))?;
        route.id = Uuid::new_v4();
        route.tenant_id = tenant_id;
        let stored = self.store.insert_route(route)?;
        audit::log_mutation(
            "route.created",
            ctx,
            ROUTE_RESOURCE,
            &format_route_id(stored.id),
            request_id,
            None,
            Some(&format!("upstream {}", format_upstream_id(upstream_id))),
        );
        Ok(stored)
    }

    /// Replaces a route of the calling tenant. `upstream_id` is immutable, so
    /// the stored owner is kept (DESIGN section 3.3 "PUT (Replace)").
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the replacement is invalid,
    /// [`OagwError::NotFound`] when the route is unknown to the tenant, and
    /// [`OagwError::Conflict`] on a duplicate match rule.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        route_id: Uuid,
        input: &RouteInput,
    ) -> Result<Arc<Route>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self
            .store
            .get_route(tenant_id, route_id)
            .ok_or_else(|| missing_route(route_id))?;
        let owner = self
            .store
            .get_upstream(tenant_id, existing.upstream_id)
            .ok_or_else(|| missing_upstream(existing.upstream_id))?;
        let mut replaced = self
            .validator
            .validate_route(existing.upstream_id, &owner, input)?;
        self.validator
            .validate_route_bindings(&replaced, &self.plugin_catalog(tenant_id))?;
        replaced.id = existing.id;
        replaced.upstream_id = existing.upstream_id;
        replaced.tenant_id = tenant_id;
        let stored = self.store.replace_route(replaced)?;
        audit::log_mutation(
            "route.replaced",
            ctx,
            ROUTE_RESOURCE,
            &format_route_id(stored.id),
            request_id,
            None,
            Some(&format!(
                "upstream {}",
                format_upstream_id(stored.upstream_id)
            )),
        );
        Ok(stored)
    }

    /// Reads one route of the calling tenant.
    #[must_use]
    pub fn get_route(&self, ctx: &SecurityContext, route_id: Uuid) -> Option<Arc<Route>> {
        self.store.get_route(ctx.subject_tenant_id(), route_id)
    }

    /// Lists the routes of the calling tenant, highest priority first.
    #[must_use]
    pub fn list_routes(&self, ctx: &SecurityContext) -> Vec<Arc<Route>> {
        self.store.list_routes(&[ctx.subject_tenant_id()])
    }

    /// Deletes a route of the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::NotFound`] when the calling tenant does not own
    /// the route.
    pub fn delete_route(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        route_id: Uuid,
    ) -> Result<Arc<Route>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let removed = self
            .store
            .get_route(tenant_id, route_id)
            .ok_or_else(|| missing_route(route_id))?;
        if !self.store.delete_route(tenant_id, route_id) {
            return Err(missing_route(route_id));
        }
        audit::log_mutation(
            "route.deleted",
            ctx,
            ROUTE_RESOURCE,
            &format_route_id(route_id),
            request_id,
            None,
            Some(&format!(
                "upstream {}",
                format_upstream_id(removed.upstream_id)
            )),
        );
        Ok(removed)
    }

    // -- plugins -----------------------------------------------------------

    /// Registers a plugin for the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `plugin_type` is not a GTS type
    /// identifier or `config` is not a JSON object, and
    /// [`OagwError::Conflict`] when the id already exists.
    pub fn create_plugin(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        input: &PluginInput,
    ) -> Result<Arc<Plugin>, OagwError> {
        let mut plugin = self.validator.validate_plugin(input)?;
        plugin.id = Uuid::new_v4();
        plugin.tenant_id = ctx.subject_tenant_id();
        let stored = self.store.insert_plugin(plugin)?;
        audit::log_mutation(
            "plugin.created",
            ctx,
            PLUGIN_RESOURCE,
            &format_plugin_id(stored.id),
            request_id,
            None,
            Some(&stored.plugin_type),
        );
        Ok(stored)
    }

    /// Reads one plugin of the calling tenant.
    #[must_use]
    pub fn get_plugin(&self, ctx: &SecurityContext, plugin_id: Uuid) -> Option<Arc<Plugin>> {
        self.store.get_plugin(ctx.subject_tenant_id(), plugin_id)
    }

    /// Lists the plugins of the calling tenant, sorted by id.
    #[must_use]
    pub fn list_plugins(&self, ctx: &SecurityContext) -> Vec<Arc<Plugin>> {
        self.store.list_plugins(&[ctx.subject_tenant_id()])
    }

    /// Deletes a plugin of the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginInUse`] carrying `plugin_id` and
    /// `referenced_by` when an upstream or a route still references the plugin
    /// (ADR-0001), and [`OagwError::NotFound`] when the calling tenant does
    /// not own it.
    ///
    /// The *blocking* scan is tenant-wide, so a binding held by another tenant
    /// still refuses the delete. The `referenced_by` body only names resources
    /// of the calling tenant: management responses never disclose another
    /// tenant's resource ids (DESIGN section 3.3 "tenancy").
    pub fn delete_plugin(
        &self,
        ctx: &SecurityContext,
        request_id: Option<&str>,
        plugin_id: Uuid,
    ) -> Result<Arc<Plugin>, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        let removed = self
            .store
            .get_plugin(tenant_id, plugin_id)
            .ok_or_else(|| missing_plugin(plugin_id))?;
        let upstreams = self.store.upstream_ids_referencing_plugin(&removed);
        let routes = self.store.route_ids_referencing_plugin(&removed);
        if !upstreams.is_empty() || !routes.is_empty() {
            // The caller's own resources, for the wire body. The management
            // surface is strictly own-tenant scoped (DESIGN section 3.3: an
            // ancestor's resources are invisible to a descendant), so the
            // chain here is the calling tenant alone.
            let own_upstreams = self
                .store
                .upstream_ids_referencing_plugin_in(&removed, &[tenant_id]);
            let own_routes = self
                .store
                .route_ids_referencing_plugin_in(&removed, &[tenant_id]);
            return Err(OagwError::plugin_in_use(format!(
                "plugin {} is still referenced by {} upstream(s) and {} route(s); unbind them first",
                format_plugin_id(plugin_id),
                upstreams.len(),
                routes.len()
            ))
            .with_plugin_id(format_plugin_id(plugin_id))
            .with_referenced_by(ReferencedBy {
                upstreams: own_upstreams
                    .iter()
                    .map(|id| format_upstream_id(*id))
                    .collect(),
                routes: own_routes.iter().map(|id| format_route_id(*id)).collect(),
            }));
        }
        if self.store.delete_plugin(tenant_id, plugin_id).is_none() {
            return Err(missing_plugin(plugin_id));
        }
        audit::log_mutation(
            "plugin.deleted",
            ctx,
            PLUGIN_RESOURCE,
            &format_plugin_id(plugin_id),
            request_id,
            None,
            Some(&removed.plugin_type),
        );
        Ok(removed)
    }

    /// Renders the deterministic Starlark definition of a plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::NotFound`] when the calling tenant does not own
    /// the plugin.
    pub fn plugin_source(
        &self,
        ctx: &SecurityContext,
        plugin_id: Uuid,
    ) -> Result<(Arc<Plugin>, String), OagwError> {
        let plugin = self
            .get_plugin(ctx, plugin_id)
            .ok_or_else(|| missing_plugin(plugin_id))?;
        let source = crate::domain::validation::render_plugin_source(
            &format_plugin_id(plugin.id),
            &plugin.plugin_type,
            &plugin.tenant_id,
            plugin.enabled,
            &plugin.tags,
            &plugin.config,
        );
        Ok((plugin, source))
    }

    // -- hierarchy ---------------------------------------------------------

    /// Rejects an alias an ancestor already owns while that ancestor enforces
    /// its hierarchical sections.
    async fn check_ancestor_bind(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<(), OagwError> {
        let mut chain = self.hierarchy.ancestors(ctx, tenant_id).await;
        chain.push(tenant_id);
        let Some(owner) = self.store.resolve_upstream_alias(&chain, alias) else {
            return Ok(());
        };
        if owner.tenant_id == tenant_id {
            // The store surfaces a same-tenant clash as a 409 conflict.
            return Ok(());
        }
        if enforces_override(&owner) {
            return Err(OagwError::bind_forbidden(format!(
                "alias `{alias}` is owned by ancestor {} and its configuration is enforced; a descendant cannot override it",
                format_upstream_id(owner.id)
            ))
            .with_alias(alias)
            .with_upstream_id(owner.id));
        }
        Ok(())
    }

    /// The plugin ids the calling tenant may bind: the builtin ids the plugin
    /// registry implements, plus the custom plugins the tenant owns.
    ///
    /// Scoped to the calling tenant on purpose (see
    /// [`Validator::validate_route_bindings`](crate::domain::validation::Validator::validate_route_bindings)):
    /// ancestor resolution is asynchronous and not plumbed into the validator,
    /// so the bind check never widens to an ancestor's plugins.
    fn plugin_catalog(&self, tenant_id: Uuid) -> PluginCatalog {
        PluginCatalog::of_plugins(&self.store.list_plugins(&[tenant_id]))
    }
}

fn missing_upstream(upstream_id: Uuid) -> OagwError {
    OagwError::not_found(format!(
        "upstream {} does not exist",
        format_upstream_id(upstream_id)
    ))
    .with_upstream_id(upstream_id)
}

fn missing_route(route_id: Uuid) -> OagwError {
    OagwError::not_found(format!(
        "route {} does not exist",
        format_route_id(route_id)
    ))
}

fn missing_plugin(plugin_id: Uuid) -> OagwError {
    OagwError::not_found(format!(
        "plugin {} does not exist",
        format_plugin_id(plugin_id)
    ))
    .with_plugin_id(format_plugin_id(plugin_id))
}

#[cfg(test)]
#[path = "services_tests.rs"]
mod tests;
