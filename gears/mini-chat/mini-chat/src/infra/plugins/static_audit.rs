//! Bundled static audit plugin (`static-mini-chat-audit-plugin`).
//!
//! Logs audit events delivered by the `mini-chat.audit` outbox handler.

use std::sync::OnceLock;

use async_trait::async_trait;
use mini_chat_sdk::{AuditError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, TurnAuditEvent};
use serde::Deserialize;
use toolkit::{Gear, GearCtx};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// Static audit plugin configuration
/// (`gears.static-mini-chat-audit-plugin.config`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// Vendor of the registered GTS instance.
    pub vendor: String,
    /// Instance priority (lower = higher priority).
    pub priority: i16,
    /// When `false`, the plugin registers but does not log audit events.
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

/// The static audit service.
pub struct StaticAuditService {
    enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn record_audit_event(&self, event: TurnAuditEvent) -> Result<(), AuditError> {
        if self.enabled {
            tracing::info!(
                event_type = %event.event_type,
                tenant_id = %event.tenant_id,
                chat_id = %event.chat_id,
                request_id = %event.request_id,
                effective_model = %event.effective_model,
                "mini-chat audit event"
            );
        }
        Ok(())
    }
}

/// The static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticAuditPlugin {
    service: OnceLock<std::sync::Arc<StaticAuditService>>,
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
        if cfg.vendor.is_empty() {
            anyhow::bail!("static-mini-chat-audit-plugin: vendor must be non-empty");
        }

        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_audit.plugin.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = std::sync::Arc::new(StaticAuditService {
            enabled: cfg.enabled,
        });
        self.service
            .set(std::sync::Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("static audit plugin already initialized"))?;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(instance_id.as_ref()),
                service,
            );
        Ok(())
    }
}
