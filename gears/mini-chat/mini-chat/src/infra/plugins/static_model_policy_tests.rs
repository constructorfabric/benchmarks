#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::StaticModelPolicyConfig;

#[test]
fn section_without_model_catalog_is_rejected() {
    let cfg: StaticModelPolicyConfig =
        serde_json::from_value(serde_json::json!({ "vendor": "constructorfabric" })).unwrap();
    assert!(cfg.model_catalog.is_none());
    let err = cfg.validate().unwrap_err();
    assert!(err.contains("model_catalog is required"), "{err}");
}

#[test]
fn empty_section_is_rejected() {
    let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn absent_section_defaults_to_an_empty_catalog() {
    let cfg = StaticModelPolicyConfig::default();
    assert_eq!(cfg.model_catalog.as_ref().map(Vec::len), Some(0));
    assert!(cfg.validate().is_ok());
}

#[test]
fn explicit_empty_catalog_is_accepted() {
    let cfg: StaticModelPolicyConfig =
        serde_json::from_value(serde_json::json!({ "model_catalog": [] })).unwrap();
    assert!(cfg.validate().is_ok());
}
