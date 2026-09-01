//! Control-plane services: upstream, route and plugin lifecycle.
//!
//! Every operation is tenant-scoped and authorized; every write is validated
//! in [`crate::domain::validation`] and every alias decision in
//! [`crate::domain::alias`]. The service is the only writer of the
//! repositories, so the invariants of `DESIGN` §3.3 hold regardless of which
//! transport calls in.

use std::sync::Arc;

use crate::domain::alias;
use crate::domain::dto::{ListQuery, PluginCommand, RequestContext, RouteCommand, UpstreamCommand};
use crate::domain::error::DomainError;
use crate::domain::model::{
    Plugin, PluginType, ROUTE_TYPE, Route, UPSTREAM_TYPE, Upstream, format_rfc3339, now_epoch_secs,
    resource_gts_id,
};
use crate::domain::repo::{
    ManagementAuthorizer, PluginRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::validation::PayloadRules;

/// Management `resource` spelling used by the authorizer.
const UPSTREAM_RESOURCE: &str = "upstream";
/// Management `resource` spelling used by the authorizer for routes.
const ROUTE_RESOURCE: &str = "route";
/// Management `resource` spelling used by the authorizer for plugins.
const PLUGIN_RESOURCE: &str = "plugin";

/// The upstream/route/plugin control plane.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    authorizer: Arc<dyn ManagementAuthorizer>,
    default_page_size: u64,
    max_page_size: u64,
    /// Deployment cleartext posture (`allow_http_upstream`): when set, an
    /// upstream may register `http`/`ws` endpoints.
    allow_cleartext_endpoints: bool,
}

