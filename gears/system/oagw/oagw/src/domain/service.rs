// Created: 2026-09-02 by Constructor Tech
//! Control-plane service: CRUD semantics for upstreams, routes and plugins.
//!
//! Every operation is scoped to the calling tenant. Ancestor resources are
//! invisible to the management API (404, never 403) but are reachable on the
//! data plane through the tenant chain walk (`DESIGN.md` §3.3 Tenant Scoping).
//!
//! The alias rules in §3.2 are enforced here in full: derivation, the
//! idempotent exact-match tolerance, and the immutable-alias update matrix.

use uuid::Uuid;

use crate::domain::model::{
    self, CorsConfig, Endpoint, HttpMatch, Plugin, PluginType, Protocol, Route,
    Upstream,
};
use crate::domain::plugin::SecretResolver;
use crate::domain::store::{self, Store};
use serde_json::Value;
use crate::error::GatewayError;
use tenant_resolver_sdk::TenantResolverClient;

/// Who is calling, as far as the control plane is concerned.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The caller's tenant.
    pub tenant_id: Uuid,
    /// Ancestors of the caller's tenant, nearest first (self excluded).
    pub ancestors: Vec<Uuid>,
    /// Subject tenant, for plugin cache isolation.
    pub subject_tenant_id: Uuid,
    /// Subject identifier, for plugin cache isolation.
    pub subject_id: String,
}

impl Caller {
    /// The tenant chain for proxy resolution: self first, then ancestors.
    #[must_use]
    pub fn chain(&self) -> Vec<Uuid> {
        let mut chain = vec![self.tenant_id];
        chain.extend(self.ancestors.iter().copied());
        chain
    }

    /// Whether the caller's tenant owns `resource_tenant`.
    #[must_use]
    pub fn owns(&self, resource_tenant: Uuid) -> bool {
        resource_tenant == self.tenant_id
    }
}

/// Control-plane service over the in-memory store.
#[derive(Clone)]
pub struct ControlPlane {
    store: std::sync::Arc<Store>,
    list_top_default: usize,
    list_top_max: usize,
    secrets: std::sync::Arc<dyn SecretResolver>,
    resolver: Option<std::sync::Arc<dyn TenantResolverClient>>,
}

impl ControlPlane {
    /// Builds a service over `store`.
    #[must_use]
    pub fn new(store: std::sync::Arc<Store>, list_top_default: usize, list_top_max: usize) -> Self {
        Self {
            store,
            list_top_default,
            list_top_max,
            secrets: std::sync::Arc::new(crate::infra::client::NoopSecretResolver),
            resolver: None,
        }
    }

    /// The backing store (data plane reads go through here too).
    #[must_use]
    pub fn store(&self) -> std::sync::Arc<Store> {
        self.store.clone()
    }

