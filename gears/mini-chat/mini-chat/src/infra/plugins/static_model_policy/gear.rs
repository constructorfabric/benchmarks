//! `ToolKit` registration of the static model policy plugin gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use super::config::StaticModelPolicyConfig;
use super::service::StaticModelPolicyService;

/// Instance segment of the registered plugin (`<spec type id><segment>`).
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_model_policy.v1";

/// The plugin gear (`static-mini-chat-model-policy-plugin`, config under
/// `gears.static-mini-chat-model-policy-plugin.config`). `init` loads the config (defaults
/// when the section is absent), builds the service, registers the plugin instance in
/// types-registry for `MiniChatModelPolicyPluginSpecV1` with the configured `vendor` and
/// `priority`, and registers its scoped client in the `ClientHub`.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticMiniChatModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

impl Default for StaticMiniChatModelPolicyPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

impl StaticMiniChatModelPolicyPlugin {
    /// The service built by `init`, if any.
    #[must_use]
    pub fn service(&self) -> Option<&Arc<StaticModelPolicyService>> {
        self.service.get()
    }
}

#[async_trait]
impl Gear for StaticMiniChatModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // an absent section uses the defaults; a present section is parsed strictly
        let section = ctx
            .config_provider()
            .get_gear_config(ctx.gear_name())
            .and_then(|gear| gear.get("config"));
        let config = match section {
            Some(raw) => StaticModelPolicyConfig::parse(raw.clone())?,
            None => StaticModelPolicyConfig::default(),
        };
        let (vendor, priority) = (config.vendor.clone(), config.priority);
        // the multipliers are validated here, before anything is registered
        let service = Arc::new(StaticModelPolicyService::new(config)?);

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-register
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let (instance_id, instance) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                vendor,
                priority,
            )?;
        RegisterResult::ensure_all_ok(&registry.register(vec![instance]).await?)?;
        let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = service.clone();
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(instance_id.as_ref()),
                client,
            );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-register

        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} is already initialized", Self::MODULE_NAME))?;
        tracing::info!(instance = %instance_id.as_ref(), "static model policy plugin registered");
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-return
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-return
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod gear_tests;
