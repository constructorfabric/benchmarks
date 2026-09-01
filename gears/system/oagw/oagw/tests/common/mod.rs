// Created: 2026-08-29 by Constructor Tech
//! Integration-test harness.
//!
//! Builds the gear's `Router` from the gear's services (control plane, plugin
//! registry, data plane) with fake `TenantResolverClient` / `CredStoreClientV1`
//! doubles, then issues `tower::ServiceExt::oneshot` requests. Every helper
//! takes the caller's tenant, so tests exercise the same `SecurityContext`
//! extraction the production handlers use.

#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef, SecretValue, SharingMode,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::OagwConfig;
use oagw::api::rest::handlers;
use oagw::api::rest::routes;
use oagw::domain::services::management::ControlPlaneService;
use oagw::infra::plugin::PluginRegistry;
use oagw::infra::proxy::service::DataPlaneService;
use oagw::infra::storage::Stores;

/// Caller tenant of every request in this suite.
#[must_use]
pub fn tenant() -> Uuid {
    Uuid::from_u128(0x6f1d_3a24_3b8e_4a6d_9d1f_0b6f_4c6b_1001)
}

/// Parent of [`tenant`].
#[must_use]
pub fn parent() -> Uuid {
    Uuid::from_u128(0x6f1d_3a24_3b8e_4a6f_9d1f_0b6f_4c6b_1002)
}

/// Root of the chain.
#[must_use]
pub fn root() -> Uuid {
    Uuid::from_u128(0x6f1d_3a24_3b8e_4a70_9d1f_0b6f_4c6b_1003)
}

/// Config that allows plaintext upstreams so `httpmock` (`127.0.0.1`, plain
/// `http`) can act as the upstream in proxy tests.
#[must_use]
pub fn test_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        // The test upstreams are httpmock servers on the loopback interface,
        // which the SSRF guard refuses; the run configuration opts out the
        // same way (see `config/e2e-local.yaml`).
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: true,
            allow_private_addresses: true,
        },
        ..OagwConfig::default()
    }
}

/// Fake tenant resolver: knows one `TENANT -> PARENT -> ROOT` chain and treats
/// every other tenant as its own root.
pub struct FakeTenantResolver {
    chain: Vec<Uuid>,
}

impl FakeTenantResolver {
    /// Resolver with the `TENANT -> PARENT -> ROOT` chain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chain: vec![parent(), root()],
        }
    }

    fn info(&self, id: TenantId) -> Option<TenantInfo> {
        let position = self.chain.iter().position(|candidate| *candidate == id.0);
        let parent_id = position.and_then(|index| self.chain.get(index + 1).copied());
        Some(TenantInfo {
            id,
            name: "tenant".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: parent_id.map(TenantId),
            self_managed: false,
        })
    }

    fn reference(id: TenantId) -> TenantRef {
        TenantRef {
            id,
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }
}