    /// Attaches the credential store used to resolve `secret_ref` values.
    #[must_use]
    pub fn with_secrets(mut self, secrets: std::sync::Arc<dyn SecretResolver>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Attaches the tenant resolver used for hierarchical lookups.
    #[must_use]
    pub fn with_tenant_resolver(
        mut self,
        resolver: std::sync::Arc<dyn TenantResolverClient>,
    ) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// The secret resolver in force.
    #[must_use]
    pub fn secrets(&self) -> std::sync::Arc<dyn SecretResolver> {
        self.secrets.clone()
    }

    /// The tenant resolver in force, when the platform provides one.
    #[must_use]
    pub fn resolver(&self) -> Option<std::sync::Arc<dyn TenantResolverClient>> {
        self.resolver.clone()
    }

    // ---------------------------------------------------------------- upstreams

    /// Resolves the alias of a new upstream: the caller's value when one is
    /// given, the name derived from the endpoints otherwise.
    ///
    /// IP-literal endpoint pools have no shared domain suffix to name
    /// themselves after, so there the alias is mandatory.
    fn resolve_new_alias(&self, spec: &Upstream) -> Result<String, GatewayError> {
        let derived = model::compute_derived_alias(&spec.server.endpoints);
        match non_empty(&spec.alias) {
            Some(alias) => {
                model::validate_alias(alias)?;
                let normalized = alias.to_ascii_lowercase();
                // A derivable pool is always named after its endpoints; the
                // caller's exact value is tolerated for idempotency only.
                if let Some(derived) = derived
                    && !derived.eq_ignore_ascii_case(&normalized) {
                        return Err(GatewayError::Validation(format!(
                            "alias '{alias}' must be the auto-derived alias '{derived}' for these endpoints"
                        )));
                    }
                Ok(normalized)
            }
            None => derived.ok_or_else(|| {
                GatewayError::Validation(
                    "alias is required: an IP-literal endpoint pool has no domain to derive one from"
                        .to_owned(),
                )
            }),
        }
    }

    /// Creates an upstream.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] for an invalid spec or a rejected alias,
    /// [`GatewayError::Conflict`] when the alias is already taken.
    pub fn create_upstream(&self, caller: &Caller, mut spec: Upstream) -> Result<Upstream, GatewayError> {
        validate_upstream(&spec)?;
        spec.alias = self.resolve_new_alias(&spec)?;
        spec.tags = validate_tags(spec.tags)?;
        if let Some(existing) = self.store.upstream_with_alias(&[caller.tenant_id], &spec.alias) {
            return Err(alias_conflict(&existing.spec.alias));
        }
        let record = self.store.insert_upstream(caller.tenant_id, spec);
        Ok(record.spec)
    }

    /// Returns an upstream owned by the caller, or `NotFound`.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the resource is absent or invisible.
    pub fn get_upstream(&self, caller: &Caller, id: &str) -> Result<Upstream, GatewayError> {
        let uuid = parse_id(id)?;
        let record = self
            .store
            .get_upstream(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| not_found("upstream", id))?;
        Ok(record.spec)
    }

    /// Lists the caller's upstreams, applying the OData query.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] when the query is malformed.
    pub fn list_upstreams(
        &self,
        caller: &Caller,
        query: &crate::domain::query::ListQuery,
    ) -> Result<Vec<Value>, GatewayError> {
        let items = self
            .store
            .upstreams_for(&[caller.tenant_id])
            .into_iter()
            .map(|r| r.spec)
            .collect::<Vec<_>>();
        crate::domain::query::apply(&items, query, self.list_top_default, self.list_top_max)
    }

    /// Replaces an upstream (full replacement, immutable alias).
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] for an invalid spec or an alias change,
    /// [`GatewayError::NotFound`] when the upstream is invisible.
    pub fn replace_upstream(
        &self,
        caller: &Caller,
        id: &str,
        mut spec: Upstream,
    ) -> Result<Upstream, GatewayError> {
        let uuid = parse_id(id)?;
        let existing = self
            .store
            .get_upstream(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| GatewayError::NotFound(format!("upstream '{id}' was not found")))?;

        validate_upstream(&spec)?;
        spec.tags = validate_tags(spec.tags)?;

        // Alias is the routing key and is immutable. A caller-supplied alias
        // that differs from the stored one is rejected outright; an endpoint
        // change that would re-derive a different alias is rejected too, even
        // when the caller stays silent — the operator deletes and re-creates.
        let provided = non_empty(&spec.alias);
        if let Some(alias) = provided
            && !alias.eq_ignore_ascii_case(&existing.spec.alias) {
                return Err(GatewayError::Validation(format!(
                    "alias '{alias}' does not match the existing alias '{}': the alias is immutable",
                    existing.spec.alias
                )));
            }
        let derived = model::compute_derived_alias(&spec.server.endpoints);
        let was_derivable = model::compute_derived_alias(&existing.spec.server.endpoints).is_some();
        match derived {
            // Still derivable, under a different name: the routing key moves.
            Some(name) if !name.eq_ignore_ascii_case(&existing.spec.alias) => {
                return Err(GatewayError::Validation(format!(
                    "endpoint change would alter the derived alias '{name}'; the alias is immutable, delete and re-create the upstream"
                )));
            }
            // A derivable pool that stops being derivable has no name to keep.
            None if was_derivable => {
                return Err(GatewayError::Validation(
                    "endpoint change makes the alias non-derivable; the alias is immutable, delete and re-create the upstream"
                        .to_owned(),
                ));
            }
            _ => {}
        }
        spec.alias = existing.spec.alias.clone();
        spec.id = Some(store::upstream_gts_id(uuid));

        let mut record = existing;
        record.spec = spec;
        self.store.put_upstream(&record);
        Ok(record.spec)
    }

