//! Static model policy plugin (`static-mini-chat-model-policy-plugin`).
//!
//! Serves a fixed policy snapshot (version 1) from its configuration: the
//! model catalog, kill switches and per-user default limits.
//! `publish_usage` only logs.

mod service;

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub use service::{KillSwitchesConfig, StaticModelPolicyConfig, StaticModelPolicyService};

#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticModelPolicyPlugin;

impl Default for StaticModelPolicyPlugin {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        let service = Arc::new(StaticModelPolicyService::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?);
        let (instance_id, instance_json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_model_policy.v1",
            &cfg.vendor,
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, models = cfg.model_catalog.len(), "static model policy plugin registered");
        Ok(())
    }
}