impl Default for FakeTenantResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TenantResolverClient for FakeTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.info(id)
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        let root = self.chain.last().copied().unwrap_or_else(Uuid::nil);
        self.info(TenantId(root))
            .ok_or(TenantResolverError::TenantNotFound {
                tenant_id: TenantId(root),
            })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids.iter().filter_map(|id| self.info(*id)).collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        if id.0 == tenant() {
            return Ok(GetAncestorsResponse {
                tenant: Self::reference(id),
                ancestors: self
                    .chain
                    .iter()
                    .map(|id| Self::reference(TenantId(*id)))
                    .collect(),
            });
        }
        if self.info(id).is_none() {
            return Err(TenantResolverError::TenantNotFound { tenant_id: id });
        }
        Ok(GetAncestorsResponse {
            tenant: Self::reference(id),
            ancestors: Vec::new(),
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        if self.info(id).is_none() {
            return Err(TenantResolverError::TenantNotFound { tenant_id: id });
        }
        Ok(GetDescendantsResponse {
            tenant: Self::reference(id),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        let Some(start) = self.chain.iter().position(|id| *id == descendant_id.0) else {
            return Ok(false);
        };
        Ok(self.chain[start..].contains(&ancestor_id.0))
    }
}

/// Fake credential store: in-memory `key -> value` map with a read counter.
pub struct FakeCredStore {
    secrets: std::sync::Mutex<HashMap<String, String>>,
    reads: AtomicUsize,
}

impl FakeCredStore {
    /// Store with the given secret set.
    #[must_use]
    pub fn new(secrets: &[(&str, &str)]) -> Self {
        Self {
            secrets: std::sync::Mutex::new(
                secrets
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            ),
            reads: AtomicUsize::new(0),
        }
    }

    /// Number of successful reads so far.
    #[must_use]
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CredStoreClientV1 for FakeCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        let found = {
            let guard = self.secrets.lock().expect("secrets lock");
            guard.get(key.as_ref()).map(|value| GetSecretResponse {
                value: SecretValue::from(value.clone()),
                id: Uuid::new_v4(),
                owner_tenant_id: TenantId(tenant()),
                sharing: SharingMode::Private,
                is_inherited: false,
                version: 1,
                secret_type: "opaque".to_owned(),
                expires_at: None,
            })
        };
        if found.is_some() {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        Ok(found)
    }
}

/// The full services bundle plus the router built from it.
pub struct Harness {
    router: Router,
    config: OagwConfig,
    data_plane: Arc<DataPlaneService>,
}

impl Harness {
    /// Build the router with the test config and the given fakes.
    pub fn new(config: OagwConfig, credstore: Option<Arc<FakeCredStore>>) -> Self {
        Self::with_plugins(config, credstore, |_registry| {})
    }

    /// Build the router with additional plugins installed on the registry.
    pub fn with_plugins(
        config: OagwConfig,
        credstore: Option<Arc<FakeCredStore>>,
        install: impl FnOnce(&mut PluginRegistry),
    ) -> Self {
        let resolver: Arc<dyn TenantResolverClient> = Arc::new(FakeTenantResolver::new());
        let store: Arc<dyn CredStoreClientV1> = credstore.map_or_else(
            || Arc::new(FakeCredStore::new(&[])) as Arc<dyn CredStoreClientV1>,
            |store| store,
        );
        let stores = Arc::new(Stores::new());
        let control_plane = Arc::new(ControlPlaneService::new(
            stores.upstreams(),
            stores.routes(),
            stores.plugins(),
        ));
        let mut registry = PluginRegistry::with_builtins(
            Some(store),
            std::time::Duration::from_secs(config.token_cache_ttl_secs),
            config.token_cache_capacity,
        );
        install(&mut registry);
        let plugins = Arc::new(registry);
        control_plane.set_plugin_catalog({
            let plugins = Arc::clone(&plugins);
            Arc::new(move |reference: &str| !plugins.missing(reference))
        });
        let data_plane = Arc::new(
            DataPlaneService::new(control_plane.clone(), plugins, Some(resolver), config)
                .expect("data plane"),
        );
        let services = Arc::new(handlers::Services {
            control_plane,
            data_plane: Arc::clone(&data_plane),
        });
        let router = routes::register(Router::new(), &OpenApiRegistryImpl::new(), services);
        Self {
            router,
            config,
            data_plane,
        }
    }

    /// The router.
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// The gear config in force.
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The data plane, for direct breaker manipulation in tests.
    pub fn data_plane(&self) -> &Arc<DataPlaneService> {
        &self.data_plane
    }

    /// Issue a request with the caller's tenant attached.
    pub async fn send(
        &self,
        method: &str,
        uri: &str,
        payload: Option<Value>,
        caller: Uuid,
    ) -> Response {
        send(&self.router, method, uri, payload, caller).await
    }

    /// Issue a pre-built request (custom headers, raw body) as `caller`.
    pub async fn send_request(&self, request: Request<Body>, caller: Uuid) -> Response {
        send_request(&self.router, request, caller).await
    }
}

/// Body of a response as `Bytes`.
pub async fn body_bytes(response: Response) -> Bytes {
    response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes()
}

/// Body of a response parsed as JSON.
pub async fn json_body(response: Response) -> Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "response is not json ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

/// Issue a request against a router with the caller's tenant attached.
pub async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    payload: Option<Value>,
    caller: Uuid,
) -> Response {
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).expect("method"))
        .uri(uri);
    if payload.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(
            payload.map_or_else(String::new, |value| value.to_string()),
        ))
        .expect("request");
    send_request(&router.clone(), request, caller).await
}

/// Issue a pre-built request with the caller's tenant attached.
pub async fn send_request(router: &Router, request: Request<Body>, caller: Uuid) -> Response {
    let mut request = request;
    request.extensions_mut().insert(security_for(caller));
    router.clone().oneshot(request).await.expect("response")
}

/// `GET` without a payload.
pub async fn get(router: &Router, uri: &str, caller: Uuid) -> Response {
    send(router, "GET", uri, None, caller).await
}

/// `POST` with a JSON payload.
pub async fn post(router: &Router, uri: &str, payload: Value, caller: Uuid) -> Response {
    send(router, "POST", uri, Some(payload), caller).await
}

/// `PUT` with a JSON payload.
pub async fn put(router: &Router, uri: &str, payload: Value, caller: Uuid) -> Response {
    send(router, "PUT", uri, Some(payload), caller).await
}

/// `DELETE`.
pub async fn delete(router: &Router, uri: &str, caller: Uuid) -> Response {
    send(router, "DELETE", uri, None, caller).await
}

/// Security context for a tenant; the subject is the tenant id so per-tenant
/// assertions stay stable.
#[must_use]
pub fn security_for(caller: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(caller)
        .subject_tenant_id(caller)
        .build()
        .expect("security context")
}

/// Value of a response header.
#[must_use]
pub fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Assert a status and return the response.
pub async fn expect_status(
    router: &Router,
    method: &str,
    uri: &str,
    payload: Option<Value>,
    caller: Uuid,
    expected: StatusCode,
) -> Response {
    let response = send(router, method, uri, payload, caller).await;
    assert_eq!(response.status(), expected, "{method} {uri}");
    response
}

/// Create an upstream pointing at an already running mock server.
pub async fn create_upstream(
    router: &Router,
    caller: Uuid,
    alias: &str,
    scheme: &str,
    host: &str,
    port: u16,
) -> Value {
    let payload = json!({
        "alias": alias,
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": scheme, "host": host, "port": port } ] },
    });
    let response = post(router, "/oagw/v1/upstreams", payload, caller).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await
}

/// Create a route for `upstream_id`.
pub async fn create_route(
    router: &Router,
    caller: Uuid,
    upstream_id: Uuid,
    path: &str,
    methods: &[&str],
) -> Value {
    let payload = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": methods, "path": path } },
    });
    let response = post(router, "/oagw/v1/routes", payload, caller).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await
}
