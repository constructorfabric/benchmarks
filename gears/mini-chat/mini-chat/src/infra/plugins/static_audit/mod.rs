//! Bundled static audit plugin (`static-mini-chat-audit-plugin`): logs audit events.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
    TurnAuditEvent, TurnDeleteAuditEvent, TurnEditAuditEvent, TurnRetryAuditEvent,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// When `false`, the plugin registers but does not log events.
    pub enabled: bool,
    /// Vendor.
    pub vendor: String,
    /// Priority.
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self { enabled: true, vendor: "constructorfabric".to_owned(), priority: 100 }
    }
}

/// Logging audit client.
pub struct StaticAuditClient {
    enabled: bool,
}

impl StaticAuditClient {
    /// New client.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn log(&self, kind: &str, payload: impl serde::Serialize) {
        if self.enabled {
            let json = serde_json::to_string(&payload).unwrap_or_default();
            tracing::info!(target: "mini_chat::audit", event_type = kind, event = %json, "mini-chat audit event");
        }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditClient {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.log(&event.event_type.clone(), event);
        Ok(())
    }
    async fn emit_turn_retry_audit(
        &self,
        event: TurnRetryAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError> {
        self.log("turn_retry", event);
        Ok(())
    }
    async fn emit_turn_edit_audit(
        &self,
        event: TurnEditAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError> {
        self.log("turn_edit", event);
        Ok(())
    }
    async fn emit_turn_delete_audit(
        &self,
        event: TurnDeleteAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError> {
        self.log("turn_delete", event);
        Ok(())
    }
}

/// The bundled static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticAuditPlugin;

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let (instance_id, json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_audit.plugin.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        RegisterResult::ensure_all_ok(&registry.register(vec![json]).await?)?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(StaticAuditClient::new(cfg.enabled));
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, enabled = cfg.enabled, "static audit plugin registered");
        Ok(())
    }
}
