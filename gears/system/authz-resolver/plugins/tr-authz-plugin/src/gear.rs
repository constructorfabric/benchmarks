//! TR `AuthZ` resolver plugin gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverPluginClient, AuthZResolverPluginSpecV1};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::config::TrAuthZPluginConfig;
use crate::domain::Service;

/// TR-based `AuthZ` resolver plugin gear.
///
/// Resolves tenant hierarchy via `TenantResolverClient` instead of
/// accessing Resource Group directly.
#[toolkit::gear(
    name = "tr-authz-plugin",
    deps = [types_registry, tenant_resolver]
)]
pub struct TrAuthZPlugin {
    service: OnceLock<Arc<Service>>,
}

impl Default for TrAuthZPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for TrAuthZPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: TrAuthZPluginConfig = ctx.config_or_default()?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "Loaded TR AuthZ plugin configuration"
        );

        // Generate plugin instance ID
        let instance_id = AuthZResolverPluginSpecV1::gts_make_instance_id(
            "cf.builtin.tr_authz_resolver.plugin.v1",
        );

        // Resolve Tenant Resolver client from ClientHub first — if it's not
        // available we want to fail before leaving a dangling instance in
        // the types-registry.
        let tr: Arc<dyn TenantResolverClient> =
            ctx.client_hub().get::<dyn TenantResolverClient>()?;

        // Register plugin instance in types-registry
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let instance = PluginV1::<AuthZResolverPluginSpecV1> {
            id: instance_id.clone(),
            vendor: cfg.vendor.clone(),
            priority: cfg.priority,
            properties: AuthZResolverPluginSpecV1,
        };
        let instance_json = serde_json::to_value(&instance)?;

        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        // Create service with TR dependency
        let service = Arc::new(Service::new(tr));
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Register scoped client in ClientHub
        let api: Arc<dyn AuthZResolverPluginClient> = service;
        ctx.client_hub()
            .register_scoped::<dyn AuthZResolverPluginClient>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id);
        Ok(())
    }
}