impl ControlPlaneService {
    /// Assemble a service over the given ports.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        authorizer: Arc<dyn ManagementAuthorizer>,
        default_page_size: u64,
        max_page_size: u64,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            authorizer,
            default_page_size,
            max_page_size,
            allow_cleartext_endpoints: false,
        }
    }

    /// Set the deployment cleartext posture (`allow_http_upstream`).
    ///
    /// `DESIGN` §2.2 keeps upstream connections HTTPS-only; this is the
    /// explicit operator opt-in for deployments that proxy cleartext upstreams
    /// (local testing). The data plane enforces the same posture again, so an
    /// endpoint registered here is still refused unless the deployment
    /// configured it.
    #[must_use]
    pub fn with_cleartext_endpoints(mut self, allow_cleartext: bool) -> Self {
        self.allow_cleartext_endpoints = allow_cleartext;
        self
    }

    /// The page size applied when `$top` is absent.
    #[must_use]
    pub const fn default_page_size(&self) -> u64 {
        self.default_page_size
    }

    /// The largest accepted `$top`.
    #[must_use]
    pub const fn max_page_size(&self) -> u64 {
        self.max_page_size
    }

    // ------------------------------------------------------------- upstreams

    /// Create an upstream.
    ///
    /// # Errors
    /// * [`DomainError::AccessDenied`] when the caller may not create.
    /// * [`DomainError::Validation`] for any payload rule failure.
    /// * [`DomainError::Conflict`] when the alias is already routed.
    pub async fn create_upstream(
        &self,
        ctx: &RequestContext,
        command: UpstreamCommand,
    ) -> Result<Upstream, DomainError> {
        self.authorize(ctx, UPSTREAM_RESOURCE, "create").await?;
        let alias =
            alias::enforce_alias_create(command.alias.as_deref(), &command.server.endpoints)?;
        crate::domain::validation::validate_upstream_with_posture(
            &command.server.endpoints,
            command.protocol,
            command.auth.as_ref(),
            PayloadRules {
                plugins: command.plugins.as_ref(),
                rate_limit: command.rate_limit.as_ref(),
                cors: command.cors.as_ref(),
                tags: &command.tags,
            },
            self.plugins.as_ref(),
            ctx.tenant,
            self.allow_cleartext_endpoints,
        )
        .await?;
        if self
            .upstreams
            .find_by_alias(ctx.tenant, &alias)
            .await?
            .is_some()
        {
            return Err(DomainError::Conflict {
                detail: format!("an upstream is already routed as '{alias}'"),
            });
        }
        let now = format_rfc3339(now_epoch_secs());
        let upstream = Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: ctx.tenant,
            alias,
            protocol: command.protocol,
            enabled: command.enabled,
            server: command.server,
            auth: command.auth,
            headers: command.headers,
            rate_limit: command.rate_limit,
            cors: command.cors,
            plugins: command.plugins,
            tags: command.tags,
            created_at: now.clone(),
            updated_at: now,
        };
        self.upstreams.insert(&upstream).await?;
        Ok(upstream)
    }

    /// Read one upstream of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::AccessDenied`] on an unauthorized read.
    /// * [`DomainError::NotFound`] when the upstream does not belong to the
    ///   tenant (ancestor resources are invisible).
    pub async fn get_upstream(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
    ) -> Result<Upstream, DomainError> {
        self.authorize(ctx, UPSTREAM_RESOURCE, "read").await?;
        self.own_upstream(ctx, id).await
    }

    /// List the upstreams of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::AccessDenied`] on an unauthorized read.
    /// * [`DomainError::Validation`] when the list query is malformed.
    pub async fn list_upstreams(
        &self,
        ctx: &RequestContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, DomainError> {
        self.authorize(ctx, UPSTREAM_RESOURCE, "read").await?;
        let rows = self.upstreams.list(ctx.tenant).await?;
        Ok(crate::domain::dto::apply_list(
            rows,
            query,
            upstream_document,
        ))
    }

    /// Replace an upstream wholesale.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the upstream is not the tenant's own.
    /// * [`DomainError::Validation`] when the replacement is malformed or would
    ///   change the alias.
    pub async fn replace_upstream(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
        command: UpstreamCommand,
    ) -> Result<Upstream, DomainError> {
        self.authorize(ctx, UPSTREAM_RESOURCE, "override").await?;
        let previous = self.own_upstream(ctx, id).await?;
        let alias = alias::enforce_alias_update(
            &previous.alias,
            command.alias.as_deref(),
            &previous.server.endpoints,
            &command.server.endpoints,
        )?;
        crate::domain::validation::validate_upstream_with_posture(
            &command.server.endpoints,
            command.protocol,
            command.auth.as_ref(),
            PayloadRules {
                plugins: command.plugins.as_ref(),
                rate_limit: command.rate_limit.as_ref(),
                cors: command.cors.as_ref(),
                tags: &command.tags,
            },
            self.plugins.as_ref(),
            ctx.tenant,
            self.allow_cleartext_endpoints,
        )
        .await?;

        // The alias is the routing key: a replacement may not hand it to a
        // different row, and the alias of *this* row may not change either.
        if self
            .upstreams
            .find_by_alias(ctx.tenant, &alias)
            .await?
            .is_some_and(|other| other.id != id)
        {
            return Err(DomainError::Conflict {
                detail: format!("an upstream is already routed as '{alias}'"),
            });
        }

        let next = Upstream {
            id: previous.id,
            tenant_id: previous.tenant_id,
            alias,
            protocol: command.protocol,
            enabled: command.enabled,
            server: command.server,
            auth: command.auth,
            headers: command.headers,
            rate_limit: command.rate_limit,
            cors: command.cors,
            plugins: command.plugins,
            tags: command.tags,
            created_at: previous.created_at,
            updated_at: format_rfc3339(now_epoch_secs()),
        };
        self.upstreams.update(&next).await?;
        Ok(next)
    }

    /// Delete an upstream of the calling tenant.
    ///
    /// Routes that target the upstream are deleted with it: an upstream is the
    /// aggregate root of its route table.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the upstream is not the tenant's own.
    /// * [`DomainError::AccessDenied`] on an unauthorized delete.
    pub async fn delete_upstream(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
    ) -> Result<(), DomainError> {
        self.authorize(ctx, UPSTREAM_RESOURCE, "delete").await?;
        let upstream = self.own_upstream(ctx, id).await?;
        for route in self.routes.list_by_upstream(ctx.tenant, id).await? {
            self.routes.delete(ctx.tenant, route.id).await?;
        }
        self.upstreams.delete(ctx.tenant, upstream.id).await?;
        Ok(())
    }

    // ---------------------------------------------------------------- routes

    /// Create a route.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the upstream is not addressable by the
    ///   calling tenant.
    /// * [`DomainError::Validation`] for any payload rule failure.
    /// * [`DomainError::Conflict`] when the match rule is already taken.
    pub async fn create_route(
        &self,
        ctx: &RequestContext,
        command: RouteCommand,
    ) -> Result<Route, DomainError> {
        self.authorize(ctx, ROUTE_RESOURCE, "create").await?;
        let upstream = self.own_upstream(ctx, command.upstream_id).await?;
        crate::domain::validation::validate_route(
            &upstream,
            &command.r#match,
            command.priority,
            PayloadRules {
                plugins: command.plugins.as_ref(),
                rate_limit: command.rate_limit.as_ref(),
                cors: command.cors.as_ref(),
                tags: &command.tags,
            },
            self.plugins.as_ref(),
            ctx.tenant,
        )
        .await?;
        crate::domain::validation::ensure_match_rule_unique(
            self.routes.as_ref(),
            ctx.tenant,
            upstream.id,
            &command.r#match,
            command.priority,
            None,
        )
        .await?;
        let now = format_rfc3339(now_epoch_secs());
        let route = Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: ctx.tenant,
            upstream_id: upstream.id,
            r#match: command.r#match,
            priority: command.priority,
            enabled: command.enabled,
            rate_limit: command.rate_limit,
            cors: command.cors,
            plugins: command.plugins,
            tags: command.tags,
            created_at: now.clone(),
            updated_at: now,
        };
        self.routes.insert(&route).await?;
        Ok(route)
    }

    /// Read one route of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the route is not the tenant's own.
    /// * [`DomainError::AccessDenied`] on an unauthorized read.
    pub async fn get_route(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
    ) -> Result<Route, DomainError> {
        self.authorize(ctx, ROUTE_RESOURCE, "read").await?;
        self.routes
            .find(ctx.tenant, id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: resource_gts_id(ROUTE_TYPE, id),
            })
    }

    /// List the routes of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::AccessDenied`] on an unauthorized read.
    /// * [`DomainError::Validation`] when the list query is malformed.
    pub async fn list_routes(
        &self,
        ctx: &RequestContext,
        query: &ListQuery,
    ) -> Result<Vec<Route>, DomainError> {
        self.authorize(ctx, ROUTE_RESOURCE, "read").await?;
        let rows = self.routes.list(ctx.tenant).await?;
        Ok(crate::domain::dto::apply_list(rows, query, route_document))
    }

    /// Replace a route. `upstream_id` is immutable and absent from the command.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the route is not the tenant's own.
    /// * [`DomainError::Validation`] for any payload rule failure.
    /// * [`DomainError::Conflict`] when the match rule is already taken.
    pub async fn replace_route(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
        command: RouteCommand,
    ) -> Result<Route, DomainError> {
        self.authorize(ctx, ROUTE_RESOURCE, "override").await?;
        let previous =
            self.routes
                .find(ctx.tenant, id)
                .await?
                .ok_or_else(|| DomainError::NotFound {
                    resource: resource_gts_id(ROUTE_TYPE, id),
                })?;
        if command.upstream_id != previous.upstream_id {
            return Err(DomainError::validation(
                "route.upstream_id is immutable and must match the existing route",
            ));
        }
        let upstream = self.own_upstream(ctx, previous.upstream_id).await?;
        crate::domain::validation::validate_route(
            &upstream,
            &command.r#match,
            command.priority,
            PayloadRules {
                plugins: command.plugins.as_ref(),
                rate_limit: command.rate_limit.as_ref(),
                cors: command.cors.as_ref(),
                tags: &command.tags,
            },
            self.plugins.as_ref(),
            ctx.tenant,
        )
        .await?;
        crate::domain::validation::ensure_match_rule_unique(
            self.routes.as_ref(),
            ctx.tenant,
            previous.upstream_id,
            &command.r#match,
            command.priority,
            Some(previous.id),
        )
        .await?;
        let next = Route {
            id: previous.id,
            tenant_id: previous.tenant_id,
            upstream_id: previous.upstream_id,
            r#match: command.r#match,
            priority: command.priority,
            enabled: command.enabled,
            rate_limit: command.rate_limit,
            cors: command.cors,
            plugins: command.plugins,
            tags: command.tags,
            created_at: previous.created_at,
            updated_at: format_rfc3339(now_epoch_secs()),
        };
        self.routes.update(&next).await?;
        Ok(next)
    }

    /// Delete a route of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the route is not the tenant's own.
    /// * [`DomainError::AccessDenied`] on an unauthorized delete.
    pub async fn delete_route(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
    ) -> Result<(), DomainError> {
        self.authorize(ctx, ROUTE_RESOURCE, "delete").await?;
        let route =
            self.routes
                .find(ctx.tenant, id)
                .await?
                .ok_or_else(|| DomainError::NotFound {
                    resource: resource_gts_id(ROUTE_TYPE, id),
                })?;
        self.routes.delete(ctx.tenant, route.id).await?;
        Ok(())
    }

    // --------------------------------------------------------------- plugins

    /// Create a custom Starlark plugin.
    ///
    /// # Errors
    /// * [`DomainError::Validation`] for a malformed source or name.
    /// * [`DomainError::Conflict`] when the name is already taken in the
    ///   tenant for the same plugin kind.
    pub async fn create_plugin(
        &self,
        ctx: &RequestContext,
        command: PluginCommand,
    ) -> Result<Plugin, DomainError> {
        self.authorize(ctx, PLUGIN_RESOURCE, "create").await?;
        crate::domain::validation::validate_plugin_source(&command.source_code)?;
        let name = command.name.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(DomainError::validation(
                "plugin name must be 1..=128 characters",
            ));
        }
        let existing = self
            .plugins
            .list(ctx.tenant, Some(command.plugin_type))
            .await?;
        if existing.iter().any(|p| p.name == name) {
            return Err(DomainError::Conflict {
                detail: format!(
                    "a {} plugin named '{name}' already exists",
                    command.plugin_type.gts_base_type()
                ),
            });
        }
        let now = format_rfc3339(now_epoch_secs());
        let plugin = Plugin {
            id: uuid::Uuid::new_v4(),
            tenant_id: ctx.tenant,
            plugin_type: command.plugin_type,
            name: name.to_owned(),
            config_schema: command.config_schema,
            source_code: command.source_code,
            phases: command.phases,
            created_at: now.clone(),
            updated_at: now,
            last_used_at: None,
            gc_eligible_at: None,
        };
        self.plugins.insert(&plugin).await?;
        Ok(plugin)
    }

    /// Read one custom plugin of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the plugin is not the tenant's own.
    /// * [`DomainError::AccessDenied`] on an unauthorized read.
    pub async fn get_plugin(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
        requested: &str,
    ) -> Result<Plugin, DomainError> {
        self.authorize(ctx, PLUGIN_RESOURCE, "read").await?;
        self.own_plugin(ctx, id, requested).await
    }

    /// List the custom plugins of the calling tenant.
    ///
    /// # Errors
    /// * [`DomainError::AccessDenied`] on an unauthorized read.
    /// * [`DomainError::Validation`] when the list query is malformed.
    pub async fn list_plugins(
        &self,
        ctx: &RequestContext,
        query: &ListQuery,
        kind: Option<PluginType>,
    ) -> Result<Vec<Plugin>, DomainError> {
        self.authorize(ctx, PLUGIN_RESOURCE, "read").await?;
        let rows = self.plugins.list(ctx.tenant, kind).await?;
        Ok(crate::domain::dto::apply_list(rows, query, plugin_document))
    }

    /// Read the Starlark source of a custom plugin.
    ///
    /// # Errors
    /// Same as [`Self::get_plugin`].
    pub async fn get_plugin_source(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
        requested: &str,
    ) -> Result<String, DomainError> {
        let plugin = self.get_plugin(ctx, id, requested).await?;
        Ok(plugin.source_code)
    }

    /// Delete a custom plugin.
    ///
    /// Returns [`DomainError::PluginInUse`] with the referencing GTS ids when
    /// any upstream or route still binds the plugin.
    ///
    /// # Errors
    /// * [`DomainError::NotFound`] when the plugin is not the tenant's own.
    /// * [`DomainError::PluginInUse`] when the plugin is referenced.
    /// * [`DomainError::AccessDenied`] on an unauthorized delete.
    pub async fn delete_plugin(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
        requested: &str,
    ) -> Result<(), DomainError> {
        self.authorize(ctx, PLUGIN_RESOURCE, "delete").await?;
        let plugin = self.own_plugin(ctx, id, requested).await?;
        let (upstreams, routes) = self.plugins.references(ctx.tenant, plugin.id).await?;
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(DomainError::PluginInUse {
                plugin_id: resource_gts_id(plugin.plugin_type.gts_base_type(), plugin.id),
                upstreams,
                routes,
            });
        }
        self.plugins.delete(ctx.tenant, plugin.id).await?;
        Ok(())
    }

    // --------------------------------------------------------------- helpers

    /// Resolve an upstream that must be owned by the calling tenant.
    async fn own_upstream(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
    ) -> Result<Upstream, DomainError> {
        self.upstreams
            .find(ctx.tenant, id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: resource_gts_id(UPSTREAM_TYPE, id),
            })
    }

    /// Resolve a plugin that must be owned by the calling tenant.
    ///
    /// `requested` is the id as the caller wrote it: the `404` names the
    /// resource with the base type the caller used, so a missing guard plugin
    /// is not reported as an auth plugin.
    async fn own_plugin(
        &self,
        ctx: &RequestContext,
        id: uuid::Uuid,
        requested: &str,
    ) -> Result<Plugin, DomainError> {
        self.plugins
            .find(ctx.tenant, id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: resource_gts_id(crate::domain::model::plugin_base_type_of(requested), id),
            })
    }

    async fn authorize(
        &self,
        ctx: &RequestContext,
        resource: &str,
        action: &str,
    ) -> Result<(), DomainError> {
        self.authorizer
            .authorize(ctx.tenant, &ctx.subject, resource, action)
            .await
    }
}

