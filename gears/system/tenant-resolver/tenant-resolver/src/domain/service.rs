//! Domain service for the tenant resolver gear.
//!
//! Plugin discovery is lazy: resolved on first API call after
//! types-registry is ready.

use std::sync::Arc;
use std::time::Duration;

use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantResolverPluginClient,
    TenantResolverPluginSpecV1,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use toolkit::telemetry::ThrottledLog;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use tracing::info;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

use super::error::DomainError;

/// Throttle interval for unavailable plugin warnings.
const UNAVAILABLE_LOG_THROTTLE: Duration = Duration::from_secs(10);

/// Tenant resolver service.
///
/// Discovers plugins via types-registry and delegates API calls.
///
/// # Security Context
///
/// The gear itself does **not** perform its own access-control checks on the
/// `SecurityContext`. It passes the context through to the selected plugin,
/// which is responsible for deciding how (or whether) to enforce
/// authorization. This is intentional — different plugins may have
/// different access-control semantics.
#[domain_model]
pub struct Service {
    hub: Arc<ClientHub>,
    vendor: String,
    /// Shared selector for plugin instance IDs.
    selector: GtsPluginSelector,
    /// Throttle for plugin unavailable warnings.
    unavailable_log_throttle: ThrottledLog,
}

impl Service {
    /// Creates a new service with lazy plugin resolution.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            unavailable_log_throttle: ThrottledLog::new(UNAVAILABLE_LOG_THROTTLE),
        }
    }

    /// Lazily resolves and returns the plugin client.
    async fn get_plugin(&self) -> Result<Arc<dyn TenantResolverPluginClient>, DomainError> {
        let instance_id = self.selector.get_or_init(|| self.resolve_plugin()).await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());

        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn TenantResolverPluginClient>(&scope)
        {
            Ok(client)
        } else {
            if self.unavailable_log_throttle.should_log() {
                tracing::warn!(
                    plugin_gts_id = %instance_id,
                    vendor = %self.vendor,
                    "Plugin client not registered yet"
                );
            }
            Err(DomainError::PluginUnavailable {
                gts_id: instance_id.to_string(),
                reason: "client not registered yet".into(),
            })
        }
    }

    /// Resolves the plugin instance from types-registry.
    #[tracing::instrument(skip_all, fields(vendor = %self.vendor))]
    async fn resolve_plugin(&self) -> Result<String, DomainError> {
        info!("Resolving tenant resolver plugin");

        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        let plugin_type_id = TenantResolverPluginSpecV1::gts_type_id().clone();

        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{plugin_type_id}*")))
            .await?;

        let gts_id = choose_plugin_instance::<TenantResolverPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )?;
        info!(plugin_gts_id = %gts_id, "Selected tenant resolver plugin instance");

        Ok(gts_id)
    }

    /// Get tenant information by ID.
    ///
    /// Returns tenant info regardless of status - the consumer can decide
    /// how to handle different statuses.
    ///
    /// # Errors
    ///
    /// - `TenantNotFound` if tenant doesn't exist
    /// - Plugin resolution errors
    #[tracing::instrument(skip_all, fields(tenant.id = %id))]
    pub async fn get_tenant(
        &self,
        ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, DomainError> {
        let plugin = self.get_plugin().await?;
        plugin.get_tenant(ctx, id).await.map_err(DomainError::from)
    }

    /// Get the root tenant (the unique tenant with no parent).
    ///
    /// This forwards to the selected plugin's `get_root_tenant`; the
    /// single-root invariant is enforced by plugins themselves (e.g. the
    /// static plugin validates its config during `Service::from_config`).
    ///
    /// # Errors
    ///
    /// - Plugin resolution errors
    /// - Any error surfaced by the plugin
    #[tracing::instrument(skip_all)]
    pub async fn get_root_tenant(&self, ctx: &SecurityContext) -> Result<TenantInfo, DomainError> {
        let plugin = self.get_plugin().await?;
        plugin.get_root_tenant(ctx).await.map_err(DomainError::from)
    }

    /// Get multiple tenants by IDs (batch).
    ///
    /// Returns only found tenants - missing IDs are silently skipped.
    ///
    /// # Errors
    ///
    /// - Plugin resolution errors
    #[tracing::instrument(skip_all, fields(ids_count = ids.len()))]
    pub async fn get_tenants(
        &self,
        ctx: &SecurityContext,
        ids: &[TenantId],
        options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, DomainError> {
        let plugin = self.get_plugin().await?;
        plugin
            .get_tenants(ctx, ids, options)
            .await
            .map_err(DomainError::from)
    }

    /// Get ancestor chain from tenant to root.
    ///
    /// # Errors
    ///
    /// - `TenantNotFound` if tenant doesn't exist
    /// - Plugin resolution errors
    #[tracing::instrument(skip_all, fields(tenant.id = %id))]
    pub async fn get_ancestors(
        &self,
        ctx: &SecurityContext,
        id: TenantId,
        options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, DomainError> {
        let plugin = self.get_plugin().await?;
        plugin
            .get_ancestors(ctx, id, options)
            .await
            .map_err(DomainError::from)
    }

    /// Get descendants subtree of the given tenant.
    ///
    /// # Errors
    ///
    /// - `TenantNotFound` if tenant doesn't exist
    /// - Plugin resolution errors
    #[tracing::instrument(skip_all, fields(tenant.id = %id))]
    pub async fn get_descendants(
        &self,
        ctx: &SecurityContext,
        id: TenantId,
        options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, DomainError> {
        let plugin = self.get_plugin().await?;
        plugin
            .get_descendants(ctx, id, options)
            .await
            .map_err(DomainError::from)
    }

    /// Check if `ancestor_id` is an ancestor of `descendant_id`.
    ///
    /// # Errors
    ///
    /// - `TenantNotFound` if either tenant doesn't exist
    /// - Plugin resolution errors
    #[tracing::instrument(skip_all, fields(ancestor_id = %ancestor_id, descendant_id = %descendant_id))]
    pub async fn is_ancestor(
        &self,
        ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        options: &IsAncestorOptions,
    ) -> Result<bool, DomainError> {
        let plugin = self.get_plugin().await?;
        plugin
            .is_ancestor(ctx, ancestor_id, descendant_id, options)
            .await
            .map_err(DomainError::from)
    }
}
