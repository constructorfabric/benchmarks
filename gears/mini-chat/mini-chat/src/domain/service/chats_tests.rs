#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1 of `cpt-cf-mini-chat-feature-chat-crud`: title validation
//! (`cpt-cf-mini-chat-algo-chat-crud-validate-title`) and chat model resolution
//! (`cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model`), as pure functions.

use mini_chat_sdk::ModelCatalogEntry;

use super::{
    DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT, TITLE_MAX_CHARS, resolve_chat_model, validate_title,
};
use crate::chat_test_env::catalog_entry;
use crate::domain::error::DomainError;
use crate::test_doubles::{entry, entry_json};

fn title(text: &str) -> Result<String, DomainError> {
    validate_title(text)
}

fn assert_invalid_title(raw: &str) {
    match validate_title(raw) {
        Err(DomainError::InvalidTitle) => {}
        other => panic!("`{raw:?}` must be an invalid title, got {other:?}"),
    }
}

// ---- constants -----------------------------------------------------------------------------------------

#[test]
fn the_title_and_page_limits_match_the_feature() {
    assert_eq!(TITLE_MAX_CHARS, 255);
    assert_eq!(DEFAULT_PAGE_LIMIT, 20);
    assert_eq!(MAX_PAGE_LIMIT, 100);
}

// ---- validate_title ------------------------------------------------------------------------------------------

#[test]
fn a_plain_title_is_returned_unchanged() {
    assert_eq!(
        title("Q3 Financial Analysis").unwrap(),
        "Q3 Financial Analysis"
    );
}

#[test]
fn leading_and_trailing_whitespace_is_removed_and_the_trimmed_value_is_returned() {
    assert_eq!(title("  Q3  ").unwrap(), "Q3");
    assert_eq!(title("\t\n Q3 \r\n").unwrap(), "Q3");
}

#[test]
fn unicode_whitespace_is_trimmed() {
    // no-break space, em space, ideographic space, line separator
    assert_eq!(
        title("\u{00A0}\u{2003}\u{3000}Q3\u{2028}\u{3000}").unwrap(),
        "Q3"
    );
}

#[test]
fn inner_whitespace_is_preserved() {
    assert_eq!(title("  a   b \t c ").unwrap(), "a   b \t c");
}

#[test]
fn an_empty_title_is_invalid() {
    assert_invalid_title("");
}

#[test]
fn a_whitespace_only_title_is_invalid() {
    assert_invalid_title("   ");
    assert_invalid_title("\t\n\r");
    assert_invalid_title("\u{00A0}\u{3000}");
}

#[test]
fn a_title_of_exactly_255_characters_is_valid() {
    let text = "a".repeat(255);

    assert_eq!(title(&text).unwrap(), text);
}

#[test]
fn a_title_of_256_characters_is_invalid() {
    assert_invalid_title(&"a".repeat(256));
}

#[test]
fn the_length_is_counted_after_the_trim() {
    let padded_valid = format!("  {}  ", "a".repeat(255));
    let padded_invalid = format!("  {}  ", "a".repeat(256));

    assert_eq!(title(&padded_valid).unwrap(), "a".repeat(255));
    assert_invalid_title(&padded_invalid);
}

#[test]
fn the_length_is_counted_in_unicode_scalar_values_not_bytes() {
    let two_byte = "\u{e9}".repeat(255);
    let four_byte = "\u{1f600}".repeat(255);

    assert_eq!(two_byte.len(), 510);
    assert_eq!(four_byte.len(), 1020);
    assert_eq!(title(&two_byte).unwrap(), two_byte);
    assert_eq!(title(&four_byte).unwrap(), four_byte);
    assert_invalid_title(&"\u{e9}".repeat(256));
    assert_invalid_title(&"\u{1f600}".repeat(256));
}

#[test]
fn a_one_character_title_is_valid() {
    assert_eq!(title("x").unwrap(), "x");
    assert_eq!(title(" \u{1f600} ").unwrap(), "\u{1f600}");
}

// ---- resolve_chat_model: a given model -------------------------------------------------------------------------------

fn catalog() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry("model-off", false, Some(true)),
        catalog_entry("model-a", true, None),
        catalog_entry("Model-B", true, Some(false)),
    ]
}

fn resolve(requested: Option<&str>, catalog: &[ModelCatalogEntry]) -> Result<String, DomainError> {
    resolve_chat_model(requested, catalog)
}

fn assert_invalid_model(result: Result<String, DomainError>) {
    match result {
        Err(DomainError::InvalidModel) => {}
        other => panic!("expected InvalidModel, got {other:?}"),
    }
}

