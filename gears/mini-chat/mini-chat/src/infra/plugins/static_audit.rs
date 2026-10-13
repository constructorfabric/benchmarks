//! `static-mini-chat-audit-plugin`: logs audit events.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use toolkit::Gear;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    pub enabled: bool,
    pub vendor: String,
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self { enabled: true, vendor: "constructorfabric".to_owned(), priority: 100 }
    }
}

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
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            let json = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(target: "mini_chat", event_type = %event.event_type(), event = %json, "mini-chat audit event");
        }
        Ok(())
    }
}

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

impl Default for StaticAuditPlugin {
    fn default() -> Self {
        Self { service: OnceLock::new() }
    }
}

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let service = Arc::new(StaticAuditService::new(cfg.enabled));
        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_audit.v1",
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
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, enabled = cfg.enabled, "static audit plugin registered");
        Ok(())
    }
}
