//! `static-mini-chat-audit-plugin` gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

pub mod config;
pub mod service;

use config::StaticAuditConfig;
use service::StaticAuditService;

/// Instance segment of the registered GTS plugin instance.
const INSTANCE_SEGMENT: &str = "cf.core._.static_mini_chat_audit.v1";

/// Bundled `static-mini-chat-audit-plugin`.
#[toolkit::gear(
    name = "static-mini-chat-audit-plugin",
    deps = [types_registry]
)]
#[derive(Default)]
pub struct StaticAuditPlugin {
    service: OnceLock<Arc<StaticAuditService>>,
}

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let vendor = cfg.vendor.clone();
        let priority = cfg.priority;
        let service = Arc::new(StaticAuditService::new(cfg.enabled));

        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
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

        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        info!(instance_id = %instance_id, "static-mini-chat-audit-plugin registered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mini_chat_sdk::MiniChatAuditPluginClientV1;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit::config::ConfigProvider;
    use toolkit::{Gear, GearCtx};
    use types_registry_sdk::TypesRegistryClient;
    use uuid::Uuid;

    use super::StaticAuditPlugin;
    use crate::test_support::registry::RecordingRegistry;

    struct TestConfig(Value);

    impl ConfigProvider for TestConfig {
        fn get_gear_config(&self, gear: &str) -> Option<&Value> {
            self.0.get(gear)
        }
    }

    fn ctx(hub: &Arc<ClientHub>, config: Value) -> GearCtx {
        GearCtx::new(
            StaticAuditPlugin::MODULE_NAME,
            Uuid::new_v4(),
            Arc::new(TestConfig(config)),
            Arc::clone(hub),
            CancellationToken::new(),
        )
    }

    fn hub_with_registry() -> (Arc<ClientHub>, Arc<RecordingRegistry>) {
        let registry = Arc::new(RecordingRegistry::default());
        let hub = Arc::new(ClientHub::new());
        hub.register::<dyn TypesRegistryClient>(registry.clone());
        (hub, registry)
    }

    #[tokio::test]
    async fn registers_instance_and_scoped_client() {
        let (hub, registry) = hub_with_registry();
        let config = json!({"static-mini-chat-audit-plugin": {"config": {
            "vendor": "acme", "priority": 3, "enabled": false
        }}});
        let gear = StaticAuditPlugin::default();
        gear.init(&ctx(&hub, config.clone())).await.unwrap();

        let registered = registry.registered();
        assert_eq!(registered.len(), 1);
        let id = registered[0]["id"].as_str().unwrap().to_owned();
        assert!(id.ends_with("~cf.core._.static_mini_chat_audit.v1"), "{id}");
        assert!(
            id.starts_with("gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~")
        );
        assert_eq!(registered[0]["vendor"], "acme");
        assert_eq!(registered[0]["priority"], 3);

        assert!(
            hub.try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
                .is_some()
        );
        assert!(gear.init(&ctx(&hub, config)).await.is_err());
    }

    #[tokio::test]
    async fn absent_section_uses_defaults() {
        let (hub, registry) = hub_with_registry();
        StaticAuditPlugin::default()
            .init(&ctx(&hub, json!({})))
            .await
            .unwrap();
        let registered = registry.registered();
        assert_eq!(registered[0]["vendor"], "constructorfabric");
        assert_eq!(registered[0]["priority"], 100);
    }

    #[tokio::test]
    async fn unknown_config_key_fails_init() {
        let (hub, registry) = hub_with_registry();
        let config = json!({"static-mini-chat-audit-plugin": {"config": {"bogus": 1}}});
        assert!(
            StaticAuditPlugin::default()
                .init(&ctx(&hub, config))
                .await
                .is_err()
        );
        assert!(registry.registered().is_empty());
    }
}
