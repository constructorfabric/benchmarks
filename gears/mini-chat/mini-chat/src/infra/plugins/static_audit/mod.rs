//! Bundled static audit plugin gear (`static-mini-chat-audit-plugin`).
//!
//! Logs every audit event at `info` when `enabled`.

pub mod config;

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use self::config::StaticAuditConfig;

/// GTS instance segment of the bundled plugin.
pub const INSTANCE_SEGMENT: &str = "cf.builtin.static_mini_chat_audit.plugin.v1";

/// Static audit plugin client: logs events.
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            let payload = serde_json::to_string(&event)
                .map_err(|e| AuditPluginError::Permanent(e.to_string()))?;
            info!(
                event_type = event.event_type(),
                payload = %payload,
                "mini-chat audit event"
            );
        }
        Ok(())
    }
}

/// Static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticAuditPlugin;

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;

        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                &cfg.vendor,
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> =
            Arc::new(StaticAuditService::new(cfg.enabled));
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(
            instance_id = %instance_id,
            vendor = %cfg.vendor,
            priority = cfg.priority,
            enabled = cfg.enabled,
            "static mini-chat audit plugin registered"
        );
        Ok(())
    }
}
