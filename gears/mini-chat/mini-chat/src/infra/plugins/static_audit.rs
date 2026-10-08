//! `static-mini-chat-audit-plugin`: logs audit events.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditPluginConfig {
    pub vendor: String,
    pub priority: i16,
    pub enabled: bool,
}

impl Default for StaticAuditPluginConfig {
    fn default() -> Self {
        Self { vendor: "constructorfabric".to_owned(), priority: 100, enabled: true }
    }
}

/// Plugin service: logs every event when enabled.
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            let json = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(target: "mini_chat_audit", event_type = %event.event_type(), event = %json, "mini-chat audit event");
        }
        Ok(())
    }
}

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticMiniChatAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

impl Default for StaticMiniChatAuditPlugin {
    fn default() -> Self {
        Self { service: OnceLock::new() }
    }
}

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditPluginConfig = ctx.config_or_default()?;
        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_audit.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let service = Arc::new(StaticAuditService::new(cfg.enabled));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
