//! Unit tests for the `ControlPlaneService` facade, over a local in-memory
//! repository stub so the domain layer keeps its no-infrastructure boundary.

use std::sync::Mutex;
use uuid::Uuid;

use super::*;
use crate::domain::dto::{
    CorsConfig, Endpoint, EndpointScheme, HttpMethod, HttpMatch, MatchConfig, PathSuffixMode,
    ServerConfig,
};
use crate::domain::error::DomainError;

/// The store one test's repositories are built over. Each test gets its own,
/// because the harness runs tests in parallel and a shared static would let
/// one test's fixture erase another's records.
type Shared = std::sync::Arc<Mutex<Store>>;

#[derive(Default)]
struct StubRepo {
    store: Shared,
}

impl StubRepo {
    /// The locked store of this repository.
    fn storage(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().expect("store lock")
    }
}

#[derive(Default)]
struct Store {
    upstreams: Vec<UpstreamRecord>,
    routes: Vec<RouteRecord>,
    plugins: Vec<Plugin>,
}

fn conflict(detail: &str) -> DomainError {
    DomainError::Conflict { detail: detail.to_owned(), referenced_by: None }
}

fn not_found() -> DomainError {
    DomainError::NotFound { resource_type: "record" }
}

impl crate::domain::repo::UpstreamRepository for StubRepo {
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRecord, DomainError> {
        self.storage()
            .upstreams
            .iter()
            .find(|r| r.upstream.tenant_id == tenant_id && r.upstream.id == id)
            .cloned()
            .ok_or(not_found())
    }
    fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<UpstreamRecord, DomainError> {
        self.storage()
            .upstreams
            .iter()
            .find(|r| r.upstream.tenant_id == tenant_id && r.upstream.alias == alias)
            .cloned()
            .ok_or(not_found())
    }
    fn list(&self, tenant_id: Uuid) -> Result<Vec<UpstreamRecord>, DomainError> {
        Ok(self.storage()
            .upstreams
            .iter()
            .filter(|r| r.upstream.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    fn create(&self, tenant_id: Uuid, record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        let mut store = self.storage();
        if store
            .upstreams
            .iter()
            .any(|r| r.upstream.tenant_id == tenant_id && r.upstream.alias == record.upstream.alias)
        {
            return Err(conflict("duplicate alias"));
        }
        store.upstreams.push(record.clone());
        Ok(record)
    }
    fn replace(&self, tenant_id: Uuid, record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        let mut store = self.storage();
        let existing = store
            .upstreams
            .iter_mut()
            .find(|r| r.upstream.tenant_id == tenant_id && r.upstream.id == record.upstream.id)
            .ok_or_else(not_found)?;
        *existing = record.clone();
        Ok(record)
    }
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut store = self.storage();
        let Some(index) = store
            .upstreams
            .iter()
            .position(|r| r.upstream.tenant_id == tenant_id && r.upstream.id == id)
        else {
            return Err(not_found());
        };
        store.upstreams.remove(index);
        store.routes.retain(|r| r.route.upstream_id != id);
        Ok(())
    }
}

impl crate::domain::repo::RouteRepository for StubRepo {
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRecord, DomainError> {
        self.storage()
            .routes
            .iter()
            .find(|r| r.route.tenant_id == tenant_id && r.route.id == id)
            .cloned()
            .ok_or(not_found())
    }
    fn list(&self, tenant_id: Uuid) -> Result<Vec<RouteRecord>, DomainError> {
        Ok(self.storage()
            .routes
            .iter()
            .filter(|r| r.route.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    fn list_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<RouteRecord>, DomainError> {
        Ok(self.storage()
            .routes
            .iter()
            .filter(|r| r.route.tenant_id == tenant_id && r.route.upstream_id == upstream_id)
            .cloned()
            .collect())
    }
    fn create(&self, _tenant_id: Uuid, record: RouteRecord) -> Result<RouteRecord, DomainError> {
        let mut store = self.storage();
        if store.routes.iter().any(|r| r.route.id == record.route.id) {
            return Err(conflict("duplicate route"));
        }
        store.routes.push(record.clone());
        Ok(record)
    }
    fn replace(&self, tenant_id: Uuid, record: RouteRecord) -> Result<RouteRecord, DomainError> {
        let mut store = self.storage();
        let existing = store
            .routes
            .iter_mut()
            .find(|r| r.route.tenant_id == tenant_id && r.route.id == record.route.id)
            .ok_or_else(not_found)?;
        *existing = record.clone();
        Ok(record)
    }
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut store = self.storage();
        let Some(index) = store
            .routes
            .iter()
            .position(|r| r.route.tenant_id == tenant_id && r.route.id == id)
        else {
            return Err(not_found());
        };
        store.routes.remove(index);
        Ok(())
    }
}

impl crate::domain::repo::PluginRepository for StubRepo {
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.storage()
            .plugins
            .iter()
            .find(|p| p.tenant_id == tenant_id && p.id == id)
            .cloned()
            .ok_or(not_found())
    }
    fn get_by_name(&self, tenant_id: Uuid, name: &str) -> Result<Plugin, DomainError> {
        self.storage()
            .plugins
            .iter()
            .find(|p| p.tenant_id == tenant_id && p.name == name)
            .cloned()
            .ok_or(not_found())
    }
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        Ok(self.storage()
            .plugins
            .iter()
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    fn create(&self, tenant_id: Uuid, plugin: Plugin) -> Result<Plugin, DomainError> {
        let mut store = self.storage();
        if store.plugins.iter().any(|p| p.tenant_id == tenant_id && p.name == plugin.name) {
            return Err(conflict("duplicate name"));
        }
        store.plugins.push(plugin.clone());
        Ok(plugin)
    }
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut store = self.storage();
        let Some(index) =
            store.plugins.iter().position(|p| p.tenant_id == tenant_id && p.id == id)
        else {
            return Err(not_found());
        };
        store.plugins.remove(index);
        Ok(())
    }
    fn touch(&self, tenant_id: Uuid, id: Uuid, last_used_at: String) -> Result<(), DomainError> {
        let mut store = self.storage();
        match store
            .plugins
            .iter_mut()
            .find(|p| p.tenant_id == tenant_id && p.id == id)
        {
            Some(plugin) => {
                plugin.last_used_at = Some(last_used_at);
                Ok(())
            }
            None => Err(not_found()),
        }
    }
}

/// A service over one private store. Every test builds its own, so the
/// parallel harness cannot bleed records across tests.
fn service() -> ControlPlaneServiceImpl {
    let store: Shared = std::sync::Arc::default();
    ControlPlaneServiceImpl::new(
        std::sync::Arc::new(StubRepo { store: std::sync::Arc::clone(&store) }),
        std::sync::Arc::new(StubRepo { store: std::sync::Arc::clone(&store) }),
        std::sync::Arc::new(StubRepo { store }),
        true,
    )
}

fn upstream(tenant_id: Uuid) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id,
        alias: "api.vendor.com".to_owned(),
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: "api.vendor.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn route(tenant_id: Uuid, upstream_id: Uuid) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id,
        upstream_id,
        match_type: RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

#[test]
fn creating_an_upstream_returns_the_validated_record() {
    let tenant = Uuid::new_v4();
    let service = service();
    let created = service.create_upstream(tenant, upstream(tenant)).expect("created");
    assert_eq!(created.server.endpoints[0].host, "api.vendor.com");
    assert!(service.get_upstream(tenant, created.id).is_ok());
}

#[test]
fn a_second_upstream_with_the_same_alias_is_a_conflict() {
    let tenant = Uuid::new_v4();
    let service = service();
    service.create_upstream(tenant, upstream(tenant)).expect("first");
    let error = service.create_upstream(tenant, upstream(tenant)).expect_err("conflict");
    assert!(error.to_string().contains("unique key"), "`{error}` names the uniqueness rule");
}

#[test]
fn a_foreign_tenant_key_is_not_found() {
    let tenant = Uuid::new_v4();
    let service = service();
    let created = service.create_upstream(tenant, upstream(tenant)).expect("created");
    let other = Uuid::new_v4();
    let error = service.get_upstream(other, created.id).expect_err("not found");
    assert!(error.is_not_found());
    assert!(service.get_upstream_by_alias(other, "api.vendor.com").is_err());
}

#[test]
fn deleting_an_upstream_cascades_to_its_routes() {
    let tenant = Uuid::new_v4();
    let service = service();
    let created = service.create_upstream(tenant, upstream(tenant)).expect("created");
    let route = service.create_route(tenant, route(tenant, created.id)).expect("route");
    service.delete_upstream(tenant, created.id).expect("deleted");
    assert!(service.get_route(tenant, route.id).is_err(), "cascade removed the route");
    assert!(service.list_routes_for_upstream(tenant, created.id).expect("list").is_empty());
}

#[test]
fn resolve_proxy_target_matches_method_and_path() {
    let tenant = Uuid::new_v4();
    let service = service();
    let created = service.create_upstream(tenant, upstream(tenant)).expect("created");
    service.create_route(tenant, route(tenant, created.id)).expect("route");

    let resolved =
        service.resolve_proxy_target(tenant, "api.vendor.com", "GET", "/v1").expect("resolved");
    assert_eq!(resolved.route_id, service.list_routes(tenant).expect("routes")[0].id);
    assert_eq!(resolved.match_type, RouteMatchType::Http);
    assert!(resolved.upstream.auth.is_none());

    let error = service
        .resolve_proxy_target(tenant, "api.vendor.com", "POST", "/v1")
        .expect_err("method not matched");
    assert!(matches!(error, DomainError::RouteNotFound { .. }));
}

#[test]
fn resolve_proxy_target_rejects_a_disabled_upstream() {
    let tenant = Uuid::new_v4();
    let service = service();
    let mut disabled = upstream(tenant);
    disabled.enabled = false;
    service.create_upstream(tenant, disabled).expect("created");
    let error = service
        .resolve_proxy_target(tenant, "api.vendor.com", "GET", "/v1")
        .expect_err("disabled upstream");
    assert!(matches!(error, DomainError::LinkUnavailable { .. }));
}

#[test]
fn a_cors_block_with_wildcard_credentials_is_rejected_on_write() {
    let tenant = Uuid::new_v4();
    let mut bad = upstream(tenant);
    bad.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["*".to_owned()]),
        allow_credentials: true,
        ..CorsConfig::default()
    });
    let error = service().create_upstream(tenant, bad).expect_err("rejected");
    assert!(error.to_string().contains("allow_credentials with wildcard origin"));
}
