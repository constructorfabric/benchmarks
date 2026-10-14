//! Wire shape of the Models API DTOs and the projection mapping.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelTier;
use serde_json::json;

use super::{ModelDto, ModelListDto, ModelTierDto};
use crate::domain::model_visibility::VisibleModel;

fn dto(description: Option<&str>) -> ModelDto {
    ModelDto {
        model_id: "gpt-5.2".to_owned(),
        display_name: "GPT-5.2".to_owned(),
        tier: ModelTierDto::Premium,
        multiplier_display: "1x".to_owned(),
        description: description.map(str::to_owned),
        multimodal_capabilities: vec!["VISION_INPUT".to_owned()],
        context_window: 128_000,
    }
}

#[test]
fn the_dto_serializes_the_documented_shape() {
    let value = serde_json::to_value(dto(Some("Best for complex reasoning tasks"))).unwrap();
    assert_eq!(
        value,
        json!({
            "model_id": "gpt-5.2",
            "display_name": "GPT-5.2",
            "tier": "premium",
            "multiplier_display": "1x",
            "description": "Best for complex reasoning tasks",
            "multimodal_capabilities": ["VISION_INPUT"],
            "context_window": 128_000
        })
    );
}

#[test]
fn the_description_is_omitted_when_absent() {
    let value = serde_json::to_value(dto(None)).unwrap();
    assert!(value.get("description").is_none());
}

#[test]
fn the_tier_is_lowercase_on_the_wire() {
    assert_eq!(
        serde_json::to_value(ModelTierDto::Standard).unwrap(),
        json!("standard")
    );
    assert_eq!(
        serde_json::to_value(ModelTierDto::Premium).unwrap(),
        json!("premium")
    );
}

#[test]
fn the_list_dto_has_a_single_items_array() {
    let value = serde_json::to_value(ModelListDto { items: vec![] }).unwrap();
    assert_eq!(value, json!({ "items": [] }));
}

#[test]
fn a_visible_model_maps_to_the_public_dto() {
    let visible = VisibleModel {
        model_id: "m".to_owned(),
        display_name: "M".to_owned(),
        tier: ModelTier::Standard,
        multiplier_display: "1x".to_owned(),
        description: None,
        multimodal_capabilities: vec![],
        context_window: 8000,
    };
    let mapped = ModelDto::from(visible);
    assert_eq!(mapped.model_id, "m");
    assert_eq!(mapped.tier, ModelTierDto::Standard);
    assert_eq!(mapped.context_window, 8000);
    assert_eq!(mapped.description, None);
}
