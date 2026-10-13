//! Bundled plugins: `static-mini-chat-model-policy-plugin` and
//! `static-mini-chat-audit-plugin` (DESIGN B.1 "Bundled plugins", 5.2.5).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::catalog_entry_json;
use mini_chat::infra::plugins::static_audit::{StaticAuditConfig, StaticAuditService};
use mini_chat::infra::plugins::static_model_policy::{
    StaticModelPolicyConfig, StaticModelPolicyService,
};
use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1, ModelTier,
    UsageEvent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn policy_cfg(yaml: &str) -> Result<StaticModelPolicyConfig, String> {
    serde_saphyr::from_str(yaml).map_err(|e| e.to_string())
}

fn audit_cfg(yaml: &str) -> Result<StaticAuditConfig, String> {
    serde_saphyr::from_str(yaml).map_err(|e| e.to_string())
}

fn entry_yaml(id: &str, multiplier: u64) -> String {
    let mut v = catalog_entry_json(id, "Standard", true);
    v["input_tokens_credit_multiplier_micro"] = json!(multiplier);
    v["output_tokens_credit_multiplier_micro"] = json!(multiplier);
    // JSON is valid YAML.
    v.to_string()
}

// ═════════════════════════ model-policy plugin: config ═════════════════════════

#[test]
fn model_policy_defaults() {
    let cfg = StaticModelPolicyConfig::default();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(
        cfg.model_catalog.is_empty(),
        "catalog is [] when the section is absent"
    );

    // all kill switches default to false
    let ks = serde_json::to_value(&cfg.kill_switches).unwrap();
    for (name, value) in ks.as_object().unwrap() {
        assert_eq!(
            value,
            &json!(false),
            "kill switch `{name}` defaults to false"
        );
    }
    assert_eq!(ks.as_object().unwrap().len(), 6);

    assert_eq!(
        cfg.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        cfg.default_standard_limits.limit_monthly_credits_micro,
        1_000_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_daily_credits_micro,
        50_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );
    cfg.validate().expect("the default configuration is valid");
}

#[test]
fn model_policy_section_with_only_the_required_key_gets_all_other_defaults() {
    let cfg = policy_cfg("model_catalog: []\n").unwrap();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert_eq!(
        cfg.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );
}

#[test]
fn model_policy_catalog_is_required_when_the_section_is_present() {
    assert!(policy_cfg("vendor: acme\n").is_err());
    assert!(policy_cfg("priority: 5\n").is_err());
    assert!(
        policy_cfg("model_catalog: []\n").is_ok(),
        "an empty list is valid"
    );
}

#[test]
fn model_policy_vendor_and_priority_are_configurable() {
    let cfg = policy_cfg("vendor: acme\npriority: -3\nmodel_catalog: []\n").unwrap();
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, -3);
}

#[test]
fn model_policy_rejects_unknown_top_level_keys() {
    assert!(policy_cfg("model_catalog: []\nbogus_key: 1\n").is_err());
}

#[test]
fn kill_switches_object_and_each_field_may_be_omitted() {
    let cfg = policy_cfg("model_catalog: []\nkill_switches: {}\n").unwrap();
    let ks = serde_json::to_value(&cfg.kill_switches).unwrap();
    assert!(ks.as_object().unwrap().values().all(|v| v == &json!(false)));

    let cfg = policy_cfg("model_catalog: []\nkill_switches:\n  disable_images: true\n").unwrap();
    let ks = serde_json::to_value(&cfg.kill_switches).unwrap();
    assert_eq!(ks["disable_images"], json!(true));
    assert_eq!(ks["disable_web_search"], json!(false));
}

#[test]
fn a_misspelled_kill_switch_is_a_config_error() {
    assert!(
        policy_cfg("model_catalog: []\nkill_switches:\n  disable_web_serach: true\n").is_err(),
        "the static plugin's kill_switches object rejects unknown keys"
    );
}

#[test]
fn all_six_kill_switches_are_recognised() {
    for name in [
        "disable_premium_tier",
        "force_standard_tier",
        "disable_web_search",
        "disable_file_search",
        "disable_images",
        "disable_code_interpreter",
    ] {
        let yaml = format!("model_catalog: []\nkill_switches:\n  {name}: true\n");
        let cfg = policy_cfg(&yaml).unwrap_or_else(|e| panic!("{name}: {e}"));
        let ks = serde_json::to_value(&cfg.kill_switches).unwrap();
        assert_eq!(ks[name], json!(true), "{name}");
    }
}

