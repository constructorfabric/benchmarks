//! `ToolKit` registration of the static audit plugin gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use super::config::StaticAuditConfig;
use super::service::StaticAuditService;

/// Instance segment of the registered plugin (`<spec type id><segment>`).
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_audit.v1";

/// The plugin gear (`static-mini-chat-audit-plugin`, config under
/// `gears.static-mini-chat-audit-plugin.config`). `init` loads the config (defaults when the
/// section is absent), registers a plugin instance in types-registry for
/// `MiniChatAuditPluginSpecV1` with the configured `vendor` and `priority`, and registers its
/// scoped client in the `ClientHub`.
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

impl StaticMiniChatAuditPlugin {
    /// The service built by `init`, if any.
    #[must_use]
    pub fn service(&self) -> Option<&Arc<StaticAuditService>> {
        self.service.get()
    }
}

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // an absent section uses the defaults; a present section is parsed strictly
        let section = ctx
            .config_provider()
            .get_gear_config(ctx.gear_name())
            .and_then(|gear| gear.get("config"));
        let config = match section {
            Some(raw) => StaticAuditConfig::parse(raw.clone())?,
            None => StaticAuditConfig::default(),
        };
        let service = Arc::new(StaticAuditService::new(&config));

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-register
        // a disabled plugin is registered too, so it stays selectable but silent
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let (instance_id, instance) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            INSTANCE_SEGMENT,
            config.vendor.clone(),
            config.priority,
        )?;
        RegisterResult::ensure_all_ok(&registry.register(vec![instance]).await?)?;
        let client: Arc<dyn MiniChatAuditPluginClientV1> = service.clone();
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(instance_id.as_ref()),
                client,
            );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-register

        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} is already initialized", Self::MODULE_NAME))?;
        tracing::info!(instance = %instance_id.as_ref(), "static audit plugin registered");
        Ok(())
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod gear_tests;
