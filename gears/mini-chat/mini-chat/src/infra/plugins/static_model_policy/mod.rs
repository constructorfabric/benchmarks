//! Bundled static model policy plugin gear
//! (`static-mini-chat-model-policy-plugin`).

pub mod config;
pub mod service;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub use config::{StaticKillSwitches, StaticModelPolicyPluginConfig};
pub use service::StaticModelPolicyService;

/// Instance segment appended to the plugin spec type id.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_model_policy.v1";

#[toolkit::gear(
    name = "static-mini-chat-model-policy-plugin",
    deps = [types_registry]
)]
#[derive(Default)]
pub struct StaticMiniChatModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

#[async_trait]
impl Gear for StaticMiniChatModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyPluginConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|err| anyhow::anyhow!("static model policy plugin config invalid: {err}"))?;

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = Arc::new(StaticModelPolicyService::from_config(&cfg));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(instance_id.as_ref()),
                api,
            );
        tracing::info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            models = cfg.model_catalog.len(),
            "static mini-chat model policy plugin registered"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
