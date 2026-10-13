//! JSON contract of `UsageEvent` — the payload handed to the model-policy
//! plugin's `publish_usage` and written to the usage outbox queue.
//!
//! Source of truth: DESIGN 3.2 "Outbox payload schema for system tasks",
//! 5.6 (`dedupe_key` canonical format and UUID normalisation), 5.7 (dedupe key
//! requirement) and Appendix A.3 (`PublishUsage` payload).

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

use mini_chat_sdk::{SystemTaskType, UsageEvent, system_task_dedupe_key, turn_dedupe_key};
use serde_json::{Value, json};
use uuid::Uuid;

const TENANT: &str = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
const USER: &str = "11111111-6a88-4768-9dfc-6bcd5187d9ed";
const CHAT: &str = "0d9c1f1e-3b3b-4b8f-9a47-6a7c9f0c1a11";
const TURN: &str = "5e6f7a8b-9c0d-4e1f-8a2b-3c4d5e6f7a8b";
const REQUEST: &str = "c3d4e5f6-7890-4bcd-ef12-34567890abcd";

/// A user turn as emitted by the finalization transaction.
fn user_turn_event() -> Value {
    json!({
        "tenant_id": TENANT,
        "user_id": USER,
        "chat_id": CHAT,
        "turn_id": TURN,
        "request_id": REQUEST,
        "effective_model": "gpt-4.1-mini",
        "selected_model": "gpt-4.1",
        "terminal_state": "completed",
        "billing_outcome": "completed",
        "usage": {
            "input_tokens": 123,
            "output_tokens": 456,
            "cache_read_input_tokens": 80,
            "cache_write_input_tokens": 0,
            "reasoning_tokens": 20
        },
        "actual_credits_micro": 3_750_000,
        "settlement_method": "actual",
        "policy_version_applied": 12345,
        "web_search_calls": 1,
        "code_interpreter_calls": 0,
        "file_search_calls": 2,
        "timestamp": "2026-03-02T10:00:00Z",
        "requester_type": "user",
        "dedupe_key": "f47ac10b58cc4372a5670e02b2c3d479/5e6f7a8b9c0d4e1f8a2b3c4d5e6f7a8b/c3d4e5f678904bcdef1234567890abcd"
    })
}

/// The exact system-task example of DESIGN 3.2.
fn system_task_event() -> Value {
    json!({
        "tenant_id": TENANT,
        "chat_id": CHAT,
        "request_id": REQUEST,
        "effective_model": "gpt-4.1-mini",
        "selected_model": "gpt-4.1-mini",
        "terminal_state": "completed",
        "billing_outcome": "system_task",
        "usage": {
            "input_tokens": 5000,
            "output_tokens": 200,
            "cache_read_input_tokens": 4000,
            "cache_write_input_tokens": 0,
            "reasoning_tokens": 0
        },
        "actual_credits_micro": 0,
        "settlement_method": "none",
        "policy_version_applied": 0,
        "web_search_calls": 0,
        "code_interpreter_calls": 0,
        "file_search_calls": 0,
        "timestamp": "2026-09-26T12:00:00Z",
        "requester_type": "system",
        "dedupe_key": "f47ac10b58cc4372a5670e02b2c3d479/thread_summary_update/c3d4e5f678904bcdef1234567890abcd",
        "system_task_type": "thread_summary_update"
    })
}

fn reserialize(v: &Value) -> Value {
    let ev: UsageEvent = serde_json::from_value(v.clone()).expect("usage event parses");
    serde_json::to_value(&ev).expect("usage event serializes")
}

#[test]
fn user_turn_event_round_trips_exactly() {
    let v = user_turn_event();
    assert_eq!(reserialize(&v), v);
}

#[test]
fn system_task_event_round_trips_and_omits_user_and_turn() {
    let v = system_task_event();
    let out = reserialize(&v);
    assert_eq!(out, v);
    let obj = out.as_object().unwrap();
    assert!(
        !obj.contains_key("user_id"),
        "user_id is absent (not null) for system tasks"
    );
    assert!(
        !obj.contains_key("turn_id"),
        "turn_id is absent (not null) for system tasks"
    );
}

#[test]
fn user_turn_event_omits_system_task_type() {
    // Appendix A.3: "`system_task_type` is omitted for user turns".
    let out = reserialize(&user_turn_event());
    assert!(!out.as_object().unwrap().contains_key("system_task_type"));
}

#[test]
fn system_task_event_fields_are_typed() {
    let ev: UsageEvent = serde_json::from_value(system_task_event()).unwrap();
    assert_eq!(ev.tenant_id, Uuid::parse_str(TENANT).unwrap());
    assert!(ev.user_id.is_none());
    assert!(ev.turn_id.is_none());
    assert_eq!(ev.request_id, Uuid::parse_str(REQUEST).unwrap());
    assert_eq!(ev.actual_credits_micro, 0);
    assert_eq!(ev.policy_version_applied, 0);
    assert_eq!(
        ev.system_task_type,
        Some(SystemTaskType::ThreadSummaryUpdate)
    );
    let usage = ev.usage.as_ref().expect("usage present");
    assert_eq!(usage.input_tokens, 5000);
    assert_eq!(usage.output_tokens, 200);
    assert_eq!(usage.cache_read_input_tokens, 4000);
    assert_eq!(usage.cache_write_input_tokens, 0);
    assert_eq!(usage.reasoning_tokens, 0);
}

