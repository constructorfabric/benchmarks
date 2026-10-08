//! Bundled static model policy plugin gear (`static-mini-chat-model-policy-plugin`).
//!
//! Validates its configuration, registers a GTS plugin instance with the
//! types-registry and publishes a scoped [`MiniChatModelPolicyPluginClientV1`]
//! through `ClientHub`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub mod config;
pub mod service;

use config::StaticModelPolicyConfig;
use service::StaticModelPolicyService;

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;

/// GTS instance segment of the bundled plugin.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_model_policy.v1";

/// Static model policy plugin gear.
#[toolkit::gear(
    name = "static-mini-chat-model-policy-plugin",
    deps = [types_registry]
)]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

impl Default for StaticModelPolicyPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_expanded_or_default()?;

        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            models = cfg.model_catalog.len(),
            "Loaded static model policy plugin configuration"
        );

        // Validate before registering anything.
        let service = Arc::new(StaticModelPolicyService::from_config(&cfg)?);

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id, "Static model policy plugin registered");
        Ok(())
    }
}
