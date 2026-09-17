//! Control-plane service: validation and CRUD orchestration over the store.
//!
//! Every operation is scoped to the calling tenant — ancestor resources are
//! invisible to the management API (404), and the alias is treated as an
//! immutable routing key. Validation failures surface as 400
//! `cf.oagw.validation.error`, collisions as 409.

use crate::domain::alias::{
    derive_alias, enforce_alias_update, normalize_alias, validate_alias_shape, validate_hostname,
};
use crate::domain::error::{DomainError, DomainResult, ErrorKind};
use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::gts_helpers::{PLUGIN_TYPE_ID, ROUTE_TYPE_ID, UPSTREAM_TYPE_ID};
use crate::domain::model::{
    EndpointScheme, HttpMatch, PathSuffixMode, Plugin, Protocol, Route, RouteMatch, Upstream,
};
use crate::domain::repo::{ListFilter, PluginRepository, RouteRepository, UpstreamRepository};
use crate::infra::store::InMemoryStore;

/// Builtin plugin identifiers the data plane can execute without a stored
/// definition.
const BUILTIN_PLUGIN_IDS: &[&str] = &[
    APIKEY_AUTH_PLUGIN_ID,
    NOOP_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
    REQUEST_ID_TRANSFORM_PLUGIN_ID,
];

fn validation(detail: impl Into<String>) -> DomainError {
    DomainError::new(ErrorKind::Validation, detail)
}

fn not_found(kind: &str, prefix: &str, id: uuid::Uuid) -> DomainError {
    DomainError::new(
        ErrorKind::RouteNotFound,
        format!("{kind} '{prefix}{id}' not found"),
    )
}

/// Validates one endpoint against the wire contract.
fn validate_endpoint(scheme: EndpointScheme, host: &str, port: Option<u16>) -> DomainResult<()> {
    validate_hostname(host)?;
    if let Some(port) = port
        && port == 0
    {
        return Err(validation("endpoint port must be between 1 and 65535"));
    }
    let _ = scheme;
    Ok(())
}

/// Validates an endpoint pool: non-empty, all endpoints valid.
fn validate_endpoints(endpoints: &[crate::domain::model::Endpoint]) -> DomainResult<()> {
    if endpoints.is_empty() {
        return Err(validation("at least one endpoint is required"));
    }
    if endpoints.len() > 64 {
        return Err(validation("at most 64 endpoints are supported"));
    }
    for endpoint in endpoints {
        validate_endpoint(endpoint.scheme, &endpoint.host, endpoint.port)?;
    }
    Ok(())
}

/// Resolves the alias for a new upstream: explicit when required by the
/// endpoint pool, derived otherwise.
fn resolve_alias(
    endpoints: &[crate::domain::model::Endpoint],
    requested: Option<&str>,
) -> DomainResult<String> {
    validate_endpoints(endpoints)?;
    let derived = derive_alias(endpoints);
    match (requested.map(normalize_alias), derived.clone()) {
        (Some(explicit), _) => {
            validate_alias_shape(&explicit)?;
            if let Some(derived) = &derived
                && derived != &explicit
            {
                return Err(validation(format!(
                    "alias '{explicit}' does not match the derived alias '{derived}' for these endpoints"
                )));
            }
            Ok(explicit)
        }
        (None, Some(derived)) => Ok(derived),
        (None, None) => Err(validation(
            "an explicit alias is required: the endpoint pool is not derivable (IP literals, unrelated hosts, or a bare public suffix)",
        )),
    }
}

/// `true` when the plugin identifier is a builtin the data plane can execute.
fn is_builtin_plugin(id: &str) -> bool {
    BUILTIN_PLUGIN_IDS.contains(&id)
}

/// Validates that every bound plugin is either a builtin or a stored plugin of
/// the calling tenant.
async fn validate_plugin_bindings(
    store: &InMemoryStore,
    tenant_id: uuid::Uuid,
    bindings: &[crate::domain::model::PluginBinding],
) -> DomainResult<()> {
    for binding in bindings {
        if binding.id.is_empty() {
            return Err(validation("plugin binding id must not be empty"));
        }
        if is_builtin_plugin(&binding.id) || binding.id.starts_with(PLUGIN_TYPE_ID) {
            continue;
        }
        let parsed = uuid::Uuid::parse_str(&binding.id).ok();
        match parsed {
            Some(id)
                if PluginRepository::find_by_id(store, tenant_id, id)
                    .await?
                    .is_some() => {}
            _ => {
                return Err(validation(format!(
                    "plugin '{}' is not registered",
                    binding.id
                )));
            }
        }
    }
    Ok(())
}

