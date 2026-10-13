//! `ToolKit` gear registration of the bundled audit plugin
//! (`static-mini-chat-audit-plugin`).
//!
//! Loads and validates the configuration section named after the gear,
//! registers the GTS plugin instance in types-registry and publishes the
//! `MiniChatAuditPluginClientV1` in `ClientHub` scoped to that instance.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use super::config::StaticAuditConfig;
use super::service::StaticAuditService;
use crate::infra::plugins::raw_section;

/// Instance segment of the registered GTS instance.
pub(crate) const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_audit.v1";

/// Bundled audit plugin gear: logs every audit event as a structured record.
#[toolkit::gear(
    name = "static-mini-chat-audit-plugin",
    deps = [types_registry]
)]
#[derive(Default)]
pub struct StaticAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg = StaticAuditConfig::load(raw_section(ctx))?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            enabled = cfg.enabled,
            "Loaded plugin configuration"
        );

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-register-audit-instance
        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = Arc::new(StaticAuditService::new(cfg));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-register-audit-instance

        info!(instance_id = %instance_id);
        Ok(())
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod tests;
