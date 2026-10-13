#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the configuration part of
//! `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`
//! (step `inst-parse-audit-config`).

use serde_json::json;

use super::{StaticAuditConfig, StaticAuditConfigError};

#[test]
fn absent_section_uses_the_defaults() {
    let cfg = StaticAuditConfig::load(None).expect("absent section is valid");

    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn an_empty_section_uses_the_defaults() {
    let cfg = StaticAuditConfig::load(Some(&json!({}))).expect("empty section is valid");

    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn explicit_values_override_the_defaults() {
    let cfg = StaticAuditConfig::load(Some(&json!({
        "enabled": false,
        "vendor": "acme",
        "priority": 7
    })))
    .expect("valid section");

    assert!(!cfg.enabled);
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, 7);
}

#[test]
fn unknown_top_level_keys_are_rejected() {
    let err = StaticAuditConfig::load(Some(&json!({ "enabeld": true })))
        .expect_err("unknown key must fail");

    assert!(
        matches!(err, StaticAuditConfigError::Parse(_)),
        "got {err:?}"
    );
    assert!(err.to_string().contains("enabeld"), "{err}");
}

#[test]
fn a_wrongly_typed_value_is_rejected() {
    StaticAuditConfig::load(Some(&json!({ "enabled": true }))).expect("control is valid");

    let err = StaticAuditConfig::load(Some(&json!({ "enabled": "yes" })))
        .expect_err("wrong type must fail");

    assert!(
        matches!(err, StaticAuditConfigError::Parse(_)),
        "got {err:?}"
    );
}
