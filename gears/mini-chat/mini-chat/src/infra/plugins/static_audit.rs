//! `static-mini-chat-audit-plugin`: logs audit events (ADR-0009).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1, TurnAuditEvent,
    TurnMutationAuditEvent,
};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    pub vendor: String,
    pub priority: i16,
    pub enabled: bool,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            enabled: true,
        }
    }
}

/// Logging audit sink.
pub struct StaticAudit {
    enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAudit {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            tracing::info!(
                event_type = %event.event_type,
                chat_id = %event.chat_id,
                request_id = %event.request_id,
                effective_model = %event.effective_model,
                "mini-chat audit"
            );
        }
        Ok(())
    }

    async fn emit_turn_mutation_audit(&self, event: TurnMutationAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            tracing::info!(event_type = %event.event_type, chat_id = %event.chat_id, "mini-chat audit");
        }
        Ok(())
    }
}

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticMiniChatAuditPlugin {
    service: OnceLock<Arc<StaticAudit>>,
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
        let (instance_id, payload) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_audit.plugin.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![payload]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let service = Arc::new(StaticAudit { enabled: cfg.enabled });
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
