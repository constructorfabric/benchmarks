//! Bundled static audit plugin (`static-mini-chat-audit-plugin`).
//!
//! Logs every audit event it receives (when `enabled`).

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
    TurnAuditEvent, TurnMutationAuditEvent,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
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
        Self {
            enabled: true,
            vendor: "constructorfabric".to_owned(),
            priority: 100,
        }
    }
}

/// Logging audit sink.
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
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            let payload = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(target: "mini_chat.audit", event = %payload, "turn audit event");
        }
        Ok(())
    }

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            let payload = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(target: "mini_chat.audit", event = %payload, "turn mutation audit event");
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
                "cf.core._.static_mini_chat_audit.v1",
                cfg.vendor.clone(),
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
        tracing::info!(
            enabled = cfg.enabled,
            "static mini-chat audit plugin registered"
        );
        Ok(())
    }
}