#[test]
fn nested_sdk_types_ignore_unknown_keys_inside_the_plugin_config() {
    let yaml = format!(
        "model_catalog:\n  - {}\ndefault_standard_limits:\n  limit_daily_credits_micro: 10\n  limit_monthly_credits_micro: 20\n  future_limit: 30\n",
        {
            let mut v = catalog_entry_json("m", "Standard", true);
            v["future_entry_key"] = json!("ignored");
            v.to_string()
        }
    );
    let cfg = policy_cfg(&yaml).expect("unknown keys inside SDK types are ignored");
    assert_eq!(cfg.model_catalog.len(), 1);
    assert_eq!(cfg.default_standard_limits.limit_daily_credits_micro, 10);
    assert_eq!(cfg.default_standard_limits.limit_monthly_credits_micro, 20);
}

#[test]
fn credit_multipliers_must_be_within_1_to_10_000_000_000() {
    for (multiplier, ok) in [
        (0_u64, false),
        (1, true),
        (3_000_000, true),
        (10_000_000_000, true),
        (10_000_000_001, false),
    ] {
        let yaml = format!("model_catalog:\n  - {}\n", entry_yaml("m", multiplier));
        let cfg = policy_cfg(&yaml).unwrap();
        assert_eq!(
            cfg.validate().is_ok(),
            ok,
            "multiplier {multiplier} validate() result"
        );
        assert_eq!(
            StaticModelPolicyService::from_config(&cfg).is_ok(),
            ok,
            "multiplier {multiplier}: the plugin refuses to initialise with an invalid catalog"
        );
    }
}

#[test]
fn both_multipliers_of_every_entry_are_checked() {
    let mut bad_output = catalog_entry_json("bad", "Premium", true);
    bad_output["output_tokens_credit_multiplier_micro"] = json!(0);
    let good = catalog_entry_json("good", "Standard", true);
    let yaml = format!("model_catalog:\n  - {good}\n  - {bad_output}\n");
    let cfg = policy_cfg(&yaml).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_bytes_per_token_conservative_fails_plugin_init() {
    let mut zero = catalog_entry_json("zero", "Standard", true);
    zero["estimation_budgets"] = json!({"bytes_per_token_conservative": 0});
    let yaml = format!("model_catalog:\n  - {zero}\n");
    let cfg = policy_cfg(&yaml).unwrap();
    assert!(cfg.validate().is_err());
    let err = StaticModelPolicyService::from_config(&cfg)
        .err()
        .expect("zero bytes_per_token_conservative is rejected at init");
    let msg = err.to_string();
    assert!(msg.contains("zero") && msg.contains("bytes_per_token_conservative"));

    let mut one = catalog_entry_json("one", "Standard", true);
    one["estimation_budgets"] = json!({"bytes_per_token_conservative": 1});
    let yaml = format!("model_catalog:\n  - {one}\n");
    let cfg = policy_cfg(&yaml).unwrap();
    assert!(StaticModelPolicyService::from_config(&cfg).is_ok());
}

// ═══════════════════════ model-policy plugin: behaviour ═══════════════════════

fn service_from(yaml: &str) -> StaticModelPolicyService {
    let cfg = policy_cfg(yaml).expect("config parses");
    StaticModelPolicyService::from_config(&cfg).expect("plugin initialises")
}

fn two_model_yaml() -> String {
    format!(
        "model_catalog:\n  - {}\n  - {}\n  - {}\nkill_switches:\n  disable_web_search: true\n",
        catalog_entry_json("gpt-p", "Premium", true),
        catalog_entry_json("gpt-s", "Standard", true),
        catalog_entry_json("gpt-off", "Standard", false),
    )
}

#[tokio::test]
async fn current_policy_version_is_fixed_at_one() {
    let svc = service_from(&two_model_yaml());
    let info = svc
        .get_current_policy_version(Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        info.policy_version, 1,
        "static plugin: fixed version 1 (DESIGN 5.2.5)"
    );
}

#[tokio::test]
async fn snapshot_serves_the_configured_catalog_and_kill_switches() {
    let svc = service_from(&two_model_yaml());
    let snap = svc.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();

    assert_eq!(snap.policy_version, 1);
    let ids: Vec<&str> = snap.model_catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["gpt-p", "gpt-s", "gpt-off"],
        "the plugin is dumb: it serves the whole catalog, disabled entries included, in config order"
    );
    assert_eq!(snap.model_catalog[0].tier, ModelTier::Premium);
    assert_eq!(snap.model_catalog[1].tier, ModelTier::Standard);
    assert!(snap.model_catalog[0].enabled);
    assert!(!snap.model_catalog[2].enabled);
    assert!(snap.kill_switches.disable_web_search);
    assert!(!snap.kill_switches.disable_premium_tier);
    assert!(!snap.kill_switches.force_standard_tier);
    assert!(!snap.kill_switches.disable_file_search);
    assert!(!snap.kill_switches.disable_images);
    assert!(!snap.kill_switches.disable_code_interpreter);
}

