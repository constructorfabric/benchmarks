//! Gear declaration of the static audit plugin.

use std::sync::Arc;

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

/// Static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticAuditPlugin;

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx
            .config_or_default()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-audit-plugin config: {e}"))?;
        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                "cf.builtin.static_mini_chat_audit.plugin.v1",
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(StaticAuditService {
            enabled: cfg.enabled,
        });
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        info!(%instance_id, enabled = cfg.enabled, "static mini-chat audit plugin registered");
        Ok(())
    }
}
