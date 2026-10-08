#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use crate::models::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot};

fn config_catalog() -> Vec<ModelCatalogEntry> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    );
    let raw = std::fs::read_to_string(path).expect("read config/mini-chat.yaml");
    let doc: serde_json::Value = serde_saphyr::from_str(&raw).expect("parse yaml");
    let catalog =
        doc["gears"]["static-mini-chat-model-policy-plugin"]["config"]["model_catalog"].clone();
    serde_json::from_value(catalog).expect("catalog entries")
}

fn minimal_entry_json(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "p",
        "provider_display_name": "P",
        "tier": "standard",
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 900,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 2_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {
                "web_search": false, "file_search": false, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": true, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        },
        "multimodal_capabilities": []
    })
}

fn entry(id: &str, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    let mut v = minimal_entry_json(id);
    v["enabled"] = json!(enabled);
    v["preference"] = json!({"is_default": is_default, "sort_order": 0});
    serde_json::from_value(v).unwrap()
}

fn snapshot(models: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: models,
        kill_switches: KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: false,
            disable_web_search: false,
            disable_file_search: false,
            disable_images: false,
            disable_code_interpreter: false,
        },
    }
}

#[test]
fn parses_config_catalog_entry() {
    let catalog = config_catalog();
    let e = catalog.iter().find(|e| e.id == "gpt-4.1").expect("gpt-4.1");
    assert_eq!(e.tier, ModelTier::Premium);
    assert!(e.supports_vision());
    assert!(e.is_default());
    assert_eq!(e.estimation_budgets.tool_surcharge_tokens, 500);
    assert!(e.general_config.tool_support.web_search);
    assert_eq!(e.max_tool_calls, 10);
    assert_eq!(e.web_search_context_size, "low");
}

#[test]
fn defaults_apply_for_optional_fields() {
    let e: ModelCatalogEntry = serde_json::from_value(minimal_entry_json("m")).unwrap();
    assert!(!e.enabled);
    assert_eq!(e.max_tool_calls, 2);
    assert_eq!(e.web_search_context_size, "low");
    assert_eq!(e.estimation_budgets.bytes_per_token_conservative, 4);
    assert_eq!(e.estimation_budgets.minimal_generation_floor, 50);
    assert!(e.preference.is_none());
    assert!(!e.is_default());
    assert!(!e.supports_vision());
    assert!(e.description.is_empty());
}

#[test]
fn tier_accepts_lowercase_and_capitalized() {
    for (s, t) in [
        ("standard", ModelTier::Standard),
        ("Standard", ModelTier::Standard),
        ("premium", ModelTier::Premium),
        ("Premium", ModelTier::Premium),
    ] {
        let parsed: ModelTier = serde_json::from_value(json!(s)).unwrap();
        assert_eq!(parsed, t);
    }
    assert_eq!(
        serde_json::to_value(ModelTier::Premium).unwrap(),
        json!("premium")
    );
    assert_eq!(ModelTier::Standard.as_str(), "standard");
    assert_eq!(ModelTier::Premium.as_str(), "premium");
}

#[test]
fn kill_switches_require_every_field() {
    let err = serde_json::from_value::<KillSwitches>(json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_code_interpreter": false
    }))
    .unwrap_err();
    assert!(err.to_string().contains("disable_images"), "{err}");
}

#[test]
fn default_model_prefers_is_default_then_first_enabled() {
    let s = snapshot(vec![
        entry("a", true, false),
        entry("b", false, true),
        entry("c", true, true),
    ]);
    assert_eq!(s.default_model().unwrap().id, "c");
    assert_eq!(s.enabled().count(), 2);
    assert_eq!(s.find("b").unwrap().id, "b");
    assert!(s.find("zzz").is_none());

    let s = snapshot(vec![
        entry("a", false, true),
        entry("b", true, false),
        entry("c", true, false),
    ]);
    assert_eq!(s.default_model().unwrap().id, "b");

    let s = snapshot(vec![entry("a", false, true)]);
    assert!(s.default_model().is_none());
}

#[test]
fn general_config_sections_are_required() {
    for key in [
        "api_params",
        "features",
        "tool_support",
        "supported_endpoints",
    ] {
        let mut v = minimal_entry_json("m");
        v["general_config"].as_object_mut().unwrap().remove(key);
        let err = serde_json::from_value::<ModelCatalogEntry>(v).unwrap_err();
        assert!(err.to_string().contains(key), "{key}: {err}");
    }
}

#[test]
fn api_params_require_stop_but_not_sampling_params() {
    let mut v = minimal_entry_json("m");
    v["general_config"]["api_params"] = json!({});
    let err = serde_json::from_value::<ModelCatalogEntry>(v).unwrap_err();
    assert!(err.to_string().contains("stop"), "{err}");

    let e: ModelCatalogEntry = serde_json::from_value(minimal_entry_json("m")).unwrap();
    let p = &e.general_config.api_params;
    assert!(p.temperature.is_none() && p.top_p.is_none() && p.extra_body.is_none());
}

#[test]
fn feature_flags_are_required() {
    let mut v = minimal_entry_json("m");
    v["general_config"]["tool_support"]
        .as_object_mut()
        .unwrap()
        .remove("mcp");
    assert!(serde_json::from_value::<ModelCatalogEntry>(v).is_err());
}