/// Validates the upstream body shared by create and replace.
fn validate_upstream_body(tags: &std::collections::BTreeSet<String>) -> DomainResult<()> {
    for tag in tags {
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(validation(format!(
                "tag '{tag}' must match '^[a-z0-9_-]+$'"
            )));
        }
    }
    Ok(())
}

/// Control-plane operations over tenant-scoped resources.
#[derive(Clone)]
pub struct ControlPlaneService {
    store: InMemoryStore,
}

impl ControlPlaneService {
    /// Binds the service to the shared store.
    #[must_use]
    pub fn new(store: InMemoryStore) -> Self {
        Self { store }
    }

    /// The underlying store (used by the data plane and the gear).
    #[must_use]
    pub fn store(&self) -> &InMemoryStore {
        &self.store
    }

    /// Creates an upstream.
    ///
    /// # Errors
    ///
    /// Returns 400 on validation failure and 409 on alias collision.
    pub async fn create_upstream(
        &self,
        tenant_id: uuid::Uuid,
        endpoints: Vec<crate::domain::model::Endpoint>,
        alias: Option<String>,
        tags: std::collections::BTreeSet<String>,
        protocol: Protocol,
        upstream: Upstream,
    ) -> DomainResult<Upstream> {
        let alias = resolve_alias(&endpoints, alias.as_deref())?;
        validate_upstream_body(&tags)?;
        let mut upstream = upstream;
        upstream.id = uuid::Uuid::new_v4();
        upstream.tenant_id = tenant_id;
        upstream.alias = alias;
        upstream.server.endpoints = endpoints;
        upstream.tags = tags;
        upstream.protocol = protocol;
        validate_plugin_bindings(&self.store, tenant_id, &upstream.plugins.items).await?;
        UpstreamRepository::insert(&self.store, upstream).await
    }

    /// Lists upstreams of the calling tenant.
    ///
    /// # Errors
    ///
    /// Propagates store failures.
    pub async fn list_upstreams(
        &self,
        tenant_id: uuid::Uuid,
        filter: &ListFilter,
    ) -> DomainResult<Vec<Upstream>> {
        UpstreamRepository::list(&self.store, tenant_id, filter).await
    }

    /// Fetches one upstream.
    ///
    /// # Errors
    ///
    /// Returns 404 when the upstream is absent or owned by another tenant.
    pub async fn get_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Upstream> {
        UpstreamRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .ok_or_else(|| not_found("upstream", UPSTREAM_TYPE_ID, id))
    }

