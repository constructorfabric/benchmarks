//! Bundled static audit plugin gear (`static-mini-chat-audit-plugin`).

pub mod config;
pub mod service;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub use config::StaticAuditPluginConfig;
pub use service::StaticAuditService;

/// Instance segment appended to the plugin spec type id.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_audit.v1";

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticMiniChatAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditPluginConfig = ctx.config_or_default()?;

        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = Arc::new(StaticAuditService::from_config(&cfg));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(instance_id.as_ref()),
                api,
            );
        tracing::info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            enabled = cfg.enabled,
            "static mini-chat audit plugin registered"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
