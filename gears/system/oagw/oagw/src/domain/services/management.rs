//! Control-plane service: CRUD for upstreams, routes and plugins.
//!
//! All tenant scoping, alias enforcement and uniqueness rules live here so the
//! REST layer only has to map DTOs and errors.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias::{self, AliasOutcome};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::{
    Plugin, PluginType, Route, RouteSpec, Upstream, UpstreamSpec, normalize_alias, now_unix,
    validate_alias, validate_tag,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// References to the persistence ports the service needs.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
}

/// Validated payload of a plugin create request.
pub struct NewPlugin {
    /// Human-readable name.
    pub name: String,
    /// Plugin kind.
    pub kind: PluginType,
    /// Optional JSON Schema for the plugin configuration.
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source.
    pub source_code: String,
}

impl ControlPlaneService {
    /// Assemble the service from its repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
        }
    }

    /// Data-plane resolver over the same repositories as this control plane.
    ///
    /// The management API writes what the proxy reads, so both planes always
    /// observe one consistent view of the configured tenants.
    #[must_use]
    pub fn data_plane(
        &self,
        tenants: Option<Arc<dyn tenant_resolver_sdk::api::TenantResolverClient>>,
    ) -> crate::domain::services::proxy::DataPlaneService {
        crate::domain::services::proxy::DataPlaneService::new(
            Arc::clone(&self.upstreams),
            Arc::clone(&self.routes),
            tenants,
        )
    }

    // ---------------------------------------------------------------- upstreams

    /// Create an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a malformed spec or alias, and a
    /// conflict error when the derived or explicit alias is already taken.
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        spec: UpstreamSpec,
        requested_alias: Option<&str>,
    ) -> Result<Upstream, DomainError> {
        validate_spec(&spec)?;
        let outcome = alias::derive_alias(&spec.server.endpoints, requested_alias)?;
        let now = now_unix();
        let record = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias: match &outcome {
                AliasOutcome::Derived(value) | AliasOutcome::Explicit(value) => value.clone(),
            },
            alias_explicit: matches!(outcome, AliasOutcome::Explicit(_)),
            spec,
            created_at: now,
            updated_at: now,
        };
        self.upstreams.insert(record.clone())?;
        Ok(record)
    }

    /// Fetch one upstream.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        self.upstreams.find(tenant_id, id)
    }

    /// Fetch one upstream by normalized alias.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn find_upstream_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        self.upstreams.find_by_alias(tenant_id, alias)
    }

    /// List the tenant's upstreams.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list(tenant_id)
    }

    /// Replace an upstream. `requested_alias` is only honoured when the
    /// endpoints are IP-based (see `alias::enforce_alias_update`).
    ///
    /// # Errors
    ///
    /// Returns a not-found error for a foreign or missing row, a validation
    /// error for an illegal alias change, and a conflict error when the new
    /// alias collides with a sibling upstream.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: UpstreamSpec,
        requested_alias: Option<&str>,
    ) -> Result<Upstream, DomainError> {
        validate_spec(&spec)?;
        let existing = self
            .upstreams
            .find(tenant_id, id)?
            .ok_or_else(|| not_found("upstream"))?;
        alias::enforce_alias_update(
            &existing.alias,
            !existing.alias_explicit,
            &spec.server.endpoints,
            requested_alias,
        )?;
        let record = Upstream {
            id,
            tenant_id,
            alias: existing.alias.clone(),
            alias_explicit: existing.alias_explicit,
            spec,
            created_at: existing.created_at,
            updated_at: now_unix(),
        };
        self.upstreams.update(record.clone())?;
        Ok(record)
    }

    /// Delete an upstream and cascade its routes.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let existed = self.upstreams.delete(tenant_id, id)?;
        if existed {
            self.routes.delete_by_upstream(tenant_id, id)?;
        }
        Ok(existed)
    }

    // ------------------------------------------------------------------- routes

    /// Create a route for `upstream_id`.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when the upstream is unknown or foreign, a
    /// validation error for a malformed spec, and a conflict error when the
    /// match rule is already taken.
    pub fn create_route(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        validate_route_spec(&spec)?;
        if self.upstreams.find(tenant_id, upstream_id)?.is_none() {
            return Err(not_found("upstream"));
        }
        let now = now_unix();
        let record = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            spec,
            created_at: now,
            updated_at: now,
        };
        self.routes.insert(record.clone())?;
        Ok(record)
    }

    /// Fetch one route.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        self.routes.find(tenant_id, id)
    }

    /// List the tenant's routes, optionally narrowed to one upstream.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn list_routes(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> Result<Vec<Route>, DomainError> {
        self.routes.list(tenant_id, upstream_id)
    }

    /// Replace a route. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// Returns a not-found error for a foreign or missing row, a validation
    /// error when the payload names a different upstream, and a conflict error
    /// when the match rule collides.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        upstream_id: Uuid,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        validate_route_spec(&spec)?;
        let existing = self
            .routes
            .find(tenant_id, id)?
            .ok_or_else(|| not_found("route"))?;
        if upstream_id != existing.upstream_id {
            return Err(DomainError::validation(
                "upstream_id is immutable: create a new route bound to another upstream",
            ));
        }
        let record = Route {
            id,
            tenant_id,
            upstream_id: existing.upstream_id,
            spec,
            created_at: existing.created_at,
            updated_at: now_unix(),
        };
        self.routes.update(record.clone())?;
        Ok(record)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        self.routes.delete(tenant_id, id)
    }

    // ------------------------------------------------------------------ plugins

    /// Register a custom plugin.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a missing name or source.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        new_plugin: NewPlugin,
    ) -> Result<Plugin, DomainError> {
        if new_plugin.name.trim().is_empty() {
            return Err(DomainError::validation("name must not be empty"));
        }
        if new_plugin.source_code.trim().is_empty() {
            return Err(DomainError::validation("source_code must not be empty"));
        }
        let now = now_unix();
        let record = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            name: new_plugin.name,
            kind: new_plugin.kind,
            config_schema: new_plugin.config_schema,
            source_code: new_plugin.source_code,
            created_at: now,
            updated_at: now,
        };
        self.plugins.insert(record.clone())?;
        Ok(record)
    }

    /// Fetch one plugin.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        self.plugins.find(tenant_id, id)
    }

    /// List the tenant's custom plugins.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    pub fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        self.plugins.list(tenant_id)
    }

    /// Delete a plugin, refusing while any upstream or route still binds it.
    ///
    /// # Errors
    ///
    /// Returns a not-found error for a foreign or missing row and
    /// `PluginInUse` when a binding still references the plugin.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        if self.plugins.find(tenant_id, id)?.is_none() {
            return Ok(false);
        }
        let bound = |refs: Vec<String>| {
            refs.iter()
                .any(|r| crate::ids::uuid_instance(r) == Some(id))
        };
        for upstream in self.upstreams.list(tenant_id)? {
            let mut refs: Vec<String> = upstream
                .spec
                .plugins
                .items
                .iter()
                .map(|b| b.plugin_ref.clone())
                .collect();
            if let Some(auth) = &upstream.spec.auth {
                refs.push(auth.plugin_type.clone());
            }
            if bound(refs) {
                return Err(DomainError::new(
                    ErrorKind::PluginInUse,
                    "plugin is still referenced by an upstream or route",
                ));
            }
        }
        for route in self.routes.list(tenant_id, None)? {
            let refs: Vec<String> = route
                .spec
                .plugins
                .items
                .iter()
                .map(|b| b.plugin_ref.clone())
                .collect();
            if bound(refs) {
                return Err(DomainError::new(
                    ErrorKind::PluginInUse,
                    "plugin is still referenced by an upstream or route",
                ));
            }
        }
        self.plugins.delete(tenant_id, id)
    }
}