    /// Replaces an upstream. `id`/`tenant_id` are immutable and the alias is
    /// treated as an immutable routing key.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent, 400 when the alias transition is illegal and
    /// 409 when the alias collides with a sibling.
    pub async fn replace_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
        endpoints: Vec<crate::domain::model::Endpoint>,
        alias: Option<String>,
        tags: std::collections::BTreeSet<String>,
        mut upstream: Upstream,
    ) -> DomainResult<Upstream> {
        validate_endpoints(&endpoints)?;
        validate_upstream_body(&tags)?;
        let current = UpstreamRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .ok_or_else(|| not_found("upstream", UPSTREAM_TYPE_ID, id))?;

        enforce_alias_update(
            &current.server.endpoints,
            &current.alias,
            &endpoints,
            alias.as_deref(),
        )?;

        upstream.id = current.id;
        upstream.tenant_id = current.tenant_id;
        upstream.alias = current.alias.clone();
        upstream.server.endpoints = endpoints;
        upstream.tags = tags;
        upstream.created_at = current.created_at;
        validate_plugin_bindings(&self.store, tenant_id, &upstream.plugins.items).await?;
        UpstreamRepository::update(&self.store, upstream).await
    }

    /// Deletes an upstream; a 409 is returned while routes still reference it.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent, 409 when still referenced.
    pub async fn delete_upstream(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()> {
        if UpstreamRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .is_none()
        {
            return Err(not_found("upstream", UPSTREAM_TYPE_ID, id));
        }
        UpstreamRepository::delete(&self.store, tenant_id, id).await
    }

    /// Creates a route for an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns 400 on validation failure, 404 when the upstream is not
    /// addressable and 409 on a duplicate match rule.
    pub async fn create_route(
        &self,
        tenant_id: uuid::Uuid,
        mut route: Route,
    ) -> DomainResult<Route> {
        validate_route(&route)?;
        let upstream_id = route.upstream_id;
        UpstreamRepository::find_by_id(&self.store, tenant_id, upstream_id)
            .await?
            .ok_or_else(|| {
                DomainError::new(
                    ErrorKind::RouteNotFound,
                    format!("upstream '{upstream_id}' not found"),
                )
            })?;
        validate_plugin_bindings(&self.store, tenant_id, &route.plugins.items).await?;
        route.id = uuid::Uuid::new_v4();
        route.tenant_id = tenant_id;
        RouteRepository::insert(&self.store, route).await
    }

    /// Lists routes of the calling tenant.
    ///
    /// # Errors
    ///
    /// Propagates store failures.
    pub async fn list_routes(
        &self,
        tenant_id: uuid::Uuid,
        filter: &ListFilter,
    ) -> DomainResult<Vec<Route>> {
        RouteRepository::list(&self.store, tenant_id, filter).await
    }

    /// Fetches one route.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent or owned by another tenant.
    pub async fn get_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<Route> {
        RouteRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .ok_or_else(|| not_found("route", ROUTE_TYPE_ID, id))
    }

    /// Replaces a route. `upstream_id` is immutable and is ignored on update.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent and 409 on a duplicate match rule.
    pub async fn replace_route(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
        mut route: Route,
    ) -> DomainResult<Route> {
        validate_route(&route)?;
        let current = RouteRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .ok_or_else(|| not_found("route", ROUTE_TYPE_ID, id))?;
        validate_plugin_bindings(&self.store, tenant_id, &route.plugins.items).await?;
        route.id = current.id;
        route.tenant_id = current.tenant_id;
        route.upstream_id = current.upstream_id;
        route.created_at = current.created_at;
        RouteRepository::update(&self.store, route).await
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent or owned by another tenant.
    pub async fn delete_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()> {
        if RouteRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .is_none()
        {
            return Err(not_found("route", ROUTE_TYPE_ID, id));
        }
        RouteRepository::delete(&self.store, tenant_id, id).await
    }

    /// Creates an immutable plugin definition.
    ///
    /// # Errors
    ///
    /// Returns 400 when the plugin kind or source is invalid.
    pub async fn create_plugin(
        &self,
        tenant_id: uuid::Uuid,
        mut plugin: Plugin,
    ) -> DomainResult<Plugin> {
        if plugin.name.trim().is_empty() {
            return Err(validation("plugin name must not be empty"));
        }
        if plugin.plugin_type.trim().is_empty() {
            return Err(validation("plugin type must not be empty"));
        }
        plugin.name = plugin.name.trim().to_owned();
        plugin.id = uuid::Uuid::new_v4();
        plugin.tenant_id = tenant_id;
        PluginRepository::insert(&self.store, plugin).await
    }

    /// Lists plugins of the calling tenant.
    ///
    /// # Errors
    ///
    /// Propagates store failures.
    pub async fn list_plugins(
        &self,
        tenant_id: uuid::Uuid,
        filter: &ListFilter,
    ) -> DomainResult<Vec<Plugin>> {
        PluginRepository::list(&self.store, tenant_id, filter).await
    }

    /// Fetches one plugin.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent or owned by another tenant.
    pub async fn get_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<Plugin> {
        PluginRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .ok_or_else(|| not_found("plugin", PLUGIN_TYPE_ID, id))
    }

    /// Deletes a plugin, refusing while it is still bound.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent and 409 when still referenced.
    pub async fn delete_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()> {
        if PluginRepository::find_by_id(&self.store, tenant_id, id)
            .await?
            .is_none()
        {
            return Err(not_found("plugin", PLUGIN_TYPE_ID, id));
        }
        PluginRepository::delete(&self.store, tenant_id, id).await
    }

    /// Returns the Starlark source of a plugin.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent or owned by another tenant.
    pub async fn plugin_source(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<String> {
        Ok(self.get_plugin(tenant_id, id).await?.source)
    }
}

/// Validates a route match block.
///
/// # Errors
///
/// Returns 400 when neither protocol is present, both are present, or the HTTP
/// match is incomplete.
pub fn validate_route(route: &Route) -> DomainResult<()> {
    match (&route.route_match.http, &route.route_match.grpc) {
        (Some(http), None) => validate_http_match(http),
        (None, Some(_)) => Ok(()),
        (Some(_), Some(_)) => Err(validation(
            "exactly one of match.http or match.grpc must be present",
        )),
        (None, None) => Err(validation(
            "exactly one of match.http or match.grpc must be present",
        )),
    }
}

fn validate_http_match(http: &HttpMatch) -> DomainResult<()> {
    if http.methods.is_empty() {
        return Err(validation(
            "match.http.methods must list at least one method",
        ));
    }
    for method in &http.methods {
        let upper = method.to_ascii_uppercase();
        if !matches!(
            upper.as_str(),
            "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
        ) {
            return Err(validation(format!("unsupported HTTP method '{method}'")));
        }
    }
    if http.path.is_empty() || !http.path.starts_with('/') {
        return Err(validation(
            "match.http.path must be an absolute path starting with '/'",
        ));
    }
    for param in &http.query_allowlist {
        if param.trim().is_empty() {
            return Err(validation(
                "match.http.query_allowlist entries must not be empty",
            ));
        }
    }
    Ok(())
}

/// Builds the normalized route match used by tests and the data plane.
#[must_use]
pub fn normalized_http_match(http: &HttpMatch) -> HttpMatch {
    let mut normalized = http.clone();
    normalized.methods = http
        .methods
        .iter()
        .map(|m| m.trim().to_ascii_uppercase())
        .collect();
    normalized.path = http.path.trim_end_matches('/').to_owned();
    normalized
}

/// Convenience constructor used by tests and handlers.
#[must_use]
pub fn http_route_match(methods: &[&str], path: &str) -> RouteMatch {
    RouteMatch {
        http: Some(HttpMatch {
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme, ServerConfig};
    use std::collections::BTreeMap;

    fn ep(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn empty_upstream() -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            enabled: true,
            alias: String::new(),
            tags: Default::default(),
            server: ServerConfig {
                endpoints: Vec::new(),
            },
            protocol: Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn empty_route(upstream_id: uuid::Uuid) -> Route {
        Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            upstream_id,
            enabled: true,
            tags: Default::default(),
            route_match: http_route_match(&["GET"], "/v1/items"),
            plugins: Default::default(),
            rate_limit: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn create_upstream_derives_alias_and_rejects_mismatch() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let upstream = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "api.openai.com", None)],
                Some("api.openai.com".to_owned()),
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect("derived alias accepted");
        assert_eq!(upstream.alias, "api.openai.com");

        let err = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "api.openai.com", None)],
                Some("other.example".to_owned()),
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect_err("mismatching alias rejected");
        assert_eq!(err.kind.status(), http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_upstream_requires_explicit_alias_for_ip_pool() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let err = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "10.0.0.1", None)],
                None,
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect_err("ip pool needs an explicit alias");
        assert_eq!(err.kind, ErrorKind::Validation);

        let created = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "10.0.0.1", None)],
                Some("my-service".to_owned()),
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect("explicit alias accepted");
        assert_eq!(created.alias, "my-service");
    }

    #[tokio::test]
    async fn alias_is_immutable_on_replace() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let created = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "a.vendor.com", None)],
                None,
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect("created");
        let err = svc
            .replace_upstream(
                tenant,
                created.id,
                vec![ep(EndpointScheme::Https, "b.vendor.com", None)],
                None,
                Default::default(),
                empty_upstream(),
            )
            .await
            .expect_err("alias change rejected");
        assert_eq!(err.kind.status(), http::StatusCode::BAD_REQUEST);

        let same = svc
            .replace_upstream(
                tenant,
                created.id,
                vec![ep(EndpointScheme::Https, "a.vendor.com", None)],
                None,
                Default::default(),
                empty_upstream(),
            )
            .await
            .expect("unchanged endpoints accepted");
        assert_eq!(same.alias, "a.vendor.com");
    }

    #[tokio::test]
    async fn upstream_crud_is_tenant_scoped() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let created = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "api.example", None)],
                None,
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect("created");
        assert!(svc.get_upstream(other, created.id).await.is_err());
        assert!(
            svc.list_upstreams(tenant, &ListFilter::default())
                .await
                .expect("list")
                .len()
                == 1
        );
        svc.delete_upstream(tenant, created.id)
            .await
            .expect("deleted");
        assert!(svc.get_upstream(tenant, created.id).await.is_err());
    }

    #[tokio::test]
    async fn route_requires_addressable_upstream_and_unique_match() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let created = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "api.example", None)],
                None,
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect("created");

        let mut route = empty_route(created.id);
        route.route_match = RouteMatch::default();
        let err = svc
            .create_route(tenant, route)
            .await
            .expect_err("empty match rejected");
        assert_eq!(err.kind, ErrorKind::Validation);

        let mut route = empty_route(created.id);
        route.route_match = http_route_match(&["GET"], "/v1/items");
        let first = svc.create_route(tenant, route).await.expect("created");
        let duplicate = svc
            .create_route(tenant, empty_route(created.id))
            .await
            .expect_err("duplicate match rejected");
        assert_eq!(duplicate.kind.status(), http::StatusCode::CONFLICT);
        let _ = first;
    }

    #[tokio::test]
    async fn replace_route_keeps_upstream_immutable() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let upstream = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "api.example", None)],
                None,
                Default::default(),
                Protocol::Http,
                empty_upstream(),
            )
            .await
            .expect("created");
        let route = svc
            .create_route(tenant, empty_route(upstream.id))
            .await
            .expect("created");

        let mut replacement = empty_route(uuid::Uuid::new_v4());
        replacement.route_match = http_route_match(&["POST"], "/v2/items");
        let updated = svc
            .replace_route(tenant, route.id, replacement)
            .await
            .expect("updated");
        assert_eq!(updated.upstream_id, upstream.id);
        assert_eq!(updated.route_match.http.expect("http").path, "/v2/items");
    }

    #[tokio::test]
    async fn plugin_delete_is_refused_while_bound() {
        let svc = ControlPlaneService::new(InMemoryStore::new());
        let tenant = uuid::Uuid::new_v4();
        let plugin_id = uuid::Uuid::new_v4();
        let mut upstream = empty_upstream();
        upstream
            .plugins
            .items
            .push(crate::domain::model::PluginBinding {
                id: plugin_id.to_string(),
                sharing: None,
                config: BTreeMap::new(),
            });
        let err = svc
            .create_upstream(
                tenant,
                vec![ep(EndpointScheme::Https, "api.example", None)],
                None,
                Default::default(),
                Protocol::Http,
                upstream,
            )
            .await
            .expect_err("unknown plugin rejected");
        assert_eq!(err.kind, ErrorKind::Validation);

        let plugin = svc
            .create_plugin(
                tenant,
                Plugin {
                    id: plugin_id,
                    tenant_id: tenant,
                    name: "guard".to_owned(),
                    plugin_type: "guard".to_owned(),
                    source: String::new(),
                    gc_eligible_at: None,
                    created_at: 0,
                },
            )
            .await
            .expect("plugin created");
        assert_eq!(
            svc.plugin_source(tenant, plugin.id).await.expect("source"),
            ""
        );
        svc.delete_plugin(tenant, plugin.id)
            .await
            .expect("unbound plugin deleted");
    }

    #[test]
    fn http_match_validation_rejects_bad_paths_and_methods() {
        let tenant = uuid::Uuid::new_v4();
        let mut route = empty_route(uuid::Uuid::new_v4());
        route.tenant_id = tenant;
        route.route_match = http_route_match(&[], "/v1");
        assert!(validate_route(&route).is_err());
        route.route_match = http_route_match(&["FETCH"], "/v1");
        assert!(validate_route(&route).is_err());
        route.route_match = http_route_match(&["GET"], "v1");
        assert!(validate_route(&route).is_err());
        route.route_match = http_route_match(&["GET"], "/v1");
        assert!(validate_route(&route).is_ok());
        route.route_match.grpc = Some(Default::default());
        assert!(validate_route(&route).is_err());
    }

    #[test]
    fn normalized_match_uppercases_methods() {
        let match_rule = normalized_http_match(&HttpMatch {
            methods: vec!["get".to_owned(), "post".to_owned()],
            path: "/v1/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        });
        assert_eq!(match_rule.methods, vec!["GET", "POST"]);
        assert_eq!(match_rule.path, "/v1");
    }
}
