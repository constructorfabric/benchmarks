//! The concrete control-plane service.
//!
//! Enforces tenant scoping, alias derivation, reference integrity, plugin
//! resolution and route-match uniqueness on top of the in-memory
//! repositories, and walks the tenant hierarchy for alias resolution.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::{self, AliasDerivation};
use crate::domain::ids;
use crate::domain::model::{Endpoint, Plugin, PluginType, Route, Upstream};
use crate::domain::plugin::PluginRegistries;
use crate::domain::ratelimit;
use crate::domain::repo::{
    ControlPlaneError, ControlPlaneResult, PluginRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::service::{ControlPlaneService, MatchedRoute, ResolvedUpstream};
use crate::domain::validation;

/// Where a plugin is still bound.
#[derive(Default)]
struct PluginReferences {
    /// Upstreams binding it.
    upstreams: Vec<String>,
    /// Routes binding it.
    routes: Vec<String>,
}

impl PluginReferences {
    /// True when nothing references the plugin.
    fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }

    /// The documented `referenced_by` member.
    fn into_json(self) -> serde_json::Value {
        serde_json::json!({"upstreams": self.upstreams, "routes": self.routes})
    }

    /// The documented `detail` text.
    fn describe(&self) -> String {
        format!(
            "Plugin is referenced by {} upstream(s) and {} route(s)",
            self.upstreams.len(),
            self.routes.len()
        )
    }
}

/// Concrete control-plane implementation.
pub struct OagwControlPlane {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    registries: Arc<PluginRegistries>,
    tenant_client: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
    config: OagwConfig,
}

impl OagwControlPlane {
    /// Builds a control plane over the given repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        registries: Arc<PluginRegistries>,
        tenant_client: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
        config: OagwConfig,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            registries,
            tenant_client,
            config,
        }
    }

    /// The plugin registries, for the data plane.
    #[must_use]
    pub fn registries(&self) -> Arc<PluginRegistries> {
        Arc::clone(&self.registries)
    }

    /// The upstream repository, for list endpoints.
    #[must_use]
    pub fn upstream_repository(&self) -> Arc<dyn UpstreamRepository> {
        Arc::clone(&self.upstreams)
    }

    /// The route repository, for list endpoints.
    #[must_use]
    pub fn route_repository(&self) -> Arc<dyn RouteRepository> {
        Arc::clone(&self.routes)
    }

    /// The plugin repository, for list endpoints.
    #[must_use]
    pub fn plugin_repository(&self) -> Arc<dyn PluginRepository> {
        Arc::clone(&self.plugins)
    }

    async fn tenant_chain(&self, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant_id];
        let Some(client) = &self.tenant_client else {
            return chain;
        };
        let ctx = tenant_context(tenant_id);
        let options = tenant_resolver_sdk::GetAncestorsOptions {
            barrier_mode: tenant_resolver_sdk::BarrierMode::Ignore,
        };
        match client
            .get_ancestors(&ctx, tenant_resolver_sdk::TenantId(tenant_id), &options)
            .await
        {
            Ok(response) => {
                for ancestor in response.ancestors {
                    chain.push(ancestor.id.0);
                }
            }
            Err(error) => {
                tracing::debug!(error = %error, "tenant hierarchy unavailable; using single tenant");
            }
        }
        chain
    }

    fn validate_upstream_body(&self, tenant_id: Uuid, upstream: &Upstream) -> ControlPlaneResult<()> {
        validation::validate_upstream(upstream, self.config.allow_http_upstream)?;
        let mut refs = Vec::new();
        if let Some(auth) = &upstream.auth {
            refs.push(auth.auth_type.clone());
        }
        refs.extend(upstream.plugins.items.iter().map(|b| b.plugin_ref().to_owned()));
        self.validate_plugin_refs_sync(tenant_id, &refs)
    }

    /// Every reference must name an implementation in the registries, or a
    /// custom plugin stored by the calling tenant. Anything else — including a
    /// bare identifier, which no storage could resolve — is rejected.
    fn validate_plugin_refs_sync(&self, tenant_id: Uuid, refs: &[String]) -> ControlPlaneResult<()> {
        for plugin_ref in refs {
            if self.registries.resolves(plugin_ref).is_some() {
                continue;
            }
            if ids::split_gts_id(plugin_ref).is_none() {
                return Err(ControlPlaneError::Validation(format!(
                    "plugin reference '{plugin_ref}' is not a GTS identifier and no custom plugin \
                     storage is provisioned for it"
                )));
            }
            if self.plugins.get(tenant_id, plugin_ref).is_some() {
                continue;
            }
            return Err(ControlPlaneError::UnknownPlugin(plugin_ref.clone()));
        }
        Ok(())
    }

    fn plugin_references(&self, plugin_ref: &str) -> PluginReferences {
        let mut references = PluginReferences::default();
        for upstream in self.upstreams.list_all() {
            let bound = upstream.plugins.items.iter().any(|item| item.plugin_ref() == plugin_ref)
                || upstream
                    .auth
                    .as_ref()
                    .is_some_and(|auth| auth.auth_type == plugin_ref);
            if bound {
                references
                    .upstreams
                    .push(upstream.id.clone().unwrap_or_default());
            }
        }
        for route in self.routes.list_all() {
            if route.plugins.items.iter().any(|item| item.plugin_ref() == plugin_ref) {
                references.routes.push(route.id.clone().unwrap_or_default());
            }
        }
        references
    }
}

