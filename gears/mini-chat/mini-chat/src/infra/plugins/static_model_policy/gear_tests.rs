#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the gear registration part of
//! `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`
//! (steps `inst-register-policy-instance`, `inst-return-policy-plugin`).

use std::sync::Arc;

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use serde_json::{Value, json};
use toolkit::Gear;
use toolkit::client_hub::{ClientHub, ClientScope};
use uuid::Uuid;

use super::StaticModelPolicyPlugin;
use crate::infra::policy_gateway::ModelPolicyGateway;
use crate::infra::test_support::{FakeTypesRegistry, FixedConfig, gear_ctx};

const GEAR: &str = "static-mini-chat-model-policy-plugin";

fn expected_instance_id() -> String {
    format!(
        "{}cf.core._.static_mini_chat_model_policy.v1",
        MiniChatModelPolicyPluginSpecV1::gts_type_id()
    )
}

fn entry(model_id: &str) -> Value {
    json!({
        "id": model_id,
        "provider_model_id": model_id,
        "display_name": "Model",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "2026-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": true, "file_search": true, "image_generation": false,
                "code_interpreter": true, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": false, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        }
    })
}

#[test]
fn the_gear_is_named_after_the_bundled_plugin() {
    assert_eq!(StaticModelPolicyPlugin::MODULE_NAME, GEAR);
}

#[tokio::test]
async fn init_registers_the_gts_instance_with_vendor_and_priority() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(
        GEAR,
        FixedConfig::with_section(
            GEAR,
            json!({ "vendor": "acme", "priority": 3, "model_catalog": [] }),
        ),
        &hub,
        &registry,
    );

    StaticModelPolicyPlugin::default()
        .init(&ctx)
        .await
        .expect("init");

    assert_eq!(registry.instance_ids(), vec![expected_instance_id()]);
    let object = registry.instance_object(&expected_instance_id()).unwrap();
    assert_eq!(object["vendor"], "acme");
    assert_eq!(object["priority"], 3);
}

#[tokio::test]
async fn an_absent_section_serves_an_empty_catalog_with_the_defaults() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR, FixedConfig::default(), &hub, &registry);

    StaticModelPolicyPlugin::default()
        .init(&ctx)
        .await
        .expect("init");

    let object = registry.instance_object(&expected_instance_id()).unwrap();
    assert_eq!(object["vendor"], "constructorfabric");
    assert_eq!(object["priority"], 100);
    let client = hub
        .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(
            &expected_instance_id(),
        ))
        .expect("scoped client");
    let snapshot = client
        .get_policy_snapshot(Uuid::from_u128(0x55), 1)
        .await
        .expect("snapshot");
    assert!(snapshot.model_catalog.is_empty());
}

#[tokio::test]
async fn init_publishes_the_configured_catalog() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(
        GEAR,
        FixedConfig::with_section(
            GEAR,
            json!({
                "model_catalog": [entry("gpt-5.2")],
                "kill_switches": { "disable_images": true }
            }),
        ),
        &hub,
        &registry,
    );
    StaticModelPolicyPlugin::default()
        .init(&ctx)
        .await
        .expect("init");

    let snapshot = ModelPolicyGateway::new(&hub, "constructorfabric")
        .policy_snapshot(Uuid::from_u128(0x55), 1)
        .await
        .expect("the gateway resolves the bundled plugin");

    assert_eq!(snapshot.model_catalog.len(), 1);
    assert_eq!(snapshot.model_catalog[0].id, "gpt-5.2");
    assert!(snapshot.kill_switches.disable_images);
}

#[tokio::test]
async fn a_present_section_without_a_catalog_fails_init() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(
        GEAR,
        FixedConfig::with_section(GEAR, json!({ "vendor": "acme" })),
        &hub,
        &registry,
    );

    let err = StaticModelPolicyPlugin::default()
        .init(&ctx)
        .await
        .expect_err("model_catalog is required");

    assert!(err.to_string().contains("model_catalog"), "{err}");
    assert!(registry.instance_ids().is_empty());
}

#[tokio::test]
async fn an_invalid_entry_fails_init_naming_the_model_and_field() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let mut bad = entry("bad-model");
    bad["output_tokens_credit_multiplier_micro"] = json!(0);
    let ctx = gear_ctx(
        GEAR,
        FixedConfig::with_section(GEAR, json!({ "model_catalog": [entry("ok"), bad] })),
        &hub,
        &registry,
    );

    let err = StaticModelPolicyPlugin::default()
        .init(&ctx)
        .await
        .expect_err("invalid entry");

    let text = err.to_string();
    assert!(text.contains("bad-model"), "{text}");
    assert!(
        text.contains("output_tokens_credit_multiplier_micro"),
        "{text}"
    );
    assert!(registry.instance_ids().is_empty(), "nothing registered");
    assert!(
        hub.try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(
            &expected_instance_id()
        ))
        .is_none()
    );
}

#[tokio::test]
async fn a_registry_failure_fails_init() {
    let registry = FakeTypesRegistry::new();
    registry.fail_register("registry down");
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR, FixedConfig::default(), &hub, &registry);

    StaticModelPolicyPlugin::default()
        .init(&ctx)
        .await
        .expect_err("registration fails");
}
