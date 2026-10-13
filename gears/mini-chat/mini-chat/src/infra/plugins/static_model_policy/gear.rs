//! Gear registration of the static model-policy plugin.

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

/// Instance segment of the plugin's GTS instance id.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_model_policy.v1";

/// Static model-policy plugin gear.
#[derive(Default)]
#[toolkit::gear(
    name = "static-mini-chat-model-policy-plugin",
    deps = [types_registry]
)]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate().map_err(|err| {
            anyhow::anyhow!("static mini-chat model policy config invalid: {err}")
        })?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            models = cfg.model_catalog.len(),
            "loaded the static mini-chat model policy configuration"
        );
        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let service = Arc::new(StaticModelPolicyService::from_config(cfg));

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                client,
            );
        info!(instance_id = %instance_id, "static mini-chat model policy plugin registered");
        Ok(())
    }
}