    /// Enables or disables an upstream.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the upstream is invisible.
    pub fn set_upstream_enabled(
        &self,
        caller: &Caller,
        id: &str,
        enabled: bool,
    ) -> Result<Upstream, GatewayError> {
        let uuid = parse_id(id)?;
        let mut record = self
            .store
            .get_upstream(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| GatewayError::NotFound(format!("upstream '{id}' was not found")))?;
        record.spec.enabled = enabled;
        self.store.put_upstream(&record);
        Ok(record.spec)
    }

    /// Deletes an upstream and the routes bound to it.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the upstream is invisible.
    pub fn delete_upstream(&self, caller: &Caller, id: &str) -> Result<(), GatewayError> {
        let uuid = parse_id(id)?;
        let record = self
            .store
            .remove_upstream(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| GatewayError::NotFound(format!("upstream '{id}' was not found")))?;
        for route in self.store.routes_for_upstream(record.id) {
            let _removed = self.store.remove_route(route.id);
        }
        Ok(())
    }

    // ------------------------------------------------------------------- routes

    /// Creates a route bound to an upstream owned by the caller.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`], [`GatewayError::NotFound`],
    /// [`GatewayError::Conflict`] on a duplicate match rule.
    pub fn create_route(&self, caller: &Caller, spec: Route) -> Result<Route, GatewayError> {
        let upstream_id = parse_id(&spec.upstream_id)?;
        let upstream = self
            .store
            .get_upstream(upstream_id)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| {
                GatewayError::NotFound(format!("upstream '{}' was not found", spec.upstream_id))
            })?;
        validate_route(&spec, upstream.spec.protocol)?;
        let tags = validate_tags(spec.tags.clone())?;

        let mut owned = spec;
        owned.tags = tags;
        if let Some(http) = owned.match_config.http.as_ref() {
            let conflicts = self.store.route_match_conflicts(upstream_id, None, http);
            if let Some(first) = conflicts.first() {
                return Err(match_conflict(&first.id.to_string()));
            }
        }