fn not_found(resource: &str) -> DomainError {
    DomainError::new(
        ErrorKind::ResourceNotFound,
        format!("{resource} does not exist in this tenant"),
    )
}

/// Validate an upstream spec: endpoints present, hosts well-formed, tags and
/// plugin references legal.
///
/// # Errors
///
/// Returns a validation error describing the first violation.
pub fn validate_spec(spec: &UpstreamSpec) -> Result<(), DomainError> {
    if spec.server.endpoints.is_empty() {
        return Err(DomainError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    }
    for endpoint in &spec.server.endpoints {
        validate_host_field(&endpoint.host)?;
        if endpoint.port == 0 {
            return Err(DomainError::validation(
                "server.endpoints[].port must be between 1 and 65535",
            ));
        }
    }
    for tag in &spec.tags {
        validate_tag(tag)?;
    }
    validate_plugin_bindings(spec.plugins.items.iter().map(|b| b.plugin_ref.as_str()))?;
    Ok(())
}

/// Validate a route spec: match rule well-formed, tags legal.
///
/// # Errors
///
/// Returns a validation error describing the first violation.
pub fn validate_route_spec(spec: &RouteSpec) -> Result<(), DomainError> {
    match &spec.r#match {
        crate::domain::model::RouteMatch::Http(rules) => {
            if rules.path.is_empty() || !rules.path.starts_with('/') {
                return Err(DomainError::validation(
                    "match.http.path must start with `/`",
                ));
            }
            for method in &rules.methods {
                let valid = !method.is_empty()
                    && method.chars().all(|c| c.is_ascii_uppercase() || c == '_');
                if !valid {
                    return Err(DomainError::validation(format!(
                        "match.http.methods contains an invalid method: `{method}`"
                    )));
                }
            }
        }
        crate::domain::model::RouteMatch::Grpc(rules) => {
            if rules.service.is_empty() || rules.method.is_empty() {
                return Err(DomainError::validation(
                    "match.grpc requires both `service` and `method`",
                ));
            }
        }
    }
    for tag in &spec.tags {
        validate_tag(tag)?;
    }
    validate_plugin_bindings(spec.plugins.items.iter().map(|b| b.plugin_ref.as_str()))?;
    Ok(())
}

