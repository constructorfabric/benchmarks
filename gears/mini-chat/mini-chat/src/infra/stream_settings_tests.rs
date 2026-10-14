//! The configuration keys of the streaming slice: the ping interval of the SSE relay, the
//! bounded channel capacity, the per-message tool limits and the provider idle timeout of the
//! provider task, and the orphan watchdog keys, with their defaults and custom values.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `configuration` (`inst-cfg-streaming`, `inst-cfg-tool-limits`, `inst-cfg-watchdog`) and
//! `tool-assembly` (`inst-tools-max-calls`); definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::json;

use crate::config::MiniChatConfig;
use crate::domain::streaming::StreamSettings;
use crate::infra::orphan_watchdog::WatchdogSettings;
use crate::infra::provider_task::ProviderTaskSettings;

fn config(extra: &serde_json::Value) -> MiniChatConfig {
    let mut raw = json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
    });
    for (key, value) in extra.as_object().unwrap() {
        raw[key] = value.clone();
    }
    MiniChatConfig::from_value(raw).expect("valid config")
}

#[test]
fn the_defaults_are_the_documented_values() {
    let config = config(&json!({}));
    assert_eq!(
        StreamSettings::from_config(&config).sse_ping_interval,
        Duration::from_secs(15)
    );
    let task = ProviderTaskSettings::from_config(&config);
    assert_eq!(task.channel_capacity, 32);
    assert_eq!(task.web_search_max_calls_per_message, 2);
    assert_eq!(task.code_interpreter_max_calls_per_message, 10);
    assert_eq!(task.progress_interval, Duration::from_secs(30));
    let watchdog = WatchdogSettings::from_config(&config);
    assert_eq!(watchdog.timeout, Duration::from_secs(300));
    assert_eq!(watchdog.scan_interval, Duration::from_secs(60));
}

#[test]
fn custom_values_are_read_from_their_keys() {
    let config = config(&json!({
        "streaming": { "sse_ping_interval_seconds": 7, "sse_channel_capacity": 20 },
        "quota": { "web_search_max_calls_per_message": 5,
                    "code_interpreter_max_calls_per_message": 3 },
        "orphan_watchdog": { "timeout_secs": 120, "scan_interval_secs": 5 },
    }));
    assert_eq!(
        StreamSettings::from_config(&config).sse_ping_interval,
        Duration::from_secs(7)
    );
    let task = ProviderTaskSettings::from_config(&config);
    assert_eq!(task.channel_capacity, 20);
    assert_eq!(task.web_search_max_calls_per_message, 5);
    assert_eq!(task.code_interpreter_max_calls_per_message, 3);
    let watchdog = WatchdogSettings::from_config(&config);
    assert_eq!(watchdog.timeout, Duration::from_secs(120));
    assert_eq!(watchdog.scan_interval, Duration::from_secs(5));
}

#[test]
fn the_provider_idle_timeout_is_below_the_smallest_orphan_timeout() {
    // `orphan_watchdog.timeout_secs` is at least 90: a silent provider is cut first
    let task = ProviderTaskSettings::from_config(&config(&json!({
        "orphan_watchdog": { "timeout_secs": 90 },
    })));
    assert!(
        task.idle_timeout < Duration::from_secs(90),
        "{:?}",
        task.idle_timeout
    );
    assert!(!task.idle_timeout.is_zero());
}
