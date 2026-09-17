//! In-memory storage backends and the assembled [`Services`] handle.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use toolkit_http::HttpClient;
use uuid::Uuid;

use tenant_resolver_sdk::{
    BarrierMode, GetAncestorsOptions, TenantId, TenantResolverClient,
};
use credstore_sdk::CredStoreClientV1;

use crate::config::OagwConfig;
use crate::domain::control_plane::Caller;
use crate::domain::model::{PluginDef, Route, Upstream};
use crate::error::OagwError;

use super::plugins::AuthPluginRegistry;
use super::rate_limiter::RateLimiter;

/// Provides the tenant chain (self + ancestors, closest first) for alias and
/// configuration resolution.
#[async_trait]
pub trait TenantChainProvider: Send + Sync {
    /// Returns `[self, parent, ..., root]` (closest first). At minimum the
    /// caller's own tenant id.
    async fn chain(&self, caller: &Caller) -> Result<Vec<Uuid>, OagwError>;
}

/// Tenant chain provider backed by the tenant resolver.
pub struct TenantResolverChain {
    resolver: Arc<dyn TenantResolverClient>,
}

impl TenantResolverChain {
    #[must_use]
    pub fn new(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl TenantChainProvider for TenantResolverChain {
    async fn chain(&self, caller: &Caller) -> Result<Vec<Uuid>, OagwError> {
        let ctx = caller.security_context();
        let resp = self
            .resolver
            .get_ancestors(
                &ctx,
                TenantId(caller.tenant_id),
                &GetAncestorsOptions {
                    barrier_mode: BarrierMode::Respect,
                },
            )
            .await
            .map_err(|e| OagwError::Internal {
                detail: format!("tenant chain lookup failed: {e}"),
            })?;
        let mut chain = vec![caller.tenant_id];
        chain.extend(resp.ancestors.iter().map(|a| a.id.0));
        Ok(chain)
    }
}

/// A tenant chain that only knows the single tenant (unit tests / single
/// tenant deployments).
pub struct SingleTenantChain(pub Uuid);

#[async_trait]
impl TenantChainProvider for SingleTenantChain {
    async fn chain(&self, caller: &Caller) -> Result<Vec<Uuid>, OagwError> {
        let mut chain = vec![caller.tenant_id];
        // A nil secondary tenant means "no ancestors" (single-tenant mode).
        if !self.0.is_nil() && self.0 != caller.tenant_id {
            chain.push(self.0);
        }
        Ok(chain)
    }
}

/// Assembled OAGW services: control-plane stores + data-plane machinery.
pub struct Services {
    pub config: OagwConfig,
    /// Tenant chain source (resolver-backed in production, single-tenant in
    /// tests).
    pub tenant_chain: Arc<dyn TenantChainProvider>,
    /// Credential store client (auth plugin secrets).
    pub credstore: Arc<dyn CredStoreClientV1>,
    /// Data-plane HTTP client (upstream forwarding).
    pub outgoing_client: HttpClient,
    /// Data-plane rate limiter.
    pub limiter: RateLimiter,
    /// Resolvable builtin auth plugins.
    pub auth_plugins: AuthPluginRegistry,
    /// Round-robin cursor for multi-endpoint pools.
    pub round_robin: AtomicUsize,

    // Stores (in-memory; a relational backend is future work).
    pub upstreams: DashMap<Uuid, Upstream>,
    /// `(tenant_id, alias)` -> upstream id.
    pub upstream_by_alias: DashMap<(Uuid, String), Uuid>,
    pub routes: DashMap<Uuid, Route>,
    pub plugins: DashMap<Uuid, PluginDef>,
    /// `gts_identifier` -> plugin id (for reference checks).
    pub plugin_by_gts: DashMap<String, Uuid>,
}

impl Services {
    /// Blank service for tests: hand-assemble stores and override the
    /// provided defaults.
    #[must_use]
    pub fn empty(config: OagwConfig, tenant: Uuid) -> Self {
        Self::blank(config, tenant).build()
    }

    /// Start assembling a blank service.
    #[must_use]
    pub fn blank(config: OagwConfig, tenant: Uuid) -> Blank {
        Blank {
            config,
            tenant,
            credstore: None,
            outgoing: None,
        }
    }
}

/// Builder for test services.
pub struct Blank {
    config: OagwConfig,
    tenant: Uuid,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    outgoing: Option<HttpClient>,
}

impl Blank {
    #[must_use]
    pub fn with_credstore(mut self, credstore: Arc<dyn CredStoreClientV1>) -> Self {
        self.credstore = Some(credstore);
        self
    }

    #[must_use]
    pub fn with_outgoing(mut self, client: HttpClient) -> Self {
        self.outgoing = Some(client);
        self
    }

    pub fn build(self) -> Services {
        let outgoing = self.outgoing.unwrap_or_else(|| {
            toolkit_http::HttpClientBuilder::with_config(toolkit_http::HttpClientConfig::proxy())
                .build()
                .expect("test http client builds")
        });
        let credstore: Arc<dyn CredStoreClientV1> = match self.credstore {
            Some(c) => c,
            None => Arc::new(NoopCredStore),
        };
        Services {
            config: self.config,
            tenant_chain: Arc::new(SingleTenantChain(self.tenant)),
            auth_plugins: AuthPluginRegistry::with_builtins(
                credstore.clone(),
                toolkit_http::HttpClientConfig::proxy(),
                Default::default(),
            ),
            credstore,
            outgoing_client: outgoing,
            limiter: RateLimiter::new(),
            round_robin: AtomicUsize::new(0),
            upstreams: DashMap::new(),
            upstream_by_alias: DashMap::new(),
            routes: DashMap::new(),
            plugins: DashMap::new(),
            plugin_by_gts: DashMap::new(),
        }
    }
}

/// Minimal no-op credstore client for tests that don't exercise auth.
pub struct NoopCredStore;

#[async_trait]
impl CredStoreClientV1 for NoopCredStore {
    async fn get(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        Ok(None)
    }
}
