#![allow(clippy::unwrap_used, clippy::expect_used)]

use time::OffsetDateTime;
use uuid::Uuid;

use crate::usage::{UsageEvent, UsageTokens};

fn base_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::from_u128(1),
        user_id: Some(Uuid::from_u128(2)),
        chat_id: Uuid::from_u128(3),
        turn_id: Some(Uuid::from_u128(4)),
        request_id: Uuid::from_u128(5),
        effective_model: "gpt-4.1".to_owned(),
        selected_model: "gpt-4.1".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: Some(UsageTokens {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        actual_credits_micro: 123,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 7,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::from_unix_timestamp(1_772_445_600).unwrap(),
        requester_type: "user".to_owned(),
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    }
}

#[test]
fn system_usage_event_omits_user_and_turn() {
    let mut e = base_event();
    e.user_id = None;
    e.turn_id = None;
    e.requester_type = "system".to_owned();
    e.system_task_type = Some("thread_summary_update".to_owned());
    let v = serde_json::to_value(&e).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("user_id"));
    assert!(!obj.contains_key("turn_id"));
    assert_eq!(obj["system_task_type"], "thread_summary_update");
    let back: UsageEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, e);
}

#[test]
fn user_usage_event_has_null_usage_when_unknown() {
    let mut e = base_event();
    e.usage = None;
    let v = serde_json::to_value(&e).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj["usage"].is_null());
    assert!(obj.contains_key("user_id"));
    assert!(!obj.contains_key("system_task_type"));
    assert_eq!(obj["timestamp"], "2026-03-02T10:00:00Z");
}