fn tenant_context(tenant_id: Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(tenant_id)
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap_or_else(|_| toolkit_security::SecurityContext::anonymous())
}

#[async_trait]
impl ControlPlaneService for OagwControlPlane {
    async fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
    ) -> ControlPlaneResult<Upstream> {
        normalize(&mut upstream);
        self.validate_upstream_body(tenant_id, &upstream)?;
        let derivation = alias::enforce_create(upstream.alias.as_deref(), &upstream.server.endpoints)
            .map_err(|(message, _)| ControlPlaneError::Validation(message))?;
        upstream.alias = Some(derivation.1.as_str().to_owned());
        let id = ids::upstream_id(Uuid::new_v4());
        upstream.id = Some(id);
        self.upstreams.insert(tenant_id, upstream.clone())?;
        Ok(upstream)
    }

    async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: &str,
        mut upstream: Upstream,
    ) -> ControlPlaneResult<Upstream> {
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .ok_or(ControlPlaneError::NotFound)?;
        normalize(&mut upstream);
        self.validate_upstream_body(tenant_id, &upstream)?;
        alias::enforce_update(
            existing.alias.as_deref().unwrap_or_default(),
            upstream.alias.as_deref(),
            &existing.server.endpoints,
            &upstream.server.endpoints,
        )
        .map_err(ControlPlaneError::Validation)?;
        upstream.id = Some(id.to_owned());
        upstream.alias.clone_from(&existing.alias);
        self.upstreams.update(tenant_id, upstream.clone())?;
        Ok(upstream)
    }

    async fn get_upstream(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<Upstream> {
        self.upstreams
            .get(tenant_id, id)
            .ok_or(ControlPlaneError::NotFound)
    }

    async fn delete_upstream(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<()> {
        let routes = self.routes.routes_for_upstream(id);
        for (owner, route) in routes {
            if owner == tenant_id {
                self.routes.delete(owner, route.id.as_deref().unwrap_or_default())?;
            }
        }
        self.upstreams.delete(tenant_id, id)?;
        Ok(())
    }

    async fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.upstreams.list(tenant_id)
    }

    async fn create_route(&self, tenant_id: Uuid, mut route: Route) -> ControlPlaneResult<Route> {
        validation::validate_route(&route)?;
        self.upstreams
            .get(tenant_id, &route.upstream_id)
            .ok_or(ControlPlaneError::UnknownUpstream)?;
        self.check_route_conflicts(tenant_id, &route, None)?;
        let refs: Vec<String> = route
            .plugins
            .items
            .iter()
            .map(|binding| binding.plugin_ref().to_owned())
            .collect();
        self.validate_plugin_refs_sync(tenant_id, &refs)?;
        let id = ids::route_id(Uuid::new_v4());
        route.id = Some(id);
        self.routes.insert(tenant_id, route.clone())?;
        Ok(route)
    }

    async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: &str,
        mut route: Route,
    ) -> ControlPlaneResult<Route> {
        let existing = self
            .routes
            .get(tenant_id, id)
            .ok_or(ControlPlaneError::NotFound)?;
        if route.upstream_id != existing.upstream_id {
            return Err(ControlPlaneError::Validation(
                "upstream_id is immutable on replace".to_owned(),
            ));
        }
        validation::validate_route(&route)?;
        self.upstreams
            .get(tenant_id, &route.upstream_id)
            .ok_or(ControlPlaneError::UnknownUpstream)?;
        self.check_route_conflicts(tenant_id, &route, Some(id))?;
        let refs: Vec<String> = route
            .plugins
            .items
            .iter()
            .map(|binding| binding.plugin_ref().to_owned())
            .collect();
        self.validate_plugin_refs_sync(tenant_id, &refs)?;
        route.id = Some(id.to_owned());
        self.routes.update(tenant_id, route.clone())?;
        Ok(route)
    }

    async fn get_route(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<Route> {
        self.routes
            .get(tenant_id, id)
            .ok_or(ControlPlaneError::NotFound)
    }

    async fn delete_route(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<()> {
        self.routes.delete(tenant_id, id)?;
        Ok(())
    }

    async fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.routes.list(tenant_id)
    }

    async fn create_plugin(
        &self,
        tenant_id: Uuid,
        name: &str,
        plugin_type: PluginType,
        source_code: &str,
    ) -> ControlPlaneResult<Plugin> {
        if name.trim().is_empty() {
            return Err(ControlPlaneError::Validation("name is required".to_owned()));
        }
        let id = ids::plugin_id(plugin_type.type_id(), Uuid::new_v4());
        let plugin = Plugin {
            id,
            name: name.to_owned(),
            plugin_type,
            source_code: source_code.to_owned(),
            tenant_id,
        };
        self.plugins.insert(plugin.clone())?;
        Ok(plugin)
    }

    async fn get_plugin(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<Plugin> {
        self.plugins
            .get(tenant_id, id)
            .ok_or(ControlPlaneError::NotFound)
    }

    async fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<()> {
        let references = self.plugin_references(id);
        if !references.is_empty() {
            let detail = references.describe();
            return Err(ControlPlaneError::InUse {
                referenced_by: references.into_json(),
                detail,
            });
        }
        self.plugins.delete(tenant_id, id)?;
        Ok(())
    }

    async fn list_plugins(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.plugins.list(tenant_id)
    }

    async fn resolve_alias(&self, tenant_id: Uuid, alias_value: &str) -> Option<ResolvedUpstream> {
        let chain = self.tenant_chain(tenant_id).await;
        for (depth, tenant) in chain.iter().enumerate() {
            if let Some(upstream) = self.upstreams.find_by_alias(*tenant, alias_value) {
                // A disabled upstream is still the closest match: it is
                // reported as unavailable rather than shadowed by an
                // ancestor's upstream.
                return Some(ResolvedUpstream {
                    tenant_id: *tenant,
                    upstream,
                    depth,
                });
            }
        }
        None
    }

    async fn match_route(
        &self,
        resolved: &ResolvedUpstream,
        method: &str,
        path_suffix: &str,
    ) -> Option<MatchedRoute> {
        let chain = self.tenant_chain(resolved.tenant_id).await;
        let upstream_id = resolved.upstream.id.clone()?;
        let candidates = self.routes.routes_for_upstream(&upstream_id);
        let mut best: Option<(usize, std::cmp::Reverse<usize>, Route, Uuid)> = None;
        for (owner, route) in candidates {
            if !route.enabled {
                continue;
            }
            let Some(http) = &route.match_rules.http else {
                continue;
            };
            if !http.methods.iter().any(|m| m.as_str() == method) {
                continue;
            }
            if match_suffix(http.path.as_str(), path_suffix, http.path_suffix_mode).is_none() {
                continue;
            }
            let Some(depth) = chain.iter().position(|t| *t == owner) else {
                continue;
            };
            let specificity = std::cmp::Reverse(http.path.len());
            if best
                .as_ref()
                .is_none_or(|b| (depth, specificity) < (b.0, b.1))
            {
                best = Some((depth, specificity, route, owner));
            }
        }
        let (_, _, route, owner) = best?;
        Some(MatchedRoute {
            route,
            tenant_id: owner,
            depth: 0,
        })
    }

    async fn effective_rate_limit(
        &self,
        tenant_id: Uuid,
        alias_value: &str,
        route: Option<&Route>,
    ) -> Option<crate::domain::model::RateLimit> {
        let chain = self.tenant_chain(tenant_id).await;
        let mut enforced = Vec::new();
        for tenant in chain.iter().skip(1) {
            if let Some(ancestor) = self.upstreams.find_by_alias(*tenant, alias_value)
                && let Some(limit) = ancestor.rate_limit
                && ratelimit::is_enforced(limit.sharing)
            {
                enforced.push(limit);
            }
        }
        ratelimit::merge_limits(
            self.upstreams
                .find_by_alias(tenant_id, alias_value)
                .and_then(|u| u.rate_limit),
            route.and_then(|r| r.rate_limit),
            enforced,
        )
    }

    async fn validate_plugin_refs(&self, tenant_id: Uuid, refs: &[String]) -> ControlPlaneResult<()> {
        self.validate_plugin_refs_sync(tenant_id, refs)
    }
}

impl OagwControlPlane {
    fn check_route_conflicts(
        &self,
        tenant_id: Uuid,
        route: &Route,
        excluding: Option<&str>,
    ) -> ControlPlaneResult<()> {
        let Some(http) = &route.match_rules.http else {
            return Ok(());
        };
        for (owner, existing) in self.routes.routes_for_upstream(&route.upstream_id) {
            if owner != tenant_id || !existing.enabled {
                continue;
            }
            if Some(existing.id.as_deref().unwrap_or_default()) == excluding {
                continue;
            }
            let Some(existing_http) = &existing.match_rules.http else {
                continue;
            };
            if existing_http.path != http.path {
                continue;
            }
            if existing_http
                .methods
                .iter()
                .any(|method| http.methods.contains(method))
            {
                return Err(ControlPlaneError::RouteConflict(format!(
                    "route path '{}' already accepts an overlapping method set on this upstream",
                    http.path
                )));
            }
        }
        Ok(())
    }
}

/// Normalises endpoint hosts: trailing dots are stripped and hosts lowercased,
/// so the stored upstream reflects the canonical hostname.
fn normalize(upstream: &mut Upstream) {
    for endpoint in &mut upstream.server.endpoints {
        endpoint.host = alias::normalize(&endpoint.host);
    }
}

/// Computes the path suffix a route accepts for a request suffix, honouring
/// the route's `path_suffix_mode`.
///
/// A non-empty return value is the remainder still to be appended to the
/// route path. For `disabled` a remainder means the request must be rejected,
/// but the route is still *selected* so the rejection is `400` rather than
/// `404`.
#[must_use]
pub fn match_suffix(
    route_path: &str,
    request_suffix: &str,
    mode: crate::domain::model::PathSuffixMode,
) -> Option<String> {
    use crate::domain::model::PathSuffixMode;

    let route = route_path.trim_end_matches('/');
    let request = if request_suffix.is_empty() { "/" } else { request_suffix };
    match mode {
        PathSuffixMode::Disabled => {
            if request == route {
                Some(String::new())
            } else if route != "/" && request.starts_with(&format!("{route}/")) {
                Some(request[route.len()..].to_owned())
            } else {
                None
            }
        }
        PathSuffixMode::Append => {
            if route == "/" {
                Some(request.to_owned())
            } else if request == route {
                Some(String::new())
            } else if request.starts_with(&format!("{route}/")) {
                Some(request[route.len()..].to_owned())
            } else {
                None
            }
        }
    }
}

/// The derived alias kind recorded for a stored upstream.
#[must_use]
pub fn derivation_of(upstream: &Upstream) -> AliasDerivation {
    alias::derive(&upstream.server.endpoints).unwrap_or_else(|| {
        AliasDerivation::Explicit(upstream.alias.clone().unwrap_or_default())
    })
}

/// True when the upstream's alias was derived from a multi-host common
/// suffix, which makes `X-OAGW-Target-Host` mandatory.
#[must_use]
pub fn is_common_suffix_alias(upstream: &Upstream) -> bool {
    derivation_of(upstream).is_common_suffix()
}

/// The endpoints an upstream declares, in configuration order.
#[must_use]
pub fn endpoints_of(upstream: &Upstream) -> &[Endpoint] {
    &upstream.server.endpoints
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::domain::model::PathSuffixMode;

    #[test]
    fn suffix_matching_append() {
        assert_eq!(
            match_suffix("/v1", "/v1/models", PathSuffixMode::Append),
            Some("/models".to_owned())
        );
        assert_eq!(match_suffix("/v1", "/v1", PathSuffixMode::Append), Some(String::new()));
        assert_eq!(match_suffix("/v1", "/v2", PathSuffixMode::Append), None);
    }

    #[test]
    fn suffix_matching_disabled() {
        assert_eq!(
            match_suffix("/v1/only", "/v1/only", PathSuffixMode::Disabled),
            Some(String::new())
        );
        assert_eq!(
            match_suffix("/v1/only", "/v1/only/extra", PathSuffixMode::Disabled),
            Some("/extra".to_owned())
        );
        assert_eq!(match_suffix("/v1/only", "/v2/only", PathSuffixMode::Disabled), None);
    }
}
