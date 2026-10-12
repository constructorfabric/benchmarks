#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::json;

use super::{enabled_models, find_enabled_model, resolve_chat_model, select_default_model};
use crate::domain::error::DomainError;

fn entry(id: &str, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "Standard",
        "enabled": enabled,
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1,
        "output_tokens_credit_multiplier_micro": 1,
        "max_num_results": 5,
        "preference": { "is_default": is_default, "sort_order": 0 },
        "general_config": {
            "type": "", "available_from": "", "max_file_size_mb": 1,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": false, "file_search": false, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {}
        }
    }))
    .unwrap()
}

fn ids(catalog: &[ModelCatalogEntry]) -> Vec<&str> {
    enabled_models(catalog)
        .map(|entry| entry.id.as_str())
        .collect()
}

#[test]
fn only_enabled_models_are_visible() {
    let catalog = [
        entry("a", true, false),
        entry("b", false, false),
        entry("c", true, false),
    ];

    assert_eq!(ids(&catalog), vec!["a", "c"]);
    assert!(find_enabled_model(&catalog, "a").is_some());
    assert!(find_enabled_model(&catalog, "b").is_none());
    assert!(find_enabled_model(&catalog, "missing").is_none());
}

#[test]
fn the_default_model_is_the_first_enabled_default_entry_else_the_first_enabled_entry() {
    let flagged = [
        entry("a", true, false),
        entry("b", true, true),
        entry("c", true, true),
    ];
    let none_flagged = [
        entry("off", false, true),
        entry("x", true, false),
        entry("y", true, false),
    ];
    let nothing_enabled = [entry("off", false, true)];

    assert_eq!(select_default_model(&flagged).unwrap().id, "b");
    assert_eq!(select_default_model(&none_flagged).unwrap().id, "x");
    assert!(select_default_model(&nothing_enabled).is_none());
    assert!(select_default_model(&[]).is_none());
}

#[test]
fn a_requested_model_must_be_enabled() {
    let catalog = [entry("a", true, false), entry("b", false, false)];

    assert_eq!(resolve_chat_model(&catalog, Some("a")).unwrap(), "a");
    for refused in [Some("b"), Some("missing")] {
        assert!(matches!(
            resolve_chat_model(&catalog, refused),
            Err(DomainError::InvalidModel { .. })
        ));
    }
}

#[test]
fn without_a_requested_model_the_default_is_used_or_the_request_is_refused() {
    let catalog = [entry("a", true, false), entry("b", true, true)];

    assert_eq!(resolve_chat_model(&catalog, None).unwrap(), "b");
    assert!(matches!(
        resolve_chat_model(&[entry("off", false, true)], None),
        Err(DomainError::InvalidModel { .. })
    ));
}
