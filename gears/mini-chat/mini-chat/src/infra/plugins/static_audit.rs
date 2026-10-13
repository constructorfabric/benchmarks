//! Static audit plugin: logs mini-chat audit events.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, TurnAuditEvent,
    TurnMutationAuditEvent,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

const INSTANCE_SEGMENT: &str = "cf.builtin.static_mini_chat_audit.plugin.v1";

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

/// Logs audit events when enabled.
#[derive(Debug, Clone)]
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
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            tracing::info!(
                target: "mini_chat.audit",
                event_type = %event.event_type,
                tenant_id = %event.tenant_id,
                chat_id = %event.chat_id,
                request_id = %event.request_id,
                effective_model = %event.effective_model,
                "mini-chat turn audit event"
            );
        }
        Ok(())
    }

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError> {
        if self.enabled {
            tracing::info!(
                target: "mini_chat.audit",
                event_type = %event.event_type,
                tenant_id = %event.tenant_id,
                chat_id = %event.chat_id,
                "mini-chat turn mutation audit event"
            );
        }
        Ok(())
    }
}

/// `static-mini-chat-audit-plugin` gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

impl Default for StaticAuditPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticAuditPlugin {
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
        let svc = Arc::new(StaticAuditService::new(cfg.enabled));
        self.service
            .set(Arc::clone(&svc))
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = svc;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        Ok(())
    }
}
