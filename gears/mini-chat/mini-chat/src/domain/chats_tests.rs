//! Unit tests of the pure rules of the chats service: title validation, model
//! resolution and the default list order.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::json;

use super::*;

fn entry(id: &str, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "p",
        "provider_display_name": "P",
        "tier": "Standard",
        "enabled": enabled,
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 1_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0,
                           "presence_penalty": 0.0, "stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true,
                             "image_generation": false, "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true,
                                    "embeddings": false, "image_generation": false,
                                    "audio_speech_generation": false,
                                    "audio_transcription": false, "audio_translation": false}
        },
        "preference": {"is_default": is_default, "sort_order": 0},
    }))
    .expect("catalog entry")
}

#[test]
fn title_is_trimmed_and_counted_in_characters() {
    assert_eq!(validate_title("  hi  ").unwrap(), "hi");
    assert!(validate_title(&"\u{e9}".repeat(TITLE_MAX_CHARS)).is_ok());
    assert!(matches!(
        validate_title(&"\u{e9}".repeat(TITLE_MAX_CHARS + 1)),
        Err(DomainError::InvalidTitle { .. })
    ));
}

#[test]
fn blank_titles_are_rejected() {
    for raw in ["", "   ", "\t\n"] {
        assert!(matches!(
            validate_title(raw),
            Err(DomainError::InvalidTitle { .. })
        ));
    }
}

#[test]
fn model_resolution_prefers_the_enabled_default_then_the_first_enabled() {
    let catalog = [
        entry("off-default", false, true),
        entry("first", true, false),
        entry("default", true, true),
    ];
    assert_eq!(resolve_model(&catalog, None).unwrap(), "default");
    assert_eq!(resolve_model(&catalog[..2], None).unwrap(), "first");
    assert_eq!(resolve_model(&catalog, Some("first")).unwrap(), "first");
}

#[test]
fn model_resolution_rejects_disabled_unknown_and_missing_models() {
    let catalog = [entry("off", false, true)];
    assert!(matches!(
        resolve_model(&catalog, Some("off")),
        Err(DomainError::InvalidModel)
    ));
    assert!(matches!(
        resolve_model(&catalog, Some("nope")),
        Err(DomainError::InvalidModel)
    ));
    assert!(matches!(
        resolve_model(&catalog, None),
        Err(DomainError::InvalidModel)
    ));
}

#[test]
fn default_order_applies_only_without_orderby_and_cursor() {
    let plain = with_default_order(&ODataQuery::default(), "updated_at", SortDir::Desc);
    assert_eq!(plain.order.0.len(), 1);
    assert_eq!(plain.order.0[0].field, "updated_at");

    let explicit = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey {
        field: "title".to_owned(),
        dir: SortDir::Asc,
    }]));
    let kept = with_default_order(&explicit, "updated_at", SortDir::Desc);
    assert_eq!(kept.order.0[0].field, "title");
}
