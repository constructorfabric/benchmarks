use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelPreference, PolicySnapshot, PublishError, UsageEvent,
    UserLimits,
};
use serde_json::json;
use uuid::Uuid;

use super::ModelResolver;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyPort;

struct StubPolicy(Arc<PolicySnapshot>);

#[async_trait]
impl PolicyPort for StubPolicy {
    async fn current_snapshot(&self, _: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        Ok(Arc::clone(&self.0))
    }
    async fn snapshot_for_version(
        &self,
        _: Uuid,
        _: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        Ok(Arc::clone(&self.0))
    }
    async fn user_limits(&self, _: Uuid, _: u64) -> Result<UserLimits, DomainError> {
        Err(DomainError::Internal("unused".to_owned()))
    }
    async fn publish_usage(&self, _: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

fn entry(id: &str, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    let mut e: ModelCatalogEntry = serde_json::from_value(json!({
        "id": id,
        "provider_model_id": format!("{id}-provider"),
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1,
        "output_tokens_credit_multiplier_micro": 1,
        "max_num_results": 5,
        "general_config": {
            "type": "chat",
            "available_from": "",
            "max_file_size_mb": 25,
            "api_params": {"stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true, "image_generation": false,
                             "code_interpreter": false, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true, "embeddings": false,
                                    "image_generation": false, "audio_speech_generation": false,
                                    "audio_transcription": false, "audio_translation": false}
        },
        "enabled": enabled
    }))
    .expect("catalog entry fixture");
    if is_default {
        e.preference = Some(ModelPreference {
            is_default: true,
            sort_order: 0,
        });
    }
    e
}

fn resolver(catalog: Vec<ModelCatalogEntry>) -> ModelResolver {
    ModelResolver::new(Arc::new(StubPolicy(Arc::new(PolicySnapshot {
        policy_version: 1,
        model_catalog: catalog,
        kill_switches: KillSwitches::default(),
    }))))
}

const USER: Uuid = Uuid::nil();

#[tokio::test]
async fn new_chat_default_is_first_enabled_is_default_entry() {
    let r = resolver(vec![
        entry("a", true, false),
        entry("off-default", false, true),
        entry("b", true, true),
        entry("c", true, true),
    ]);
    let m = r
        .resolve_for_new_chat(USER, None)
        .await
        .expect("default model");
    assert_eq!(m.id, "b");
}

#[tokio::test]
async fn new_chat_default_falls_back_to_first_enabled() {
    let r = resolver(vec![entry("off", false, false), entry("a", true, false)]);
    let m = r
        .resolve_for_new_chat(USER, None)
        .await
        .expect("default model");
    assert_eq!(m.id, "a");
}

#[tokio::test]
async fn new_chat_without_enabled_models_is_invalid_model() {
    let r = resolver(vec![entry("off", false, true)]);
    assert_eq!(
        r.resolve_for_new_chat(USER, None).await,
        Err(DomainError::InvalidModel)
    );
}

#[tokio::test]
async fn new_chat_requested_disabled_or_unknown_is_invalid_model() {
    let r = resolver(vec![entry("off", false, false), entry("a", true, false)]);
    assert_eq!(
        r.resolve_for_new_chat(USER, Some("off")).await,
        Err(DomainError::InvalidModel)
    );
    assert_eq!(
        r.resolve_for_new_chat(USER, Some("nope")).await,
        Err(DomainError::InvalidModel)
    );
    assert_eq!(
        r.resolve_for_new_chat(USER, Some("a")).await.map(|m| m.id),
        Ok("a".to_owned())
    );
}

#[tokio::test]
async fn chat_model_resolution_ignores_enabled_flag() {
    let r = resolver(vec![entry("off", false, false)]);
    let (snap, m) = r
        .resolve_chat_model(USER, "off")
        .await
        .expect("disabled still resolves");
    assert_eq!(m.id, "off");
    assert_eq!(snap.policy_version, 1);
    assert_eq!(
        r.resolve_chat_model(USER, "gone").await.map(|(_, m)| m.id),
        Err(DomainError::InvalidModel)
    );
}

#[tokio::test]
async fn visible_models_are_enabled_in_catalog_order() {
    let r = resolver(vec![
        entry("z", true, false),
        entry("off", false, false),
        entry("a", true, false),
    ]);
    let ids: Vec<String> = r
        .visible_models(USER)
        .await
        .expect("visible")
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["z", "a"]);
}

#[test]
fn find_returns_entry_by_id() {
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![entry("a", false, false)],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(
        ModelResolver::find(&snap, "a").map(|m| m.id.as_str()),
        Some("a")
    );
    assert!(ModelResolver::find(&snap, "b").is_none());
}
