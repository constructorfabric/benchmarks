//! Shared gear state handed to the REST handlers and the data plane.

use std::sync::Arc;

use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin::SecretResolver;
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::infra::store::InMemoryStore;

/// Resolves the tenant chain (descendant → root) used for alias shadowing.
#[async_trait::async_trait]
pub trait TenantChain: Send + Sync {
    /// Returns the calling tenant followed by its ancestors.
    ///
    /// # Errors
    ///
    /// Returns a 503 `LinkUnavailable` problem when the hierarchy cannot be
    /// walked.
    async fn chain(&self, ctx: &SecurityContext) -> Result<Vec<uuid::Uuid>, DomainError>;
}

/// [`TenantChain`] backed by the tenant-resolver gear.
pub struct ResolverTenantChain {
    resolver: Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
}

impl std::fmt::Debug for ResolverTenantChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolverTenantChain")
    }
}

impl ResolverTenantChain {
    /// Wraps a tenant-resolver client.
    #[must_use]
    pub fn new(resolver: Arc<dyn tenant_resolver_sdk::TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl TenantChain for ResolverTenantChain {
    async fn chain(&self, ctx: &SecurityContext) -> Result<Vec<uuid::Uuid>, DomainError> {
        let options = tenant_resolver_sdk::GetAncestorsOptions {
            barrier_mode: tenant_resolver_sdk::BarrierMode::Respect,
        };
        let response = self
            .resolver
            .get_ancestors(
                ctx,
                credstore_sdk::TenantId(ctx.subject_tenant_id()),
                &options,
            )
            .await
            .map_err(|_| {
                DomainError::new(
                    ErrorKind::LinkUnavailable,
                    "tenant hierarchy is unavailable",
                )
            })?;
        let mut chain = vec![response.tenant.id.0];
        chain.extend(response.ancestors.iter().map(|tenant| tenant.id.0));
        Ok(chain)
    }
}

/// Single-tenant fallback used when no tenant-resolver is wired.
#[derive(Debug, Default, Clone, Copy)]
pub struct StaticTenantChain;

#[async_trait::async_trait]
impl TenantChain for StaticTenantChain {
    async fn chain(&self, ctx: &SecurityContext) -> Result<Vec<uuid::Uuid>, DomainError> {
        Ok(vec![ctx.subject_tenant_id()])
    }
}

/// Aggregated gear state.
#[derive(Clone)]
pub struct GearState {
    /// Gear configuration.
    pub config: Arc<OagwConfig>,
    /// Control-plane store.
    pub store: InMemoryStore,
    /// Auth plugin registry.
    pub auth_plugins: Arc<AuthPluginRegistry>,
    /// Guard plugin registry.
    pub guard_plugins: Arc<GuardPluginRegistry>,
    /// Transform plugin registry.
    pub transform_plugins: Arc<TransformPluginRegistry>,
    /// Credential resolver.
    pub secrets: Arc<dyn SecretResolver>,
    /// Tenant-chain provider.
    pub tenants: Arc<dyn TenantChain>,
}

impl std::fmt::Debug for GearState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GearState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl GearState {
    /// Builds the state from a gear context.
    ///
    /// # Errors
    ///
    /// Returns an error when the tenant resolver cannot be obtained from the
    /// ClientHub and no fallback is supplied.
    pub fn from_parts(
        config: OagwConfig,
        store: InMemoryStore,
        secrets: Arc<dyn SecretResolver>,
        tenants: Arc<dyn TenantChain>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            store,
            auth_plugins: Arc::new(AuthPluginRegistry::with_builtins(secrets.clone())),
            guard_plugins: Arc::new(GuardPluginRegistry::with_builtins()),
            transform_plugins: Arc::new(TransformPluginRegistry::with_builtins()),
            secrets,
            tenants,
        }
    }
}
