use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};

use super::config::StaticMiniChatAuditPluginConfig;
use super::service::Service;

/// Static audit plugin gear for mini-chat.
///
/// Logs all audit events via `tracing` for development and testing.
/// When `enabled: false` in config, the plugin registers normally but
/// all emit methods return immediately without logging.
#[toolkit::gear(
    name = "static-mini-chat-audit-plugin",
    deps = [types_registry]
)]
pub struct StaticMiniChatAuditPlugin {
    service: OnceLock<Arc<Service>>,
}

impl Default for StaticMiniChatAuditPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticMiniChatAuditPluginConfig = ctx.config_or_default()?;
        info!(
            enabled = cfg.enabled,
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "Loaded static mini-chat audit plugin configuration"
        );

        let service = Arc::new(Service {
            enabled: cfg.enabled,
        });
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Build registration payload and instance id for this plugin.
        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                "cf.core._.static_mini_chat_audit.v1",
                &cfg.vendor,
                cfg.priority,
            )?;

        // Publish to types-registry.
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id, "Static mini-chat audit plugin registered");
        Ok(())
    }
}
