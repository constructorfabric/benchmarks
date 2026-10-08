//! `static-mini-chat-model-policy-plugin` gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub mod config;
pub mod service;

use config::StaticModelPolicyConfig;
use service::StaticModelPolicyService;

/// Instance segment of the registered GTS plugin instance.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_model_policy.v1";

/// Bundled `static-mini-chat-model-policy-plugin`.
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
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        let vendor = cfg.vendor.clone();
        let priority = cfg.priority;
        let service = Arc::new(
            StaticModelPolicyService::from_config(cfg)
                .map_err(|e| anyhow::anyhow!("{} config invalid: {e}", Self::MODULE_NAME))?,
        );

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                vendor,
                priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        info!(instance_id = %instance_id, "static-mini-chat-model-policy-plugin registered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit::config::ConfigProvider;
    use toolkit::{Gear, GearCtx};
    use types_registry_sdk::TypesRegistryClient;
    use uuid::Uuid;

    use super::StaticModelPolicyPlugin;
    use crate::test_support::fixtures::catalog_entry_json;
    use crate::test_support::registry::RecordingRegistry;

    struct TestConfig(Value);

    impl ConfigProvider for TestConfig {
        fn get_gear_config(&self, gear: &str) -> Option<&Value> {
            self.0.get(gear)
        }
    }

    fn ctx(hub: &Arc<ClientHub>, config: Value) -> GearCtx {
        GearCtx::new(
            StaticModelPolicyPlugin::MODULE_NAME,
            Uuid::new_v4(),
            Arc::new(TestConfig(config)),
            Arc::clone(hub),
            CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn registers_instance_and_scoped_client() {
        let registry = Arc::new(RecordingRegistry::default());
        let hub = Arc::new(ClientHub::new());
        hub.register::<dyn TypesRegistryClient>(registry.clone());

        let config = json!({"static-mini-chat-model-policy-plugin": {"config": {
            "vendor": "acme",
            "priority": 7,
            "model_catalog": [catalog_entry_json("gpt-x")],
        }}});
        let gear = StaticModelPolicyPlugin::default();
        gear.init(&ctx(&hub, config.clone())).await.unwrap();

        let registered = registry.registered();
        assert_eq!(registered.len(), 1);
        let id = registered[0]["id"].as_str().unwrap().to_owned();
        assert!(
            id.ends_with("~cf.core._.static_mini_chat_model_policy.v1"),
            "{id}"
        );
        assert!(id.starts_with(
            "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"
        ));
        assert_eq!(registered[0]["vendor"], "acme");
        assert_eq!(registered[0]["priority"], 7);

        let client = hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(&id))
            .expect("scoped client registered");
        let snapshot = client.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();
        assert_eq!(snapshot.model_catalog.len(), 1);
        assert_eq!(snapshot.model_catalog[0].id, "gpt-x");

        assert!(
            gear.init(&ctx(&hub, config)).await.is_err(),
            "second init must fail"
        );
    }

    #[tokio::test]
    async fn absent_section_registers_with_empty_catalog() {
        let registry = Arc::new(RecordingRegistry::default());
        let hub = Arc::new(ClientHub::new());
        hub.register::<dyn TypesRegistryClient>(registry.clone());
        StaticModelPolicyPlugin::default()
            .init(&ctx(&hub, json!({})))
            .await
            .unwrap();
        let registered = registry.registered();
        assert_eq!(registered[0]["vendor"], "constructorfabric");
        assert_eq!(registered[0]["priority"], 100);
    }

    #[tokio::test]
    async fn invalid_multiplier_fails_init_before_registering() {
        let registry = Arc::new(RecordingRegistry::default());
        let hub = Arc::new(ClientHub::new());
        hub.register::<dyn TypesRegistryClient>(registry.clone());
        let mut entry = catalog_entry_json("bad");
        entry["input_tokens_credit_multiplier_micro"] = json!(0);
        let config = json!({"static-mini-chat-model-policy-plugin": {"config": {
            "model_catalog": [entry],
        }}});
        let err = StaticModelPolicyPlugin::default()
            .init(&ctx(&hub, config))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("bad"), "{err}");
        assert!(registry.registered().is_empty());
    }
}
