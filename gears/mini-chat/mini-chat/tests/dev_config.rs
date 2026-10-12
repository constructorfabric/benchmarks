#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The development configuration `config/mini-chat.yaml` is a valid configuration of the gear and
//! of the bundled plugins (with the environment variables it references set).

use std::path::PathBuf;

use mini_chat::config::MiniChatConfig;
use mini_chat::infra::plugins::static_audit::StaticAuditConfig;
use mini_chat::infra::plugins::static_model_policy::StaticModelPolicyConfig;
use serde_json::Value;
use toolkit::var_expand::ExpandVars;

fn dev_config() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../config/mini-chat.yaml");
    let yaml = std::fs::read_to_string(path).expect("the dev config is readable");
    serde_saphyr::from_str(&yaml).expect("the dev config is valid YAML")
}

fn section(name: &str) -> Value {
    dev_config()["gears"][name]["config"].clone()
}

#[test]
fn the_gear_section_parses_expands_and_validates() {
    temp_env::with_vars(
        [
            ("AZURE_OPENAI_API_HOST", Some("example.openai.azure.com")),
            ("AZURE_OPENAI_API_KEY", Some("key")),
        ],
        || {
            let mut cfg: MiniChatConfig = serde_json::from_value(section("mini-chat")).unwrap();
            cfg.expand_vars().expect("the referenced variables are set");

            cfg.validate().unwrap();
            assert_eq!(
                cfg.providers["azure_openai"].host,
                "example.openai.azure.com"
            );
            assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
            assert_eq!(cfg.thread_summary_worker.summary_model_id, "gpt-4.1-mini");
            assert!(cfg.deprecation_warnings().is_empty());
        },
    );
}

#[test]
fn an_unset_provider_variable_fails_the_expansion() {
    temp_env::with_vars([("AZURE_OPENAI_API_HOST", None::<&str>)], || {
        let mut cfg: MiniChatConfig = serde_json::from_value(section("mini-chat")).unwrap();

        assert!(cfg.expand_vars().is_err());
    });
}

#[test]
fn the_policy_plugin_section_parses_and_validates() {
    let cfg: StaticModelPolicyConfig =
        serde_json::from_value(section("static-mini-chat-model-policy-plugin")).unwrap();

    cfg.validate().unwrap();
    assert_eq!(cfg.model_catalog.len(), 2);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert!(cfg.model_catalog.iter().all(|entry| entry.enabled));
    assert_eq!(
        cfg.model_catalog
            .iter()
            .filter(|entry| entry.is_default())
            .count(),
        1
    );
}

#[test]
fn the_audit_plugin_section_parses() {
    let cfg: StaticAuditConfig =
        serde_json::from_value(section("static-mini-chat-audit-plugin")).unwrap();

    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
}
