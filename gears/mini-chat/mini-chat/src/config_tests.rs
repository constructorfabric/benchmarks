#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{MiniChatConfig, derive_alias, expand_vars};

fn base() -> serde_json::Value {
    serde_json::json!({
        "vendor": "constructorfabric",
        "client_credentials": {"client_id": "mini-chat", "client_secret": "s"},
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "storage_kind": "openai",
                "host": "127.0.0.1",
                "port": 18090,
                "use_http": true
            }
        }
    })
}

#[test]
fn defaults_validate() {
    let cfg: MiniChatConfig = serde_json::from_value(base()).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(cfg.thread_summary_worker.effective_model_id(), "gpt-4.1-mini");
}

#[test]
fn unknown_top_level_and_removed_keys_rejected() {
    let mut v = base();
    v["bogus"] = serde_json::json!(1);
    assert!(serde_json::from_value::<MiniChatConfig>(v).is_err());
    let mut v = base();
    v["streaming"] = serde_json::json!({"web_search_context_size": "low"});
    assert!(serde_json::from_value::<MiniChatConfig>(v).is_err());
    let mut v = base();
    v["providers"]["openai"]["supports_file_search_filters"] = serde_json::json!(true);
    assert!(serde_json::from_value::<MiniChatConfig>(v).is_err());
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let mut v = base();
    v["orphan_watchdog"] = serde_json::json!({"timeout_secs": 90, "whatever": 1});
    let cfg: MiniChatConfig = serde_json::from_value(v).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn ranges_validated() {
    for (path, val) in [
        (("streaming", "sse_ping_interval_seconds"), serde_json::json!(4)),
        (("streaming", "sse_channel_capacity"), serde_json::json!(65)),
        (("quota", "overshoot_tolerance_factor"), serde_json::json!(1.6)),
        (("orphan_watchdog", "timeout_secs"), serde_json::json!(89)),
        (("upload_reaper", "stale_after_secs"), serde_json::json!(59)),
        (("outbox", "num_partitions"), serde_json::json!(3)),
    ] {
        let mut v = base();
        v[path.0] = serde_json::json!({ path.1: val });
        let cfg: MiniChatConfig = serde_json::from_value(v).unwrap();
        assert!(cfg.validate().is_err(), "{}.{} should be rejected", path.0, path.1);
    }
}

#[test]
fn azure_requires_api_version() {
    let mut v = base();
    v["providers"]["openai"]["storage_kind"] = serde_json::json!("azure");
    let cfg: MiniChatConfig = serde_json::from_value(v.clone()).unwrap();
    assert!(cfg.validate().is_err());
    v["providers"]["openai"]["api_version"] = serde_json::json!("2025-03-01-preview");
    let cfg: MiniChatConfig = serde_json::from_value(v).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn alias_derivation() {
    assert_eq!(derive_alias("API.openai.com.", 443), "api.openai.com");
    assert_eq!(derive_alias("localhost", 8099), "localhost:8099");
    let mut cfg: MiniChatConfig = serde_json::from_value(base()).unwrap();
    cfg.fill_aliases();
    assert_eq!(cfg.providers["openai"].alias(), "127.0.0.1:18090");
}

#[test]
fn env_expansion() {
    // SAFETY-free: uses a unique variable name.
    assert_eq!(expand_vars("plain").unwrap(), "plain");
    assert_eq!(expand_vars("${MC_TEST_SURELY_UNSET:-dflt}").unwrap(), "dflt");
    assert!(expand_vars("${MC_TEST_SURELY_UNSET}").is_err());
    let path = std::env::var("PATH").unwrap();
    assert_eq!(expand_vars("x${PATH}y").unwrap(), format!("x{path}y"));
}