/// JSON projection of an upstream, used for `$filter`/`$orderby`.
fn upstream_document(upstream: &Upstream) -> serde_json::Value {
    serde_json::to_value(upstream).unwrap_or(serde_json::Value::Null)
}

/// JSON projection of a route, used for `$filter`/`$orderby`.
fn route_document(route: &Route) -> serde_json::Value {
    serde_json::to_value(route).unwrap_or(serde_json::Value::Null)
}

/// JSON projection of a plugin, used for `$filter`/`$orderby` and by the REST
/// list envelope.
///
/// A `type` alias is added so the documented `$filter=type eq 'guard'`
/// spelling works next to `plugin_type eq 'guard'`.
#[must_use]
pub fn plugin_document(plugin: &Plugin) -> serde_json::Value {
    let mut document = serde_json::to_value(plugin).unwrap_or(serde_json::Value::Null);
    // The document carries the same anonymous GTS identifier every other
    // plugin view spells, so a listed plugin can be addressed again.
    if let Some(object) = document.as_object_mut() {
        object.insert(
            "id".to_owned(),
            serde_json::Value::String(crate::domain::model::resource_gts_id(
                plugin.plugin_type.gts_base_type(),
                plugin.id,
            )),
        );
    }
    let kind = match plugin.plugin_type {
        PluginType::Auth => "auth",
        PluginType::Guard => "guard",
        PluginType::Transform => "transform",
    };
    if let Some(object) = document.as_object_mut() {
        object.insert(
            "type".to_owned(),
            serde_json::Value::String(kind.to_owned()),
        );
    }
    document
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "services_tests.rs"]
mod tests;
