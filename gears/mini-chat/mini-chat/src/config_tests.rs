use super::*;

fn base() -> serde_json::Value {
    serde_json::json!({ "client_credentials": { "client_id": "mini-chat", "client_secret": "s" } })
}

fn parse(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
fn defaults_are_valid() {
    let cfg = parse(base()).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(cfg.quota.web_search_daily_quota, 75);
    assert_eq!(cfg.thread_summary_worker.effective_summary_model_id(), "gpt-4.1-mini");
    assert!(cfg.providers.contains_key("openai"));
}

#[test]
fn rejects_unknown_top_level_and_removed_keys() {
    let mut v = base();
    v["mcp"] = serde_json::json!({});
    assert!(parse(v).is_err());
    let mut v = base();
    v["streaming"] = serde_json::json!({ "web_search_context_size": "low" });
    assert!(parse(v).is_err());
}

#[test]
fn worker_sections_ignore_unknown_keys() {
    let mut v = base();
    v["orphan_watchdog"] = serde_json::json!({ "scan_interval_secs": 10, "timeout_secs": 90, "whatever": 1 });
    let cfg = parse(v).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
}

#[test]
fn rejects_out_of_range_values() {
    for (path, value) in [
        (("streaming", "sse_ping_interval_seconds"), serde_json::json!(3)),
        (("streaming", "sse_channel_capacity"), serde_json::json!(8)),
        (("quota", "overshoot_tolerance_factor"), serde_json::json!(2.0)),
        (("orphan_watchdog", "timeout_secs"), serde_json::json!(30)),
        (("estimation_budgets", "minimal_generation_floor"), serde_json::json!(0)),
        (("outbox", "num_partitions"), serde_json::json!(3)),
    ] {
        let mut v = base();
        v[path.0] = serde_json::json!({ path.1: value });
        let cfg = parse(v).unwrap();
        assert!(cfg.validate().is_err(), "{}.{} should be rejected", path.0, path.1);
    }
}

#[test]
fn azure_requires_api_version() {
    let mut v = base();
    v["providers"] = serde_json::json!({
        "az": { "kind": "openai_responses", "host": "x.openai.azure.com", "storage_kind": "azure" }
    });
    assert!(parse(v.clone()).unwrap().validate().is_err());
    v["providers"]["az"]["api_version"] = serde_json::json!("2025-03-01-preview");
    parse(v).unwrap().validate().unwrap();
}

#[test]
fn rag_provider_must_exist() {
    let mut v = base();
    v["providers"] = serde_json::json!({
        "a": { "kind": "anthropic_messages", "host": "api.anthropic.com", "rag_provider": "missing" }
    });
    assert!(parse(v).unwrap().validate().is_err());
}

#[test]
fn missing_client_credentials_fail() {
    let cfg = parse(serde_json::json!({})).unwrap();
    assert!(cfg.validate().is_err());
}
