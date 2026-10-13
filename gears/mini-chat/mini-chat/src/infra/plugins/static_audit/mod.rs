//! Bundled static audit plugin gear (`static-mini-chat-audit-plugin`).
//!
//! Registers a GTS plugin instance with the types-registry and publishes a
//! scoped [`MiniChatAuditPluginClientV1`] through `ClientHub`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub mod config;
pub mod service;

use config::StaticAuditConfig;
use service::StaticAuditService;

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;

/// GTS instance segment of the bundled plugin.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_audit.v1";

/// Static audit plugin gear.
#[toolkit::gear(
    name = "static-mini-chat-audit-plugin",
    deps = [types_registry]
)]
pub struct StaticAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

impl Default for StaticAuditPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_expanded_or_default()?;

        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            enabled = cfg.enabled,
            "Loaded static audit plugin configuration"
        );

        let service = Arc::new(StaticAuditService::from_config(&cfg));

        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id, "Static audit plugin registered");
        Ok(())
    }
}
