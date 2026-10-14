//! Static audit plugin config tests (DESIGN Appendix B.1, `static-mini-chat-audit-plugin`).
//!
//! Slice S2: algorithm `static-audit-plugin`, steps `inst-static-audit-load` and
//! `inst-static-audit-defaults`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;

use super::StaticAuditConfig;

#[test]
fn the_defaults_of_an_absent_section() {
    let config = StaticAuditConfig::default();
    assert!(config.enabled);
    assert_eq!(config.vendor, "constructorfabric");
    assert_eq!(config.priority, 100);
}

#[test]
fn an_empty_section_takes_every_default() {
    assert_eq!(
        StaticAuditConfig::parse(json!({})).unwrap(),
        StaticAuditConfig::default()
    );
}

#[test]
fn every_key_can_be_set() {
    let config = StaticAuditConfig::parse(json!({
        "enabled": false, "vendor": "acme", "priority": -5
    }))
    .unwrap();
    assert!(!config.enabled);
    assert_eq!(config.vendor, "acme");
    assert_eq!(config.priority, -5);
}

#[test]
fn a_partial_section_keeps_the_other_defaults() {
    let config = StaticAuditConfig::parse(json!({ "enabled": false })).unwrap();
    assert!(!config.enabled);
    assert_eq!(config.vendor, "constructorfabric");
    assert_eq!(config.priority, 100);
}

#[test]
fn an_unknown_top_level_key_is_rejected_naming_it() {
    for config in [
        json!({ "bogus_key": 1 }),
        json!({ "enabled": true, "verbose": true }),
        json!({ "model_catalog": [] }),
    ] {
        let error = StaticAuditConfig::parse(config.clone()).unwrap_err();
        let key = config
            .as_object()
            .unwrap()
            .keys()
            .find(|k| *k != "enabled")
            .unwrap()
            .clone();
        assert!(error.to_string().contains(&key), "{config}: {error}");
    }
}

#[test]
fn a_value_of_the_wrong_type_or_range_is_rejected() {
    for config in [
        json!({ "enabled": "yes" }),
        json!({ "vendor": 7 }),
        json!({ "priority": "high" }),
        json!({ "priority": 40_000 }),
    ] {
        assert!(
            StaticAuditConfig::parse(config.clone()).is_err(),
            "{config}"
        );
    }
}
