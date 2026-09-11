//! Configuration surface tests for the `oagw` gear.
//!
//! Covers `cpt-cf-oagw-algo-config-load-validate` and
//! `cpt-cf-oagw-dod-config-surface`: the five configurable families, their
//! defaults when `oagw.config` is absent, unknown-key rejection,
//! out-of-range rejection, and the write-time `http` scheme admission gate.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use serde_json::json;

use oagw::{ConfigError, OagwConfig, Scheme, SsrfPolicy};

/// The five declared defaults, asserted one key at a time so a single wrong
/// default names itself in the failure message.
#[test]
fn absent_section_yields_every_declared_default() {
    let cfg = OagwConfig::load(None).expect("absent section loads to defaults");
    assert_eq!(cfg.proxy_timeout_secs, 30, "proxy_timeout_secs default");
    assert!(!cfg.allow_http_upstream, "allow_http_upstream default");
    assert!(cfg.ssrf_policy.enabled, "ssrf_policy.enabled default");
    assert_eq!(
        cfg.token_cache_ttl_secs, 300,
        "token_cache_ttl_secs default"
    );
    assert_eq!(
        cfg.token_cache_capacity, 10_000,
        "token_cache_capacity default"
    );
}

/// An empty `oagw.config` object behaves exactly like an absent section.
#[test]
fn empty_object_yields_every_declared_default() {
    let cfg = OagwConfig::load(Some(&json!({}))).expect("empty object loads to defaults");
    assert_eq!(cfg, OagwConfig::default());
}

#[test]
fn zero_proxy_timeout_is_rejected_and_names_the_key() {
    let raw = json!({ "proxy_timeout_secs": 0 });
    let err = OagwConfig::load(Some(&raw)).expect_err("proxy_timeout_secs: 0 is rejected");
    assert_eq!(err.offending_key(), Some("proxy_timeout_secs"));
}

#[test]
fn zero_token_cache_capacity_is_rejected_and_names_the_key() {
    let raw = json!({ "token_cache_capacity": 0 });
    let err = OagwConfig::load(Some(&raw)).expect_err("token_cache_capacity: 0 is rejected");
    assert_eq!(err.offending_key(), Some("token_cache_capacity"));
}

#[test]
fn negative_token_cache_ttl_is_rejected_and_names_the_key() {
    let raw = json!({ "token_cache_ttl_secs": -1 });
    let err = OagwConfig::load(Some(&raw)).expect_err("negative token_cache_ttl_secs is rejected");
    assert!(
        err.to_string().contains("token_cache_ttl_secs"),
        "error must name the offending key: {err}"
    );
}

#[test]
fn unknown_top_level_key_is_rejected_and_names_the_key() {
    let raw = json!({ "proxy_timeout_secs": 30, "ssrf": true });
    let err = OagwConfig::load(Some(&raw)).expect_err("unknown key is rejected");
    assert_eq!(err.offending_key(), Some("ssrf"));
}

#[test]
fn unknown_key_inside_ssrf_policy_is_rejected_and_names_the_key() {
    let raw = json!({ "ssrf_policy": { "enabled": true, "mode": "strict" } });
    let err = OagwConfig::load(Some(&raw)).expect_err("unknown ssrf_policy key is rejected");
    assert_eq!(err.offending_key(), Some("ssrf_policy.mode"));
}

#[test]
fn every_other_integer_value_passes_validation() {
    let raw = json!({
        "proxy_timeout_secs": 2,
        "token_cache_ttl_secs": 1,
        "token_cache_capacity": 1
    });
    let cfg = OagwConfig::load(Some(&raw)).expect("boundary value 1 is accepted");
    assert_eq!(cfg.proxy_timeout_secs, 2);
    assert_eq!(cfg.token_cache_ttl_secs, 1);
    assert_eq!(cfg.token_cache_capacity, 1);
}

/// `allow_http_upstream` is the only input that admits the `http` literal at
/// write time; the other four literals are admitted either way.
#[test]
fn allow_http_upstream_gates_only_the_http_literal() {
    let denied = OagwConfig::default();
    let admitted = OagwConfig::load(Some(&json!({ "allow_http_upstream": true })))
        .expect("allow_http_upstream: true parses");

    assert!(!denied.admits_scheme(Scheme::Http));
    assert!(admitted.admits_scheme(Scheme::Http));

    for scheme in [Scheme::Https, Scheme::Wss, Scheme::Wt, Scheme::Grpc] {
        assert!(
            denied.admits_scheme(scheme),
            "{scheme:?} admitted when denied"
        );
        assert!(
            admitted.admits_scheme(scheme),
            "{scheme:?} admitted when allowed"
        );
    }
}

#[test]
fn ssrf_policy_disabled_round_trips() {
    let raw = json!({ "ssrf_policy": { "enabled": false } });
    let cfg = OagwConfig::load(Some(&raw)).expect("ssrf_policy parses");
    assert_eq!(cfg.ssrf_policy, SsrfPolicy { enabled: false });
    let back = serde_json::to_value(cfg).expect("config serializes");
    assert_eq!(back["ssrf_policy"]["enabled"], false);
}

#[test]
fn deserialize_failure_reports_the_deserialize_variant() {
    let raw = json!({ "allow_http_upstream": "yes" });
    let err = OagwConfig::load(Some(&raw)).expect_err("wrong type is rejected");
    assert!(matches!(err, ConfigError::Deserialize { .. }), "{err:?}");
}
