//! Control-plane service: the management API's business rules.
//!
//! Validation → derivation → uniqueness → persistence, always scoped to the
//! caller's tenant. Ancestor resources are invisible through this layer (404);
//! they are only reachable through the data plane's alias walk.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias;
use crate::domain::dto::{Cors, Endpoint, HttpMethod, Route, Upstream};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::layering::{self, EffectiveConfig};
use crate::domain::repo::{
    PluginKind, PluginRecord, PluginRepository, RouteRecord, RouteRepository, UpstreamRecord,
    UpstreamRepository,
};
use crate::domain::services::data_plane::ResolvedUpstream;
use crate::infra::authz::{scope_allows, Pep, TenantChain};

/// Tenant-scoped, validating control plane.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    pep: Option<Pep>,
    /// The executable named-plugin registry, consulted when a binding names a
    /// plugin that is not persisted (DESIGN §"Resolution Algorithm" step 3).
    named_plugins: Option<Arc<crate::infra::plugin::PluginRegistry>>,
}

impl ControlPlaneService {
    /// Builds a service over the given tables.
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        pep: Option<Pep>,
        named_plugins: Option<Arc<crate::infra::plugin::PluginRegistry>>,
    ) -> Self {
        Self { upstreams, routes, plugins, pep, named_plugins }
    }

    /// The upstream table.
    pub fn upstreams(&self) -> &Arc<dyn UpstreamRepository> {
        &self.upstreams
    }

    /// The route table.
    pub fn routes(&self) -> &Arc<dyn RouteRepository> {
        &self.routes
    }

    /// The plugin table.
    pub fn plugins(&self) -> &Arc<dyn PluginRepository> {
        &self.plugins
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Creates an upstream for `tenant_id`.
    pub async fn create_upstream(
        &self,
        tenant_id: Uuid,
        requested: Upstream,
    ) -> Result<Upstream, DomainError> {
        validate_upstream(&requested)?;
        self.validate_plugin_bindings(tenant_id, requested.plugins.as_ref()).await?;
        let derived = alias::enforce_alias_create(
            &requested.server.endpoints,
            requested.alias.as_deref(),
        )
        .map_err(|detail| DomainError::ValidationError { detail })?;

        let mut upstream = requested;
        if self.upstreams.get_by_alias(tenant_id, &derived).await?.is_some() {
            return Err(DomainError::Conflict {
                detail: format!("an upstream with alias `{derived}` already exists"),
            });
        }
        if let Some(cors) = &upstream.cors {
            validate_cors(cors)?;
        }
        let id = Uuid::new_v4().to_string();
        upstream.id = Some(id.clone());
        upstream.alias = Some(derived);
        self.upstreams
            .insert(UpstreamRecord { tenant_id, upstream: upstream.clone() })
            .await?;
        Ok(upstream)
    }

    /// Reads an upstream, 404 for foreign ids.
    pub async fn get_upstream(
        &self,
        tenant_id: Uuid,
        id: &str,
    ) -> Result<Upstream, DomainError> {
        self.upstreams
            .get_by_id(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                detail: format!("upstream `{id}` does not exist"),
            })
    }

    /// Lists the caller's upstreams, oldest first.
    pub async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list(tenant_id).await
    }

    /// Replaces an upstream. Full replacement: omitted optional fields clear.
    pub async fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: &str,
        requested: Upstream,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, id).await?;
        validate_upstream(&requested)?;
        self.validate_plugin_bindings(tenant_id, requested.plugins.as_ref()).await?;

        let new_endpoints = requested.server.endpoints.clone();
        if new_endpoints.is_empty() {
            return Err(DomainError::ValidationError {
                detail: "at least one endpoint is required".to_string(),
            });
        }
        for endpoint in &new_endpoints {
            alias::validate_endpoint_host(endpoint)
                .map_err(|detail| DomainError::ValidationError { detail })?;
        }

        let verdict = alias::enforce_alias_update_with(
            existing.alias_str(),
            &existing.server.endpoints,
            &new_endpoints,
            requested.alias.as_deref(),
        );
        match verdict {
            alias::AliasUpdateVerdict::Reject(detail) => {
                return Err(DomainError::ValidationError { detail })
            }
            alias::AliasUpdateVerdict::Keep | alias::AliasUpdateVerdict::KeepDerived => {}
        }

        if let Some(cors) = &requested.cors {
            validate_cors(cors)?;
        }

        let mut updated = requested;
        updated.id = existing.id.clone();
        updated.alias = existing.alias.clone();
        self.upstreams
            .update(UpstreamRecord { tenant_id, upstream: updated.clone() })
            .await?;
        Ok(updated)
    }

    /// Deletes an upstream.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError> {
        self.get_upstream(tenant_id, id).await?;
        let routes = self.routes.list(tenant_id).await?;
        if routes.iter().any(|r| r.upstream_id == id) {
            return Err(DomainError::Conflict {
                detail: format!("upstream `{id}` still has routes bound to it"),
            });
        }
        if !self.upstreams.delete(tenant_id, id).await? {
            return Err(DomainError::NotFound {
                detail: format!("upstream `{id}` does not exist"),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Creates a route against an upstream the caller owns.
    pub async fn create_route(
        &self,
        tenant_id: Uuid,
        requested: Route,
    ) -> Result<Route, DomainError> {
        validate_route(&requested)?;
        self.validate_plugin_bindings(tenant_id, requested.plugins.as_ref()).await?;
        if self.upstreams.get_by_id(tenant_id, &requested.upstream_id).await?.is_none() {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "upstream `{}` does not exist for this tenant",
                    requested.upstream_id
                ),
            });
        }
        let mut route = requested;
        route.id = Some(Uuid::new_v4().to_string());
        self.routes.insert(RouteRecord { tenant_id, route: route.clone() }).await?;
        Ok(route)
    }

    /// Reads a route, 404 for foreign ids.
    pub async fn get_route(&self, tenant_id: Uuid, id: &str) -> Result<Route, DomainError> {
        self.routes.get_by_id(tenant_id, id).await?.ok_or_else(|| DomainError::NotFound {
            detail: format!("route `{id}` does not exist"),
        })
    }

    /// Lists the caller's routes, oldest first.
    pub async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        self.routes.list(tenant_id).await
    }

    /// Replaces a route. `upstream_id` is immutable and re-sent unchanged.
    pub async fn update_route(
        &self,
        tenant_id: Uuid,
        id: &str,
        requested: Route,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id).await?;
        validate_route(&requested)?;
        self.validate_plugin_bindings(tenant_id, requested.plugins.as_ref()).await?;
        let mut route = requested;
        route.id = existing.id.clone();
        route.upstream_id = existing.upstream_id.clone();
        self.routes
            .update(RouteRecord { tenant_id, route: route.clone() })
            .await?;
        Ok(route)
    }

    /// Deletes a route.
    pub async fn delete_route(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError> {
        self.get_route(tenant_id, id).await?;
        if !self.routes.delete(tenant_id, id).await? {
            return Err(DomainError::NotFound {
                detail: format!("route `{id}` does not exist"),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Creates a plugin definition.
    pub async fn create_plugin(
        &self,
        tenant_id: Uuid,
        kind: PluginKind,
        name: &str,
        config: serde_json::Value,
        source: &str,
    ) -> Result<PluginRecord, DomainError> {
        if name.trim().is_empty() {
            return Err(DomainError::ValidationError {
                detail: "plugin `name` is required".to_string(),
            });
        }
        let record = PluginRecord {
            id: Uuid::new_v4(),
            tenant_id,
            kind,
            name: name.trim().to_string(),
            config,
            source: source.to_string(),
        };
        self.plugins.insert(record.clone()).await?;
        Ok(record)
    }

    /// Reads a plugin, 404 for foreign ids.
    pub async fn get_plugin(
        &self,
        tenant_id: Uuid,
        id: &str,
    ) -> Result<PluginRecord, DomainError> {
        self.plugins.get(tenant_id, id).await?.ok_or_else(|| {
            DomainError::NotFound {
                detail: format!("plugin `{id}` does not exist"),
            }
        })
    }

    /// Lists the caller's plugins.
    pub async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<PluginRecord>, DomainError> {
        self.plugins.list(tenant_id).await
    }

    /// Deletes an unreferenced plugin; a delete that is refused because the
    /// plugin is still bound reports 409 with the referencing resources.
    pub async fn delete_plugin(
        &self,
        tenant_id: Uuid,
        chain: &TenantChain,
        id: &str,
    ) -> Result<(), DomainError> {
        let record = self.get_plugin(tenant_id, id).await?;
        let reference = record.id.to_string();
        let referenced_by = self.plugins.referenced_by(chain.entries(), &reference).await?;
        if !referenced_by.upstreams.is_empty() || !referenced_by.routes.is_empty() {
            return Err(DomainError::PluginInUse {
                plugin_id: record.id.to_string(),
                referenced_by,
            });
        }
        if !self.plugins.delete(tenant_id, id).await? {
            return Err(DomainError::NotFound {
                detail: format!("plugin `{id}` does not exist"),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Data-plane resolution
    // ------------------------------------------------------------------

    /// Resolves an alias down the tenant chain, closest enabled upstream wins.
    pub async fn resolve_alias(
        &self,
        chain: &TenantChain,
        requested_alias: &str,
    ) -> Result<Option<ResolvedUpstream>, DomainError> {
        let normalised = alias::normalise_alias(requested_alias);
        let found = self
            .upstreams
            .find_in_chain(chain.entries(), &normalised)
            .await?;
        Ok(found.map(|(owner_tenant_id, upstream)| ResolvedUpstream {
            owner_tenant_id,
            upstream,
            chain: chain.clone(),
        }))
    }

    /// The routes visible to a proxy request, descendant-first.
    pub async fn proxy_routes(
        &self,
        chain: &TenantChain,
    ) -> Result<Vec<(Uuid, Route)>, DomainError> {
        let records = self.routes.list_in_chain(chain.entries()).await?;
        Ok(records.into_iter().map(|r| (r.tenant_id, r.route)).collect())
    }

    /// Folds the effective configuration for a resolved upstream and route.
    pub async fn effective_config(
        &self,
        chain: &TenantChain,
        resolved: &ResolvedUpstream,
        route: Option<&Route>,
    ) -> Result<EffectiveConfig, DomainError> {
        let mut config = layering::fold_route(&resolved.upstream, resolved.owner_tenant_id, route);
        // Ancestor-enforced layers constrain the descendant's configuration.
        for tenant in chain.ancestors() {
            if let Some(upstream) = self
                .upstreams
                .get_by_alias(*tenant, &resolved.upstream.alias_str())
                .await?
            {
                layering::merge_ancestor_enforced(&mut config, &upstream);
            }
        }
        Ok(config)
    }

    /// The id an upstream resource is addressed by on the wire.
    pub fn upstream_gts_id(upstream: &Upstream) -> String {
        match &upstream.id {
            Some(uuid_part) => gts::anonymous_gts_id(gts::UPSTREAM_TYPE, uuid_part),
            None => gts::UPSTREAM_TYPE.to_string(),
        }
    }

    /// The id a route resource is addressed by on the wire.
    pub fn route_gts_id(route: &Route) -> String {
        match &route.id {
            Some(uuid_part) => gts::anonymous_gts_id(gts::ROUTE_TYPE, uuid_part),
            None => gts::ROUTE_TYPE.to_string(),
        }
    }

    /// Validates every plugin binding of a resource: a named reference must be
    /// one this gear can execute through its registry (the catalog-only
    /// identifiers are unbindable) and a UUID must name a plugin the tenant
    /// recorded (DESIGN §"Resolution Algorithm" steps 2 and 3).
    async fn validate_plugin_bindings(
        &self,
        tenant_id: Uuid,
        plugins: Option<&crate::domain::dto::PluginSet>,
    ) -> Result<(), DomainError> {
        let Some(plugins) = plugins else {
            return Ok(());
        };
        for reference in &plugins.items {
            if crate::domain::gts_helpers::is_catalog_only_plugin(reference) {
                return Err(DomainError::ValidationError {
                    detail: format!(
                        "plugin `{reference}` is catalogued but has no executable implementation and cannot be bound"
                    ),
                });
            }
            match crate::domain::gts_helpers::plugin_uuid(reference) {
                Some(uuid) => {
                    let known = self.plugins.get(tenant_id, &uuid.to_string()).await?.is_some()
                        || self.plugins.get(tenant_id, reference).await?.is_some();
                    if !known {
                        return Err(DomainError::ValidationError {
                            detail: format!("plugin `{reference}` does not exist"),
                        });
                    }
                }
                None => {
                    if !crate::domain::gts_helpers::is_gts_id(reference) {
                        return Err(DomainError::ValidationError {
                            detail: format!("plugin `{reference}` is not a plugin identifier"),
                        });
                    }
                    let executable = self
                        .named_plugins
                        .as_ref()
                        .is_some_and(|registry| registry.resolves(reference));
                    if !executable {
                        return Err(DomainError::ValidationError {
                            detail: format!("plugin `{reference}` is not a plugin this gear can execute"),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// Validates the endpoints and top-level shape of an upstream.
fn validate_upstream(upstream: &Upstream) -> Result<(), DomainError> {
    if upstream.server.endpoints.is_empty() {
        return Err(DomainError::ValidationError {
            detail: "at least one endpoint is required".to_string(),
        });
    }
    for endpoint in &upstream.server.endpoints {
        alias::validate_endpoint_host(endpoint)
            .map_err(|detail| DomainError::ValidationError { detail })?;
    }
    if upstream.server.endpoints.len() > 1 {
        // A pool must agree on protocol, scheme and port.
        let ports: Vec<u16> = upstream
            .server
            .endpoints
            .iter()
            .map(Endpoint::effective_port)
            .collect();
        if ports.iter().any(|p| *p != ports[0]) {
            return Err(DomainError::ValidationError {
                detail: "all endpoints of a pool must use the same port".to_string(),
            });
        }
        let schemes: Vec<String> = upstream
            .server
            .endpoints
            .iter()
            .map(|e| e.scheme.as_str().to_string())
            .collect();
        if schemes.iter().any(|s| *s != schemes[0]) {
            return Err(DomainError::ValidationError {
                detail: "all endpoints of a pool must use the same scheme".to_string(),
            });
        }
    }
    for tag in &upstream.tags {
        if !is_valid_tag(tag) {
            return Err(DomainError::ValidationError {
                detail: format!("tag `{tag}` must match ^[a-z0-9_-]+$"),
            });
        }
    }
    Ok(())
}

/// Validates the match rule of a route.
fn validate_route(route: &Route) -> Result<(), DomainError> {
    if route.upstream_id.trim().is_empty() {
        return Err(DomainError::ValidationError {
            detail: "`upstream_id` is required".to_string(),
        });
    }
    route
        .match_rule
        .exactly_one()
        .map_err(|detail| DomainError::ValidationError { detail })?;
    if let Some(http) = &route.match_rule.http {
        if http.methods.is_empty() {
            return Err(DomainError::ValidationError {
                detail: "at least one HTTP method is required".to_string(),
            });
        }
        for method in &http.methods {
            if HttpMethod::parse(method).is_none() {
                return Err(DomainError::ValidationError {
                    detail: format!("`{method}` is not a supported HTTP method"),
                });
            }
        }
        if http.path.trim().is_empty() {
            return Err(DomainError::ValidationError {
                detail: "route path is required".to_string(),
            });
        }
    }
    if let Some(grpc) = &route.match_rule.grpc {
        if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
            return Err(DomainError::ValidationError {
                detail: "both gRPC service and method are required".to_string(),
            });
        }
    }
    for tag in &route.tags {
        if !is_valid_tag(tag) {
            return Err(DomainError::ValidationError {
                detail: format!("tag `{tag}` must match ^[a-z0-9_-]+$"),
            });
        }
    }
    Ok(())
}

/// Validates a CORS block, including the credentials/`*` exclusivity rule.
pub fn validate_cors(cors: &Cors) -> Result<(), DomainError> {
    layering::is_overridable(cors.sharing); // no-op; keeps the import used
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::ValidationError {
            detail:
                "allow_credentials requires specific origins and cannot be combined with `*`"
                    .to_string(),
        });
    }
    for method in &cors.allowed_methods {
        if !crate::domain::cors::method_allowed(&Cors { enabled: true, allowed_methods: vec![method.clone()], ..Cors::default() }, method) {
            return Err(DomainError::ValidationError {
                detail: format!("`{method}` is not a CORS-allowed method"),
            });
        }
    }
    Ok(())
}

/// Whether a tag matches `^[a-z0-9_-]+$`.
fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl ControlPlaneService {
    /// Wraps a PDP evaluation for a management action.
    pub async fn authorize(
        &self,
        ctx: &toolkit_security::SecurityContext,
        resource: &'static str,
        action: &str,
        resource_id: Option<Uuid>,
    ) -> Result<toolkit_security::AccessScope, DomainError> {
        match &self.pep {
            Some(pep) => pep.access_scope(ctx, resource, action, resource_id).await,
            None => Ok(toolkit_security::AccessScope::allow_all()),
        }
    }

    /// Whether the scope admits a row owned by `tenant_id`.
    pub fn scope_allows(scope: &toolkit_security::AccessScope, tenant_id: Uuid) -> bool {
        scope_allows(scope, tenant_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::storage::Storage;

    fn service() -> ControlPlaneService {
        let storage = Storage::new();
        ControlPlaneService::new(
            storage.upstreams.clone(),
            storage.routes.clone(),
            storage.plugins.clone(),
            None,
            None,
        )
    }

    fn tenant() -> Uuid {
        Uuid::from_u128(1000)
    }

    fn hostname_upstream() -> Upstream {
        Upstream {
            server: crate::domain::dto::Server {
                endpoints: vec![Endpoint {
                    scheme: crate::domain::dto::EndpointScheme::Https,
                    host: "api.openai.com".into(),
                    port: 443,
                }],
            },
            ..Upstream::default()
        }
    }

    /// An upstream whose endpoints are IP literals, so an explicit alias is
    /// accepted rather than derived.
    fn ip_upstream(alias: &str) -> Upstream {
        Upstream {
            alias: Some(alias.into()),
            server: crate::domain::dto::Server {
                endpoints: vec![Endpoint {
                    scheme: crate::domain::dto::EndpointScheme::Https,
                    host: "10.0.0.5".into(),
                    port: 443,
                }],
            },
            ..Upstream::default()
        }
    }

    #[tokio::test]
    async fn create_derives_the_alias_and_returns_the_id() {
        let svc = service();
        let upstream = svc.create_upstream(tenant(), hostname_upstream()).await.unwrap();
        assert_eq!(upstream.alias_str(), "api.openai.com");
        assert!(upstream.id.is_some());
    }

    #[tokio::test]
    async fn create_rejects_a_duplicate_alias() {
        let svc = service();
        svc.create_upstream(tenant(), hostname_upstream()).await.unwrap();
        let err = svc.create_upstream(tenant(), hostname_upstream()).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }));
    }

    #[tokio::test]
    async fn create_requires_an_explicit_alias_for_ip_endpoints() {
        let svc = service();
        let ip_upstream = Upstream {
            server: crate::domain::dto::Server {
                endpoints: vec![Endpoint {
                    scheme: crate::domain::dto::EndpointScheme::Https,
                    host: "10.0.1.1".into(),
                    port: 443,
                }],
            },
            ..Upstream::default()
        };
        assert!(svc.create_upstream(tenant(), ip_upstream.clone()).await.is_err());
        let with_alias = Upstream { alias: Some("my-service".into()), ..ip_upstream };
        let created = svc.create_upstream(tenant(), with_alias).await.unwrap();
        assert_eq!(created.alias_str(), "my-service");
    }

    #[tokio::test]
    async fn get_of_a_foreign_upstream_is_a_404() {
        let svc = service();
        let other = Uuid::from_u128(2000);
        let upstream = svc.create_upstream(tenant(), hostname_upstream()).await.unwrap();
        let id = upstream.id.clone().unwrap();
        assert!(svc.get_upstream(other, &id).await.is_err());
        let err = svc.get_upstream(other, &id).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound { .. }));
    }

    #[tokio::test]
    async fn put_replaces_and_clears_omitted_optionals() {
        let svc = service();
        let mut created = hostname_upstream();
        created.tags = vec!["llm".into()];
        created.rate_limit = Some(crate::domain::dto::RateLimit {
            sustained: crate::domain::dto::Sustained {
                rate: 10,
                window: crate::domain::dto::RateWindow::Second,
            },
            ..crate::domain::dto::RateLimit::default()
        });
        let created = svc.create_upstream(tenant(), created).await.unwrap();
        let id = created.id.clone().unwrap();

        let replacement = Upstream {
            id: Some(id.clone()),
            alias: Some("api.openai.com".into()),
            ..hostname_upstream()
        };
        let updated = svc.update_upstream(tenant(), &id, replacement).await.unwrap();
        assert!(updated.tags.is_empty(), "omitted optionals are cleared");
        assert!(updated.rate_limit.is_none());
        assert_eq!(updated.alias_str(), "api.openai.com");
    }

    #[tokio::test]
    async fn put_rejects_an_alias_change() {
        let svc = service();
        let created = svc.create_upstream(tenant(), hostname_upstream()).await.unwrap();
        let id = created.id.clone().unwrap();
        let changed = Upstream {
            alias: Some("other.example".into()),
            ..hostname_upstream()
        };
        let err = svc.update_upstream(tenant(), &id, changed).await.unwrap_err();
        assert!(matches!(err, DomainError::ValidationError { .. }));
    }

    #[tokio::test]
    async fn delete_removes_the_upstream() {
        let svc = service();
        let created = svc.create_upstream(tenant(), hostname_upstream()).await.unwrap();
        let id = created.id.clone().unwrap();
        svc.delete_upstream(tenant(), &id).await.unwrap();
        assert!(svc.get_upstream(tenant(), &id).await.is_err());
    }

    #[tokio::test]
    async fn route_crud_round_trips() {
        let svc = service();
        let upstream = svc.create_upstream(tenant(), hostname_upstream()).await.unwrap();
        let upstream_id = upstream.id.clone().unwrap();
        let route = Route {
            upstream_id: upstream_id.clone(),
            match_rule: crate::domain::dto::MatchRule {
                http: Some(crate::domain::dto::HttpMatch {
                    methods: vec!["GET".to_string()],
                    path: "/v1".into(),
                    ..crate::domain::dto::HttpMatch::default()
                }),
                grpc: None,
            },
            ..Route::default()
        };
        let created = svc.create_route(tenant(), route).await.unwrap();
        let route_id = created.id.clone().unwrap();
        assert_eq!(created.upstream_id, upstream_id);

        let fetched = svc.get_route(tenant(), &route_id).await.unwrap();
        assert_eq!(fetched.id, Some(route_id.clone()));

        svc.delete_route(tenant(), &route_id).await.unwrap();
        assert!(svc.get_route(tenant(), &route_id).await.is_err());
    }

    #[tokio::test]
    async fn a_route_may_not_reference_a_foreign_upstream() {
        let svc = service();
        let other = Uuid::from_u128(3000);
        let route = Route {
            upstream_id: Uuid::new_v4().to_string(),
            match_rule: crate::domain::dto::MatchRule {
                http: Some(crate::domain::dto::HttpMatch {
                    methods: vec!["GET".to_string()],
                    path: "/v1".into(),
                    ..crate::domain::dto::HttpMatch::default()
                }),
                grpc: None,
            },
            ..Route::default()
        };
        let err = svc.create_route(other, route).await.unwrap_err();
        assert!(matches!(err, DomainError::ValidationError { .. }));
    }

    #[tokio::test]
    async fn resolve_alias_walks_the_chain_and_prefers_the_closest() {
        let svc = service();
        let root = Uuid::from_u128(1);
        let leaf = Uuid::from_u128(2);
        // IP endpoints carry an explicit alias, which is what makes shadowing
        // possible: the same alias owned by two tenants of one chain.
        svc.create_upstream(root, ip_upstream("shared.example")).await.unwrap();
        svc.create_upstream(leaf, ip_upstream("shared.example")).await.unwrap();

        let chain = TenantChain::from_entries(vec![leaf, root]);
        let resolved = svc.resolve_alias(&chain, "shared.example").await.unwrap().unwrap();
        assert_eq!(resolved.owner_tenant_id, leaf, "closest match wins");

        let chain = TenantChain::from_entries(vec![leaf, root]);
        // An ancestor-only alias is visible on the data plane.
        svc.create_upstream(root, ip_upstream("root-only.example")).await.unwrap();
        let resolved = svc.resolve_alias(&chain, "root-only.example").await.unwrap().unwrap();
        assert_eq!(resolved.owner_tenant_id, root);
    }
}
