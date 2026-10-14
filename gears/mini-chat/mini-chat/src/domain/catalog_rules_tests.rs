//! Catalog rules (algorithm `catalog-rules`, DESIGN 4 `Model Catalog Configuration`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{KillSwitches, ModelTier, PolicySnapshot};
use serde_json::json;

use super::{
    DOWNGRADE_TIER_ORDER, default_model, kill_switches, supports_images, tier_cascade,
    validate_selected_model,
};
use crate::domain::error::DomainError;
use crate::test_support::{entry, entry_json, snapshot};

const DEFAULT: &str = "preference";

fn default_flag(is_default: bool) -> serde_json::Value {
    json!({ DEFAULT: { "is_default": is_default, "sort_order": 0 } })
}

fn ids_of(snap: &PolicySnapshot) -> Vec<&str> {
    snap.model_catalog.iter().map(|e| e.id.as_str()).collect()
}

#[test]
fn the_downgrade_tier_order_is_premium_then_standard() {
    assert_eq!(
        DOWNGRADE_TIER_ORDER,
        [ModelTier::Premium, ModelTier::Standard]
    );
}

#[test]
fn the_default_model_is_the_first_enabled_entry_flagged_default() {
    let snap = snapshot(vec![
        entry("first", ModelTier::Premium, true, &default_flag(false)),
        entry("second", ModelTier::Standard, true, &default_flag(true)),
        entry("third", ModelTier::Standard, true, &default_flag(true)),
    ]);
    assert_eq!(default_model(&snap).unwrap().id, "second");
}

#[test]
fn without_a_default_flag_the_first_enabled_entry_is_the_default() {
    let snap = snapshot(vec![
        entry("off", ModelTier::Premium, false, &json!({})),
        entry("first-enabled", ModelTier::Standard, true, &json!({})),
        entry("second-enabled", ModelTier::Premium, true, &json!({})),
    ]);
    assert_eq!(default_model(&snap).unwrap().id, "first-enabled");
}

#[test]
fn a_disabled_default_entry_is_skipped() {
    let snap = snapshot(vec![
        entry(
            "disabled-default",
            ModelTier::Premium,
            false,
            &default_flag(true),
        ),
        entry("enabled", ModelTier::Standard, true, &default_flag(false)),
    ]);
    assert_eq!(default_model(&snap).unwrap().id, "enabled");
}

#[test]
fn the_tier_is_not_considered_when_picking_the_default() {
    let snap = snapshot(vec![
        entry("premium-first", ModelTier::Premium, true, &json!({})),
        entry(
            "standard-default",
            ModelTier::Standard,
            true,
            &default_flag(true),
        ),
    ]);
    assert_eq!(default_model(&snap).unwrap().id, "standard-default");
    let snap = snapshot(vec![
        entry("standard-first", ModelTier::Standard, true, &json!({})),
        entry("premium-second", ModelTier::Premium, true, &json!({})),
    ]);
    assert_eq!(default_model(&snap).unwrap().id, "standard-first");
}

#[test]
fn there_is_no_default_when_nothing_is_enabled() {
    assert!(default_model(&snapshot(vec![])).is_none());
    let snap = snapshot(vec![entry(
        "off",
        ModelTier::Standard,
        false,
        &default_flag(true),
    )]);
    assert!(default_model(&snap).is_none());
}

#[test]
fn a_selected_model_must_be_present_and_enabled() {
    let snap = snapshot(vec![
        entry("on", ModelTier::Standard, true, &json!({})),
        entry("off", ModelTier::Standard, false, &json!({})),
    ]);
    assert_eq!(validate_selected_model(&snap, "on").unwrap().id, "on");
    for bad in ["off", "ghost", ""] {
        match validate_selected_model(&snap, bad) {
            Err(DomainError::InvalidModel { model }) => assert_eq!(model, bad),
            other => panic!("`{bad}` must be INVALID_MODEL, got {other:?}"),
        }
    }
}

#[test]
fn the_cascade_lists_the_tiers_that_have_an_enabled_entry_in_fixed_order() {
    let both = snapshot(vec![
        entry("s", ModelTier::Standard, true, &json!({})),
        entry("p", ModelTier::Premium, true, &json!({})),
    ]);
    assert_eq!(
        tier_cascade(&both),
        vec![ModelTier::Premium, ModelTier::Standard]
    );

    let standard_only = snapshot(vec![entry("s", ModelTier::Standard, true, &json!({}))]);
    assert_eq!(tier_cascade(&standard_only), vec![ModelTier::Standard]);

    let premium_only = snapshot(vec![entry("p", ModelTier::Premium, true, &json!({}))]);
    assert_eq!(tier_cascade(&premium_only), vec![ModelTier::Premium]);
}

#[test]
fn a_tier_with_only_disabled_entries_is_skipped_in_the_cascade() {
    let snap = snapshot(vec![
        entry("p-off", ModelTier::Premium, false, &json!({})),
        entry("s-on", ModelTier::Standard, true, &json!({})),
    ]);
    assert_eq!(tier_cascade(&snap), vec![ModelTier::Standard]);
    assert!(tier_cascade(&snapshot(vec![])).is_empty());
}

#[test]
fn an_entry_without_an_enabled_flag_counts_as_disabled_everywhere() {
    let mut value = entry_json("quiet", "premium", true);
    value.as_object_mut().unwrap().remove("enabled");
    let snap = snapshot(vec![serde_json::from_value(value).unwrap()]);
    assert!(default_model(&snap).is_none());
    assert!(tier_cascade(&snap).is_empty());
    assert!(matches!(
        validate_selected_model(&snap, "quiet"),
        Err(DomainError::InvalidModel { .. })
    ));
    assert_eq!(ids_of(&snap), vec!["quiet"]);
}

#[test]
fn image_capability_is_resolved_from_vision_input() {
    let vision = entry(
        "v",
        ModelTier::Standard,
        true,
        &json!({ "multimodal_capabilities": ["VISION_INPUT"] }),
    );
    let both = entry(
        "b",
        ModelTier::Standard,
        true,
        &json!({ "multimodal_capabilities": ["RAG", "VISION_INPUT"] }),
    );
    let rag_only = entry(
        "r",
        ModelTier::Standard,
        true,
        &json!({ "multimodal_capabilities": ["RAG"] }),
    );
    let none = entry("n", ModelTier::Standard, true, &json!({}));
    assert!(supports_images(&vision));
    assert!(supports_images(&both));
    assert!(!supports_images(&rag_only));
    assert!(!supports_images(&none));
}

#[test]
fn the_six_kill_switches_are_exposed_from_the_snapshot() {
    let mut snap = snapshot(vec![]);
    assert_eq!(kill_switches(&snap), KillSwitches::default());
    snap.kill_switches = KillSwitches {
        disable_premium_tier: true,
        force_standard_tier: false,
        disable_web_search: true,
        disable_file_search: false,
        disable_images: true,
        disable_code_interpreter: false,
    };
    let exposed = kill_switches(&snap);
    assert!(exposed.disable_premium_tier);
    assert!(!exposed.force_standard_tier);
    assert!(exposed.disable_web_search);
    assert!(!exposed.disable_file_search);
    assert!(exposed.disable_images);
    assert!(!exposed.disable_code_interpreter);
}