        let record = self.store.insert_route(caller.tenant_id, upstream_id, owned);
        Ok(record.spec)
    }

    /// Returns a route owned by the caller.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the route is invisible.
    pub fn get_route(&self, caller: &Caller, id: &str) -> Result<Route, GatewayError> {
        let uuid = parse_id(id)?;
        self.store
            .get_route(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .map(|r| r.spec)
            .ok_or_else(|| GatewayError::NotFound(format!("route '{id}' was not found")))
    }

    /// Lists the caller's routes, optionally filtered by upstream.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] when the query is malformed.
    pub fn list_routes(
        &self,
        caller: &Caller,
        query: &crate::domain::query::ListQuery,
    ) -> Result<Vec<Value>, GatewayError> {
        let items = self
            .store
            .routes_for(&[caller.tenant_id])
            .into_iter()
            .map(|r| r.spec)
            .collect::<Vec<_>>();
        crate::domain::query::apply(&items, query, self.list_top_default, self.list_top_max)
    }

    /// Replaces a route. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`], [`GatewayError::NotFound`],
    /// [`GatewayError::Conflict`].
    pub fn replace_route(
        &self,
        caller: &Caller,
        id: &str,
        mut spec: Route,
    ) -> Result<Route, GatewayError> {
        let uuid = parse_id(id)?;
        let existing = self
            .store
            .get_route(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| GatewayError::NotFound(format!("route '{id}' was not found")))?;

        if non_empty(&spec.upstream_id).is_some()
            && !spec.upstream_id.eq_ignore_ascii_case(&store::upstream_gts_id(existing.upstream_id))
        {
            return Err(GatewayError::Conflict(
                "route.upstream_id is immutable".to_owned(),
            ));
        }
        spec.upstream_id = store::upstream_gts_id(existing.upstream_id);
        validate_route(&spec, self.upstream_protocol(existing.upstream_id))?;
        spec.tags = validate_tags(spec.tags)?;

        if let Some(http) = spec.match_config.http.as_ref() {
            let conflicts = self.store.route_match_conflicts(existing.upstream_id, Some(uuid), http);
            if let Some(first) = conflicts.first() {
                return Err(match_conflict(&first.id.to_string()));
            }
        }
        spec.id = Some(store::route_gts_id(uuid));

        let mut record = existing;
        record.spec = spec;
        self.store.put_route(&record);
        Ok(record.spec)
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the route is invisible.
    pub fn delete_route(&self, caller: &Caller, id: &str) -> Result<(), GatewayError> {
        let uuid = parse_id(id)?;
        self.store
            .remove_route(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| GatewayError::NotFound(format!("route '{id}' was not found")))?;
        Ok(())
    }

    fn upstream_protocol(&self, upstream_id: Uuid) -> Protocol {
        self.store
            .get_upstream(upstream_id)
            .map(|r| r.spec.protocol)
            .unwrap_or(Protocol::Http)
    }

    // ------------------------------------------------------------------ plugins

    /// Creates an immutable plugin definition.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] for a bad name or empty source,
    /// [`GatewayError::Conflict`] when the name is taken.
    pub fn create_plugin(
        &self,
        caller: &Caller,
        spec: Plugin,
    ) -> Result<Plugin, GatewayError> {
        if spec.name.trim().is_empty() || spec.name.len() > 128 {
            return Err(GatewayError::Validation(
                "plugin.name must be 1..=128 characters".to_owned(),
            ));
        }
        if spec.source_code.trim().is_empty() {
            return Err(GatewayError::Validation("plugin.source_code must not be empty".to_owned()));
        }
        if self.store.plugin_with_name(&[caller.tenant_id], &spec.name).is_some() {
            return Err(GatewayError::Conflict(format!(
                "a plugin named '{}' already exists",
                spec.name
            )));
        }
        let record = self.store.insert_plugin(caller.tenant_id, spec);
        Ok(record.spec)
    }

    /// Returns a plugin definition owned by the caller.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the plugin is invisible.
    pub fn get_plugin(&self, caller: &Caller, id: &str) -> Result<Plugin, GatewayError> {
        let uuid = parse_id(id)?;
        self.store
            .get_plugin(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .map(|r| r.spec)
            .ok_or_else(|| GatewayError::NotFound(format!("plugin '{id}' was not found")))
    }

    /// Lists the caller's plugin definitions.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] when the query is malformed.
    pub fn list_plugins(
        &self,
        caller: &Caller,
        query: &crate::domain::query::ListQuery,
    ) -> Result<Vec<Value>, GatewayError> {
        let items = self
            .store
            .plugins_for(&[caller.tenant_id])
            .into_iter()
            .map(|r| r.spec)
            .collect::<Vec<_>>();
        crate::domain::query::apply(&items, query, self.list_top_default, self.list_top_max)
    }

    /// Returns the Starlark source of a plugin, as `GET /plugins/{id}/source`.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the plugin is invisible.
    pub fn plugin_source(&self, caller: &Caller, id: &str) -> Result<String, GatewayError> {
        Ok(self.get_plugin(caller, id)?.source_code)
    }

    /// Deletes a plugin unless an upstream or route still references it.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NotFound`] when the plugin is invisible,
    /// [`GatewayError::PluginInUse`] when it is still bound.
    pub fn delete_plugin(&self, caller: &Caller, id: &str) -> Result<(), GatewayError> {
        let uuid = parse_id(id)?;
        let gts_id = store::plugin_gts_id(uuid);
        let record = self
            .store
            .remove_plugin(uuid)
            .filter(|r| caller.owns(r.tenant_id))
            .ok_or_else(|| GatewayError::NotFound(format!("plugin '{id}' was not found")))?;

        let referencing_upstreams: Vec<String> = self
            .store
            .upstreams_referencing_plugin(&gts_id)
            .iter()
            .filter(|u| u.tenant_id == record.tenant_id)
            .filter_map(|u| u.spec.id.clone())
            .collect();
        let referencing_routes: Vec<String> = self
            .store
            .routes_referencing_plugin(&gts_id)
            .iter()
            .filter(|r| r.tenant_id == record.tenant_id)
            .filter_map(|r| r.spec.id.clone())
            .collect();

        if !referencing_upstreams.is_empty() || !referencing_routes.is_empty() {
            // Restore the definition: the delete did not happen.
            self.store.put_plugin(&record);
            return Err(GatewayError::PluginInUse {
                plugin_id: gts_id,
                upstreams: referencing_upstreams,
                routes: referencing_routes,
            });
        }
        Ok(())
    }
}

fn parse_id(raw: &str) -> Result<Uuid, GatewayError> {
    store::parse_resource_id(raw)
        .ok_or_else(|| GatewayError::Validation(format!("'{raw}' is not a valid resource id")))
}

fn not_found(kind: &str, id: &str) -> GatewayError {
    GatewayError::NotFound(format!("{kind} '{id}' was not found"))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn alias_conflict(alias: &str) -> GatewayError {
    GatewayError::Conflict(format!("an upstream with alias '{alias}' already exists"))
}

fn match_conflict(route_id: &str) -> GatewayError {
    GatewayError::Conflict(format!(
        "another route of the same upstream already matches this method and path (route '{route_id}')"
    ))
}

fn validate_tags(tags: Vec<String>) -> Result<Vec<String>, GatewayError> {
    tags.iter().try_for_each(|tag| model::validate_tag(tag))?;
    Ok(tags)
}

/// Validates an upstream spec that is about to be stored.
///
/// # Errors
///
/// [`GatewayError::Validation`] with the first problem found.
pub fn validate_upstream(spec: &Upstream) -> Result<(), GatewayError> {
    if spec.server.endpoints.is_empty() {
        return Err(GatewayError::Validation(
            "server.endpoints must contain at least one endpoint".to_owned(),
        ));
    }
    let mut scheme = None;
    let mut port = None;
    for endpoint in &spec.server.endpoints {
        model::validate_host(&endpoint.host)?;
        if endpoint.port.is_some_and(|p| p == 0) {
            return Err(GatewayError::Validation("endpoint.port must be between 1 and 65535".to_owned()));
        }
        match scheme {
            None => scheme = Some(endpoint.scheme),
            Some(prev) if prev != endpoint.scheme => {
                return Err(GatewayError::Validation(
                    "all endpoints of an upstream must share the same scheme".to_owned(),
                ));
            }
            _ => {}
        }
        let effective = endpoint.port();
        match port {
            None => port = Some(effective),
            Some(prev) if prev != effective => {
                return Err(GatewayError::Validation(
                    "all endpoints of an upstream must share the same port".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validates a route against its owning upstream's protocol.
///
/// # Errors
///
/// [`GatewayError::Validation`] with the first problem found.
pub fn validate_route(spec: &Route, protocol: Protocol) -> Result<(), GatewayError> {
    let http = spec.match_config.http.as_ref();
    let grpc = spec.match_config.grpc.as_ref();
    match (http, grpc) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(GatewayError::Validation(
                "route.match must contain exactly one of 'http' or 'grpc'".to_owned(),
            ));
        }
        (Some(_), _) if protocol == Protocol::Grpc => {
            return Err(GatewayError::Validation(
                "route.match.http cannot be bound to a gRPC upstream".to_owned(),
            ));
        }
        (None, Some(_)) if protocol == Protocol::Http => {
            return Err(GatewayError::Validation(
                "route.match.grpc cannot be bound to an HTTP upstream".to_owned(),
            ));
        }
        _ => {}
    }
    if let Some(http) = http {
        if http.methods.is_empty() {
            return Err(GatewayError::Validation(
                "route.match.http.methods must contain at least one method".to_owned(),
            ));
        }
        if http.path.is_empty() {
            return Err(GatewayError::Validation(
                "route.match.http.path must not be empty".to_owned(),
            ));
        }
        if !http.path.starts_with('/') {
            return Err(GatewayError::Validation(format!(
                "route.match.http.path '{}' must start with '/'",
                http.path
            )));
        }
    }
    if let Some(grpc) = grpc
        && (grpc.service.is_empty() || grpc.method.is_empty()) {
            return Err(GatewayError::Validation(
                "route.match.grpc requires both 'service' and 'method'".to_owned(),
            ));
        }
    Ok(())
}

/// Rejects `allow_credentials` together with a wildcard origin (schema `if/then`).
///
/// # Errors
///
/// [`GatewayError::Validation`].
pub fn validate_cors(cors: &CorsConfig) -> Result<(), GatewayError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(GatewayError::Validation(
            "cors.allow_credentials cannot be combined with the wildcard origin '*'".to_owned(),
        ));
    }
    Ok(())
}

/// The endpoints of an upstream spec, for tests and the data plane.
#[must_use]
pub fn endpoints_of(spec: &Upstream) -> &[Endpoint] {
    &spec.server.endpoints
}

/// The HTTP match of a route spec, if any.
#[must_use]
pub fn http_match_of(spec: &Route) -> Option<&HttpMatch> {
    spec.match_config.http.as_ref()
}

/// The plugin kind implied by a GTS base type id.
#[must_use]
pub fn plugin_kind(type_id: &str) -> Option<PluginType> {
    PluginType::from_type_id(type_id)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