#[test]
fn usage_is_null_when_the_provider_reported_none() {
    // Orphan-watchdog finalization has no provider usage: `usage: null`
    // (present, not omitted).
    let mut v = user_turn_event();
    v["usage"] = Value::Null;
    v["terminal_state"] = json!("failed");
    v["billing_outcome"] = json!("aborted");
    v["settlement_method"] = json!("estimated");
    let out = reserialize(&v);
    assert!(out.as_object().unwrap().contains_key("usage"));
    assert_eq!(out["usage"], Value::Null);
    assert_eq!(out, v);
}

#[test]
fn enum_string_values_match_the_documented_wire_format() {
    for state in ["completed", "failed", "cancelled"] {
        let mut v = user_turn_event();
        v["terminal_state"] = json!(state);
        assert_eq!(reserialize(&v)["terminal_state"], json!(state));
    }
    for outcome in ["completed", "failed", "aborted", "system_task"] {
        let mut v = user_turn_event();
        v["billing_outcome"] = json!(outcome);
        assert_eq!(reserialize(&v)["billing_outcome"], json!(outcome));
    }
    for method in ["actual", "estimated", "released", "none"] {
        let mut v = user_turn_event();
        v["settlement_method"] = json!(method);
        assert_eq!(reserialize(&v)["settlement_method"], json!(method));
    }
    for requester in ["user", "system"] {
        let mut v = user_turn_event();
        v["requester_type"] = json!(requester);
        assert_eq!(reserialize(&v)["requester_type"], json!(requester));
    }
    for task in ["thread_summary_update", "doc_summary_generation"] {
        let mut v = system_task_event();
        v["system_task_type"] = json!(task);
        assert_eq!(reserialize(&v)["system_task_type"], json!(task));
    }
}

#[test]
fn unknown_enum_values_are_rejected() {
    for (field, bad) in [
        ("terminal_state", "running"),
        ("billing_outcome", "refunded"),
        ("settlement_method", "magic"),
        ("requester_type", "robot"),
    ] {
        let mut v = user_turn_event();
        v[field] = json!(bad);
        assert!(
            serde_json::from_value::<UsageEvent>(v).is_err(),
            "{field}={bad} must be rejected"
        );
    }
}

#[test]
fn identity_and_billing_fields_are_required() {
    for missing in [
        "tenant_id",
        "request_id",
        "effective_model",
        "selected_model",
        "terminal_state",
        "billing_outcome",
        "actual_credits_micro",
        "settlement_method",
        "policy_version_applied",
        "timestamp",
        "requester_type",
        "dedupe_key",
    ] {
        let mut v = user_turn_event();
        v.as_object_mut().unwrap().remove(missing);
        assert!(
            serde_json::from_value::<UsageEvent>(v).is_err(),
            "usage event without `{missing}` must not deserialize"
        );
    }
}

#[test]
fn turn_dedupe_key_uses_simple_lowercase_uuids() {
    let tenant = Uuid::parse_str(TENANT).unwrap();
    let turn = Uuid::parse_str(TURN).unwrap();
    let request = Uuid::parse_str(REQUEST).unwrap();
    let key = turn_dedupe_key(tenant, turn, request);
    assert_eq!(
        key,
        "f47ac10b58cc4372a5670e02b2c3d479/5e6f7a8b9c0d4e1f8a2b3c4d5e6f7a8b/c3d4e5f678904bcdef1234567890abcd"
    );
    assert_eq!(key.len(), 32 * 3 + 2, "96 hex chars + two slashes");
    assert!(!key.contains('-'));
    assert_eq!(key, key.to_lowercase());
}

#[test]
fn system_task_dedupe_key_has_tenant_task_type_and_request() {
    let tenant = Uuid::parse_str(TENANT).unwrap();
    let request = Uuid::parse_str(REQUEST).unwrap();
    let key = system_task_dedupe_key(tenant, SystemTaskType::ThreadSummaryUpdate, request);
    assert_eq!(
        key,
        "f47ac10b58cc4372a5670e02b2c3d479/thread_summary_update/c3d4e5f678904bcdef1234567890abcd"
    );
    // The key starts with the tenant in simple form (Mini Chat domain invariant).
    assert!(key.starts_with(&format!("{}/", tenant.as_simple())));
}

#[test]
fn dedupe_keys_match_the_payload_examples() {
    let ev: UsageEvent = serde_json::from_value(user_turn_event()).unwrap();
    let want = turn_dedupe_key(ev.tenant_id, ev.turn_id.expect("turn id"), ev.request_id);
    assert_eq!(ev.dedupe_key, want);

    let ev: UsageEvent = serde_json::from_value(system_task_event()).unwrap();
    let want = system_task_dedupe_key(
        ev.tenant_id,
        ev.system_task_type.expect("system task type"),
        ev.request_id,
    );
    assert_eq!(ev.dedupe_key, want);
}

#[test]
fn system_task_type_wire_names() {
    assert_eq!(
        serde_json::to_value(SystemTaskType::ThreadSummaryUpdate).unwrap(),
        json!("thread_summary_update")
    );
    assert_eq!(
        serde_json::to_value(SystemTaskType::DocSummaryGeneration).unwrap(),
        json!("doc_summary_generation")
    );
}