#[tokio::test]
async fn snapshot_is_identical_for_every_user() {
    let svc = service_from(&two_model_yaml());
    let a = svc.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();
    let b = svc.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();
    assert_eq!(
        a, b,
        "PolicySnapshot is shared and immutable (DESIGN 5.2.6)"
    );
}

#[tokio::test]
async fn absent_section_serves_an_empty_catalog_and_the_default_limits() {
    let cfg = StaticModelPolicyConfig::default();
    let svc = StaticModelPolicyService::from_config(&cfg).unwrap();
    let snap = svc.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();
    assert!(snap.model_catalog.is_empty());

    let user = Uuid::new_v4();
    let limits = svc.get_user_limits(user, 1).await.unwrap();
    assert_eq!(limits.user_id, user);
    assert_eq!(limits.policy_version, 1);
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(limits.standard.limit_monthly_credits_micro, 1_000_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 50_000_000);
    assert_eq!(limits.premium.limit_monthly_credits_micro, 500_000_000);
}

#[tokio::test]
async fn user_limits_come_from_the_configured_defaults_and_are_the_same_for_every_user() {
    let yaml = "model_catalog: []\ndefault_standard_limits:\n  limit_daily_credits_micro: 11\n  limit_monthly_credits_micro: 22\ndefault_premium_limits:\n  limit_daily_credits_micro: 33\n  limit_monthly_credits_micro: 44\n";
    let svc = service_from(yaml);
    let (u1, u2) = (Uuid::new_v4(), Uuid::new_v4());
    let l1 = svc.get_user_limits(u1, 1).await.unwrap();
    let l2 = svc.get_user_limits(u2, 1).await.unwrap();

    assert_eq!(l1.user_id, u1);
    assert_eq!(l2.user_id, u2);
    for l in [&l1, &l2] {
        assert_eq!(l.standard.limit_daily_credits_micro, 11);
        assert_eq!(l.standard.limit_monthly_credits_micro, 22);
        assert_eq!(l.premium.limit_daily_credits_micro, 33);
        assert_eq!(l.premium.limit_monthly_credits_micro, 44);
    }
}

#[tokio::test]
async fn publish_usage_only_logs_and_succeeds() {
    let svc = service_from("model_catalog: []\n");
    let event: UsageEvent = serde_json::from_value(json!({
        "tenant_id": Uuid::new_v4(),
        "chat_id": Uuid::new_v4(),
        "request_id": Uuid::new_v4(),
        "effective_model": "m",
        "selected_model": "m",
        "terminal_state": "completed",
        "billing_outcome": "system_task",
        "usage": null,
        "actual_credits_micro": 0,
        "settlement_method": "none",
        "policy_version_applied": 0,
        "timestamp": "2026-09-26T12:00:00Z",
        "requester_type": "system",
        "dedupe_key": "x/thread_summary_update/y",
        "system_task_type": "thread_summary_update"
    }))
    .expect("usage event");
    svc.publish_usage(event)
        .await
        .expect("publish_usage never fails");
}

