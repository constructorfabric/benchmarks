//! `ToolKit` gear registration of the bundled model policy plugin
//! (`static-mini-chat-model-policy-plugin`).
//!
//! Loads and validates the configuration section named after the gear,
//! registers the GTS plugin instance in types-registry and publishes the
//! `MiniChatModelPolicyPluginClientV1` in `ClientHub` scoped to that instance.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use super::config::StaticModelPolicyConfig;
use super::service::StaticModelPolicyService;
use crate::infra::plugins::raw_section;

/// Instance segment of the registered GTS instance.
pub(crate) const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_model_policy.v1";

/// Bundled model policy plugin gear: serves the configured catalog as policy
/// version 1.
#[toolkit::gear(
    name = "static-mini-chat-model-policy-plugin",
    deps = [types_registry]
)]
#[derive(Default)]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Parse and validate before anything is registered, so a bad section
        // fails the plugin init (naming the model id and field).
        let cfg = StaticModelPolicyConfig::load(raw_section(ctx))?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            models = cfg.model_catalog.len(),
            "Loaded plugin configuration"
        );

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-register-policy-instance
        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = Arc::new(StaticModelPolicyService::new(cfg));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-register-policy-instance

        info!(instance_id = %instance_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-return-policy-plugin
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-return-policy-plugin
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod tests;
