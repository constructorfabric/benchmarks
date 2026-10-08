//! Catalog fixtures shared by the domain unit tests.

use mini_chat_sdk::{ModelCatalogEntry, ModelPreference, ModelTier};
use serde_json::json;

/// Enabled catalog entry: multipliers 1 / 3 micro-credits per token
/// (`1_000_000` / `3_000_000`), `max_output_tokens` 4096, default
/// estimation budgets (4 / 100 / 10 / 1000 / 500 / 500 / 1000), every tool
/// supported.
pub fn entry(id: &str, tier: ModelTier) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id,
        "provider_model_id": format!("{id}-provider"),
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier.as_str(),
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "chat",
            "available_from": "",
            "max_file_size_mb": 25,
            "api_params": {"stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true, "image_generation": false,
                             "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true, "embeddings": false,
                                    "image_generation": false, "audio_speech_generation": false,
                                    "audio_transcription": false, "audio_translation": false}
        },
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "enabled": true
    }))
    .expect("catalog entry fixture")
}

pub fn premium(id: &str) -> ModelCatalogEntry {
    entry(id, ModelTier::Premium)
}

pub fn standard(id: &str) -> ModelCatalogEntry {
    entry(id, ModelTier::Standard)
}

/// `e` with `preference.is_default = true`.
pub fn default_of_tier(mut e: ModelCatalogEntry) -> ModelCatalogEntry {
    e.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    e
}

/// `e` with `enabled = false`.
pub fn disabled(mut e: ModelCatalogEntry) -> ModelCatalogEntry {
    e.enabled = false;
    e
}