#[tokio::test]
async fn check_user_license_is_not_implemented_by_the_static_plugin() {
    // DESIGN 5.2.2: default body -> `active: false`; the gear never calls it.
    let svc = service_from("model_catalog: []\n");
    let status = svc.check_user_license(Uuid::new_v4()).await.unwrap();
    assert!(!status.active);
}

// ═══════════════════ shipped config/mini-chat.yaml plugin blocks ═══════════════════

#[derive(Deserialize)]
struct ConfigFile {
    gears: BTreeMap<String, Value>,
}

fn plugin_block(gear: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../config/mini-chat.yaml");
    let text = std::fs::read_to_string(&path).expect("read config/mini-chat.yaml");
    let parsed: ConfigFile = serde_saphyr::from_str(&text).expect("valid YAML");
    parsed
        .gears
        .get(gear)
        .unwrap_or_else(|| panic!("gears.{gear} missing"))["config"]
        .clone()
}

#[tokio::test]
async fn shipped_model_policy_config_loads_and_serves_two_models() {
    let block = plugin_block("static-mini-chat-model-policy-plugin");
    let cfg: StaticModelPolicyConfig = serde_json::from_value(block).expect("plugin config parses");
    let svc = StaticModelPolicyService::from_config(&cfg).expect("plugin initialises");

    let snap = svc.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();
    assert_eq!(snap.model_catalog.len(), 2);

    let premium = &snap.model_catalog[0];
    assert_eq!(premium.id, "gpt-4.1");
    assert_eq!(premium.tier, ModelTier::Premium);
    assert!(premium.enabled);
    assert_eq!(premium.provider_id, "azure_openai");
    assert_eq!(premium.input_tokens_credit_multiplier_micro, 3_000_000);
    assert_eq!(premium.output_tokens_credit_multiplier_micro, 15_000_000);
    assert_eq!(premium.multiplier_display, "3x");
    assert_eq!(premium.max_tool_calls, 10);
    assert!(premium.general_config.tool_support.web_search);
    assert!(premium.general_config.tool_support.file_search);
    assert!(premium.preference.as_ref().is_some_and(|p| p.is_default));

    let standard = &snap.model_catalog[1];
    assert_eq!(standard.id, "gpt-4.1-mini");
    assert_eq!(standard.tier, ModelTier::Standard);
    assert!(!standard.general_config.tool_support.web_search);
    assert_eq!(standard.preference.as_ref().map(|p| p.sort_order), Some(1));
}

#[test]
fn shipped_audit_config_loads() {
    let block = plugin_block("static-mini-chat-audit-plugin");
    let cfg: StaticAuditConfig = serde_json::from_value(block).expect("audit config parses");
    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

// ═════════════════════════════ audit plugin ═════════════════════════════════════

#[test]
fn audit_defaults() {
    let cfg = StaticAuditConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);

    let parsed = audit_cfg("{}").unwrap();
    assert!(parsed.enabled);
    assert_eq!(parsed.vendor, "constructorfabric");
    assert_eq!(parsed.priority, 100);
}

#[test]
fn audit_keys_are_configurable() {
    let cfg = audit_cfg("enabled: false\nvendor: acme\npriority: 7\n").unwrap();
    assert!(!cfg.enabled);
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, 7);
}

#[test]
fn audit_rejects_unknown_keys() {
    assert!(audit_cfg("enabled: true\nbogus_key: 1\n").is_err());
}

fn sample_audit_event() -> AuditEvent {
    serde_json::from_value(json!({
        "event_type": "turn_edit",
        "tenant_id": Uuid::new_v4(),
        "actor_user_id": Uuid::new_v4(),
        "chat_id": Uuid::new_v4(),
        "original_request_id": Uuid::new_v4(),
        "new_request_id": Uuid::new_v4(),
        "timestamp": "2026-03-02T10:00:00Z",
    }))
    .expect("audit event")
}

#[tokio::test]
async fn audit_plugin_accepts_events_whether_enabled_or_not() {
    // "When `false`, the plugin registers but does not log audit events."
    for enabled in [true, false] {
        let cfg = audit_cfg(&format!("enabled: {enabled}\n")).unwrap();
        let svc = StaticAuditService::new(&cfg);
        svc.emit(sample_audit_event())
            .await
            .unwrap_or_else(|e| panic!("emit must succeed with enabled={enabled}: {e:?}"));
    }
}
