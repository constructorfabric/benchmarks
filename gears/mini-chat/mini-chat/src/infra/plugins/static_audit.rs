//! Static audit plugin (`static-mini-chat-audit-plugin`, DESIGN Appendix B.1
//! "Bundled plugins", section 3.2 "Audit plugin and audit outbox"): a gear
//! implementing `MiniChatAuditPluginClientV1` that logs audit events.
//!
//! Each event is logged at `info` with its `event_type`, tenant, chat and
//! request ids only: prompts, responses, attachments and other content are
//! never logged. With `enabled = false` the plugin still registers but drops
//! every event silently.
//!
//! At `init` the gear reads its own config section (an absent section uses
//! the defaults), registers its `PluginV1<MiniChatAuditPluginSpecV1>`
//! instance in types-registry and publishes the service as a scoped client in
//! `ClientHub` under that instance id.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// GTS instance segment of the plugin's registered `PluginV1` instance.
pub const STATIC_AUDIT_INSTANCE_SEGMENT: &str = "cf.core.mini_chat.static_audit_plugin.v1";

const LOG_TARGET: &str = "mini_chat.audit";

const fn default_enabled() -> bool {
    true
}

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

const fn default_priority() -> i16 {
    100
}

/// Configuration of `static-mini-chat-audit-plugin` (DESIGN Appendix B.1
/// "Bundled plugins").
///
/// Unknown keys are rejected; every key has a default and an absent section
/// uses [`Default`] (the serde defaults).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// When `false`, the plugin registers but does not log audit events.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Vendor of the registered GTS instance.
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Priority of the registered GTS instance (lower is higher priority).
    #[serde(default = "default_priority")]
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            vendor: default_vendor(),
            priority: default_priority(),
        }
    }
}

/// Static `MiniChatAuditPluginClientV1` implementation: logs every event
/// (identifiers only) when enabled, does nothing when disabled.
#[derive(Debug, Clone, Copy)]
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    /// Builds the service; `enabled = false` turns `emit` into a no-op.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Whether events are logged.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if !self.enabled {
            return Ok(());
        }
        match &event {
            AuditEvent::Turn(turn) => info!(
                target: LOG_TARGET,
                event_type = event.event_type(),
                tenant_id = %turn.tenant_id,
                chat_id = %turn.chat_id,
                request_id = %turn.request_id,
                "audit event"
            ),
            AuditEvent::Mutation(mutation) => info!(
                target: LOG_TARGET,
                event_type = event.event_type(),
                tenant_id = %mutation.tenant_id,
                chat_id = %mutation.chat_id,
                original_request_id = mutation.original_request_id.map(tracing::field::display),
                new_request_id = mutation.new_request_id.map(tracing::field::display),
                request_id = mutation.request_id.map(tracing::field::display),
                "audit event"
            ),
        }
        Ok(())
    }
}

/// `static-mini-chat-audit-plugin` gear.
#[toolkit::gear(
    name = "static-mini-chat-audit-plugin",
    deps = [types_registry]
)]
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
        let cfg: StaticAuditConfig = ctx.config_or_default().map_err(|err| {
            anyhow::anyhow!("static-mini-chat-audit-plugin config could not be loaded: {err}")
        })?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            enabled = cfg.enabled,
            "static audit plugin configuration loaded"
        );

        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                STATIC_AUDIT_INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let service = Arc::new(StaticAuditService::new(cfg.enabled));

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id, "static audit plugin registered");
        Ok(())
    }
}
