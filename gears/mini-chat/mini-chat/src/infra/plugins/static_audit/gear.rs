//! Gear registration of the static audit plugin.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use super::config::StaticAuditConfig;
use super::service::StaticAuditService;

/// Instance segment under the audit plugin GTS type.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_audit.v1";

/// Static audit plugin gear.
#[toolkit::gear(
    name = "static-mini-chat-audit-plugin",
    deps = [types_registry]
)]
#[derive(Default)]
pub struct StaticAuditGear {
    service: OnceLock<Arc<StaticAuditService>>,
}

#[async_trait]
impl Gear for StaticAuditGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let service = Arc::new(StaticAuditService::new(&cfg));
        tracing::info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            enabled = cfg.enabled,
            "static audit plugin configured"
        );

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
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let client: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                client,
            );
        tracing::info!(instance_id = %instance_id, "static audit plugin registered");
        Ok(())
    }
}
