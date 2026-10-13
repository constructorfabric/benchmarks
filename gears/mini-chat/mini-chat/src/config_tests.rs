use super::*;

#[test]
fn defaults_validate_with_credentials() {
    let mut c = MiniChatConfig::default();
    assert!(c.validate().is_err(), "client credentials are required");
    c.client_credentials.client_id = "id".into();
    c.client_credentials.client_secret = "secret".into();
    c.validate().unwrap();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert!(c.providers.contains_key("openai"));
}

#[test]
fn rejects_unknown_keys_and_ranges() {
    let r: Result<MiniChatConfig, _> = serde_json::from_value(serde_json::json!({"mcp": {}}));
    assert!(r.is_err());
    let r: Result<MiniChatConfig, _> = serde_json::from_value(serde_json::json!({"streaming": {"web_search_context_size": "low"}}));
    assert!(r.is_err());
    let mut c: MiniChatConfig = serde_json::from_value(serde_json::json!({
        "client_credentials": {"client_id": "a", "client_secret": "b"},
        "orphan_watchdog": {"timeout_secs": 10, "unknown_ignored": 1}
    }))
    .unwrap();
    assert!(c.validate().is_err());
    c.orphan_watchdog.timeout_secs = 90;
    c.validate().unwrap();
    c.streaming.sse_channel_capacity = 8;
    assert!(c.validate().is_err());
}

#[test]
fn azure_requires_api_version() {
    let c: MiniChatConfig = serde_json::from_value(serde_json::json!({
        "client_credentials": {"client_id": "a", "client_secret": "b"},
        "providers": {"az": {"kind": "openai_responses", "host": "x.openai.azure.com", "storage_kind": "azure"}}
    }))
    .unwrap();
    assert!(c.validate().is_err());
}

#[test]
fn deprecated_fields_warn() {
    let c: MiniChatConfig = serde_json::from_value(serde_json::json!({
        "estimation_budgets": {"safety_margin_pct": 20},
        "cleanup_worker": {"enabled": false}
    }))
    .unwrap();
    assert_eq!(c.deprecation_warnings().len(), 2);
}
