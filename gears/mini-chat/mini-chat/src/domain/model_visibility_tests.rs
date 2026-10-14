//! Model visibility and projection (algorithm `model-visibility`, DESIGN 3.3 `Models API`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{KillSwitches, ModelTier, PolicySnapshot};
use serde_json::json;

use super::{VisibleModel, find_visible_model, visible_models};
use crate::test_support::{entry, entry_json, snapshot};

fn mixed_catalog() -> PolicySnapshot {
    snapshot(vec![
        entry("gpt-premium", ModelTier::Premium, true, &json!({})),
        entry("gpt-disabled", ModelTier::Standard, false, &json!({})),
        entry("gpt-standard", ModelTier::Standard, true, &json!({})),
    ])
}

fn ids(models: &[VisibleModel]) -> Vec<&str> {
    models.iter().map(|m| m.model_id.as_str()).collect()
}

#[test]
fn only_globally_enabled_entries_are_visible_in_catalog_order() {
    let models = visible_models(&mixed_catalog());
    assert_eq!(ids(&models), vec!["gpt-premium", "gpt-standard"]);
}

#[test]
fn an_empty_catalog_yields_an_empty_list() {
    assert!(visible_models(&snapshot(vec![])).is_empty());
}

#[test]
fn an_entry_that_omits_enabled_is_not_visible() {
    let mut value = entry_json("no-flag", "standard", true);
    value.as_object_mut().unwrap().remove("enabled");
    let omitted = serde_json::from_value(value).unwrap();
    let snap = snapshot(vec![omitted]);
    assert!(visible_models(&snap).is_empty());
    assert!(find_visible_model(&snap, "no-flag").is_none());
}

#[test]
fn the_projection_carries_the_seven_public_fields() {
    let patch = json!({
        "display_name": "GPT-5.2",
        "description": "Best for complex reasoning tasks",
        "multiplier_display": "2.5x",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128_000
    });
    let snap = snapshot(vec![entry("gpt-5.2", ModelTier::Premium, true, &patch)]);
    let model = visible_models(&snap).pop().unwrap();
    assert_eq!(
        model,
        VisibleModel {
            model_id: "gpt-5.2".to_owned(),
            display_name: "GPT-5.2".to_owned(),
            tier: ModelTier::Premium,
            multiplier_display: "2.5x".to_owned(),
            description: Some("Best for complex reasoning tasks".to_owned()),
            multimodal_capabilities: vec!["VISION_INPUT".to_owned(), "RAG".to_owned()],
            context_window: 128_000,
        }
    );
}

#[test]
fn the_description_is_absent_when_none_is_configured() {
    let snap = snapshot(vec![entry("m", ModelTier::Standard, true, &json!({}))]);
    assert_eq!(visible_models(&snap).pop().unwrap().description, None);
}

#[test]
fn model_id_is_the_catalog_id_never_the_provider_model_name() {
    let patch = json!({ "provider_model_id": "gpt-5.2-azure-deployment-7" });
    let snap = snapshot(vec![entry(
        "internal-slug",
        ModelTier::Premium,
        true,
        &patch,
    )]);
    let model = visible_models(&snap).pop().unwrap();
    assert_eq!(model.model_id, "internal-slug");
    assert!(find_visible_model(&snap, "gpt-5.2-azure-deployment-7").is_none());
}

#[test]
fn a_single_model_is_found_by_its_id() {
    let model = find_visible_model(&mixed_catalog(), "gpt-standard").unwrap();
    assert_eq!(model.model_id, "gpt-standard");
    assert_eq!(model.tier, ModelTier::Standard);
}

#[test]
fn a_disabled_or_unknown_model_is_indistinguishable() {
    let snap = mixed_catalog();
    assert!(find_visible_model(&snap, "gpt-disabled").is_none());
    assert!(find_visible_model(&snap, "never-existed").is_none());
    assert!(find_visible_model(&snap, "").is_none());
}

#[test]
fn model_ids_are_opaque_and_matched_exactly() {
    let uuid_like = "3f2b8c1e-0a41-4c6e-9d7b-5a1e2f3c4d5e";
    let snap = snapshot(vec![
        entry(uuid_like, ModelTier::Standard, true, &json!({})),
        entry("gpt-5.2/preview", ModelTier::Premium, true, &json!({})),
    ]);
    assert!(find_visible_model(&snap, uuid_like).is_some());
    assert!(find_visible_model(&snap, "gpt-5.2/preview").is_some());
    assert!(find_visible_model(&snap, "GPT-5.2/PREVIEW").is_none());
    assert!(find_visible_model(&snap, "gpt-5.2").is_none());
}

#[test]
fn kill_switches_do_not_change_visibility() {
    let mut snap = mixed_catalog();
    snap.kill_switches = KillSwitches {
        disable_premium_tier: true,
        force_standard_tier: true,
        ..KillSwitches::default()
    };
    assert_eq!(
        ids(&visible_models(&snap)),
        vec!["gpt-premium", "gpt-standard"]
    );
}
