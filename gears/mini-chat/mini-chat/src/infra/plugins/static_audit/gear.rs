use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatAuditPluginSpecV1,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// GTS instance segment of the bundled static audit plugin.
pub const INSTANCE_SEGMENT: &str = "cf.builtin.static_mini_chat_audit.plugin.v1";

/// Configuration of the static audit plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    pub enabled: bool,
    pub vendor: String,
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            vendor: "constructorfabric".to_owned(),
            priority: 100,
        }
    }
}

/// Logging audit plugin.
pub struct StaticAuditService {
    enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            let json = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(event_type = %event.event_type(), tenant_id = %event.tenant_id(), event = %json, "mini-chat audit event");
        }
        Ok(())
    }
}

/// Static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticMiniChatAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
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
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let service = Arc::new(StaticAuditService {
            enabled: cfg.enabled,
        });
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        Ok(())
    }
}