fn validate_host_field(host: &str) -> Result<(), DomainError> {
    crate::domain::model::validate_host(host).map(|_| ())
}

/// Validate plugin references syntactically (a full GTS id or a UUID).
///
/// # Errors
///
/// Returns a validation error for a reference that is neither form.
fn validate_plugin_bindings<'a, I>(refs: I) -> Result<(), DomainError>
where
    I: IntoIterator<Item = &'a str>,
{
    for reference in refs {
        let is_uuid = crate::ids::uuid_instance(reference).is_some();
        let is_gts = reference.contains('.');
        if !is_uuid && !is_gts {
            return Err(DomainError::validation(format!(
                "plugin_ref must be a GTS identifier or a UUID: `{reference}`"
            )));
        }
        // Catalog-only identifiers exist for types-registry cataloging only;
        // they have no backing plugin and must never be bound (DESIGN.md:
        // "cannot be bound via plugins.items[].plugin_ref").
        let instance = crate::ids::instance_part(reference);
        let catalogued = crate::ids::CATALOG_ONLY_AUTH.contains(&instance)
            || crate::ids::CATALOG_ONLY_GUARD.contains(&instance)
            || crate::ids::CATALOG_ONLY_TRANSFORM.contains(&instance);
        if catalogued {
            return Err(DomainError::validation(format!(
                "plugin_ref `{reference}` is catalog-only and cannot be bound"
            )));
        }
    }
    Ok(())
}

/// Normalize and validate an explicit alias before it reaches the service.
///
/// # Errors
///
/// Returns a validation error for a malformed alias.
pub fn normalize_requested_alias(requested: Option<&str>) -> Result<Option<String>, DomainError> {
    requested
        .map(|raw| {
            let normalized = normalize_alias(raw);
            validate_alias(&normalized)?;
            Ok(normalized)
        })
        .transpose()
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod management_tests;
