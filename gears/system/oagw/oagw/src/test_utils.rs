//! Test harness: an OAGW stack wired to in-process fakes.
//!
//! Behind the `test-utils` feature so the fakes never reach a release build.
//! Everything a test needs to exercise the real code paths — the same
//! Control Plane, Data Plane, plugin registries and router the gear wires in
//! production — with the credential store and the tenant hierarchy replaced by
//! doubles.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use credstore_sdk::{CredStoreClientV1, test_util::MockCredStoreClient};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::state::OagwState;
use crate::config::OagwConfig;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::ControlPlaneService;
use crate::domain::tenant::TenantDirectory;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::PluginRegistries;
use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
use crate::infra::proxy::DataPlaneService;
use crate::infra::proxy::connector::UpstreamConnector;
use crate::infra::rate_limit::RateLimiterRegistry;
use crate::infra::storage::InMemoryStore;

/// A tenant directory backed by a fixed parent map.
#[derive(Debug, Default)]
pub struct StaticTenantDirectory {
    parents: HashMap<Uuid, Uuid>,
}

impl StaticTenantDirectory {
    #[must_use]
    pub fn new(parents: Vec<(Uuid, Uuid)>) -> Self {
        Self {
            parents: parents.into_iter().collect(),
        }
    }
}

#[async_trait]
impl TenantDirectory for StaticTenantDirectory {
    async fn ancestor_chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant_id];
        let mut current = tenant_id;
        // Bounded walk: a cyclic fixture must not hang a test.
        while let Some(parent) = self.parents.get(&current) {
            if chain.contains(parent) {
                break;
            }
            chain.push(*parent);
            current = *parent;
        }
        chain
    }
}

/// A fully wired OAGW stack for tests.
pub struct TestHarness {
    pub state: Arc<OagwState>,
    pub store: Arc<InMemoryStore>,
    pub config: OagwConfig,
}

/// Builder for [`TestHarness`].
pub struct HarnessBuilder {
    config: OagwConfig,
    secrets: Vec<(String, String)>,
    parents: Vec<(Uuid, Uuid)>,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            config: OagwConfig {
                // Local fixtures are plaintext loopback servers, which the
                // production defaults exist to refuse.
                allow_http_upstream: true,
                proxy_timeout_secs: 2,
                connect_timeout_secs: 2,
                ssrf_policy: crate::config::SsrfPolicy {
                    enabled: false,
                    ..crate::config::SsrfPolicy::default()
                },
                ..OagwConfig::default()
            },
            secrets: Vec::new(),
            parents: Vec::new(),
        }
    }
}

impl HarnessBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the credential store double.
    #[must_use]
    pub fn with_secret(mut self, reference: &str, value: &str) -> Self {
        self.secrets
            .push((reference.to_owned(), value.to_owned()));
        self
    }

    /// Declare a `(child, parent)` edge in the tenant hierarchy.
    #[must_use]
    pub fn with_tenant_parent(mut self, child: Uuid, parent: Uuid) -> Self {
        self.parents.push((child, parent));
        self
    }

    /// Override the gear configuration.
    #[must_use]
    pub fn with_config(mut self, config: OagwConfig) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn build(self) -> TestHarness {
        let store = InMemoryStore::shared();
        let credstore: Arc<dyn CredStoreClientV1> =
            Arc::new(MockCredStoreClient::with_secrets(self.secrets));
        let tenants: Arc<dyn TenantDirectory> =
            Arc::new(StaticTenantDirectory::new(self.parents));

        let control = Arc::new(ControlPlaneService::new(
            Arc::clone(&store) as Arc<dyn UpstreamRepository>,
            Arc::clone(&store) as Arc<dyn RouteRepository>,
            Arc::clone(&store) as Arc<dyn PluginRepository>,
            tenants,
        ));
        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig::default(),
        ));
        let data_plane = Arc::new(DataPlaneService::new(
            Arc::clone(&control),
            UpstreamConnector::shared(&self.config),
            registries,
            Arc::clone(&store) as Arc<dyn PluginRepository>,
            Arc::new(RateLimiterRegistry::new()),
            Arc::new(OagwMetrics::from_global()),
            self.config.clone(),
        ));

        let state = Arc::new(OagwState {
            control,
            data_plane,
            store: Arc::clone(&store),
            authz: None,
            config: self.config.clone(),
        });

        TestHarness {
            state,
            store,
            config: self.config,
        }
    }
}

impl TestHarness {
    /// A router carrying the real OAGW routes, with `ctx` injected the way the
    /// api-gateway's auth middleware would.
    #[must_use]
    pub fn router(&self, ctx: SecurityContext) -> Router {
        let openapi = OpenApiRegistryImpl::new();
        let router = crate::api::rest::register_routes(Router::new(), &openapi, Arc::clone(&self.state));
        router.layer(axum::Extension(ctx))
    }
}

/// A `SecurityContext` for `tenant_id` with a stable subject.
#[must_use]
pub fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5eed))
        .subject_tenant_id(tenant_id)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}
