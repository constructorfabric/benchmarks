//! Gear declaration of the static model policy plugin.

use std::sync::Arc;

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

/// Static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticModelPolicyPlugin;

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx
            .config_or_default()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-model-policy-plugin config: {e}"))?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-model-policy-plugin config: {e}"))?;

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                "cf.builtin.static_mini_chat_model_policy.plugin.v1",
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> =
            Arc::new(StaticModelPolicyService::from_config(&cfg));
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        info!(
            %instance_id,
            models = cfg.model_catalog.len(),
            "static mini-chat model policy plugin registered"
        );
        Ok(())
    }
}