#[test]
fn a_given_enabled_model_is_returned() {
    assert_eq!(resolve(Some("model-a"), &catalog()).unwrap(), "model-a");
    assert_eq!(resolve(Some("Model-B"), &catalog()).unwrap(), "Model-B");
}

#[test]
fn a_given_model_need_not_be_the_default() {
    let catalog = vec![
        catalog_entry("first", true, Some(true)),
        catalog_entry("second", true, Some(false)),
    ];

    assert_eq!(resolve(Some("second"), &catalog).unwrap(), "second");
}

#[test]
fn a_given_disabled_model_is_invalid() {
    assert_invalid_model(resolve(Some("model-off"), &catalog()));
}

#[test]
fn a_given_unknown_model_is_invalid() {
    assert_invalid_model(resolve(Some("no-such-model"), &catalog()));
}

#[test]
fn the_given_model_is_matched_case_sensitively() {
    assert_invalid_model(resolve(Some("Model-A"), &catalog()));
    assert_invalid_model(resolve(Some("model-b"), &catalog()));
}

#[test]
fn the_given_model_is_not_trimmed() {
    assert_invalid_model(resolve(Some(" model-a"), &catalog()));
    assert_invalid_model(resolve(Some("model-a "), &catalog()));
}

#[test]
fn an_empty_given_model_is_invalid_and_not_treated_as_absent() {
    assert_invalid_model(resolve(Some(""), &catalog()));
}

#[test]
fn a_given_model_is_invalid_for_an_empty_catalog() {
    assert_invalid_model(resolve(Some("model-a"), &[]));
}

#[test]
fn the_catalog_ids_are_compared_with_the_id_and_not_the_provider_model_id() {
    let mut value = entry_json("internal-id", true);
    value["provider_model_id"] = serde_json::json!("provider-name");
    let catalog = vec![entry(value)];

    assert_eq!(
        resolve(Some("internal-id"), &catalog).unwrap(),
        "internal-id"
    );
    assert_invalid_model(resolve(Some("provider-name"), &catalog));
}

// ---- resolve_chat_model: the default model ----------------------------------------------------------------------------

#[test]
fn without_a_model_the_first_enabled_default_entry_is_chosen() {
    let catalog = vec![
        catalog_entry("plain", true, None),
        catalog_entry("not-default", true, Some(false)),
        catalog_entry("default-one", true, Some(true)),
        catalog_entry("default-two", true, Some(true)),
    ];

    assert_eq!(resolve(None, &catalog).unwrap(), "default-one");
}

#[test]
fn a_disabled_default_entry_is_skipped() {
    let catalog = vec![
        catalog_entry("off-default", false, Some(true)),
        catalog_entry("plain", true, Some(false)),
        catalog_entry("on-default", true, Some(true)),
    ];

    assert_eq!(resolve(None, &catalog).unwrap(), "on-default");
}

#[test]
fn without_an_enabled_default_the_first_enabled_entry_is_chosen() {
    let catalog = vec![
        catalog_entry("off", false, Some(true)),
        catalog_entry("first-enabled", true, None),
        catalog_entry("second-enabled", true, Some(false)),
    ];

    assert_eq!(resolve(None, &catalog).unwrap(), "first-enabled");
}

#[test]
fn an_absent_preference_counts_as_not_default() {
    let catalog = vec![
        catalog_entry("no-preference", true, None),
        catalog_entry("flagged", true, Some(true)),
    ];

    assert_eq!(resolve(None, &catalog).unwrap(), "flagged");
}

#[test]
fn the_catalog_order_decides_and_sort_order_and_tier_do_not() {
    let mut early = entry_json("early", true);
    early["preference"] = serde_json::json!({ "is_default": true, "sort_order": 99 });
    early["tier"] = serde_json::json!("standard");
    let mut late = entry_json("late", true);
    late["preference"] = serde_json::json!({ "is_default": true, "sort_order": 1 });
    late["tier"] = serde_json::json!("premium");

    let result = resolve(None, &[entry(early), entry(late)]);

    assert_eq!(result.unwrap(), "early");
}

#[test]
fn an_empty_catalog_has_no_default() {
    assert_invalid_model(resolve(None, &[]));
}

#[test]
fn a_catalog_without_an_enabled_entry_has_no_default() {
    let catalog = vec![
        catalog_entry("a", false, Some(true)),
        catalog_entry("b", false, None),
    ];

    assert_invalid_model(resolve(None, &catalog));
}
