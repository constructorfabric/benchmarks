#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-encode-events`.

use std::collections::BTreeSet;

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::models::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage, BillingOutcome,
    MiniChatAuditEvent, RequesterType, SettlementMethod, SystemTaskType, TerminalState,
    TurnAuditEvent, TurnAuditEventType, TurnMutationAuditEvent, UsageEvent, UsageTokens,
    decode_audit_event, decode_usage_event, encode_audit_event, encode_usage_event,
    system_task_dedupe_key, usage_dedupe_key,
};

const TENANT: Uuid = Uuid::from_u128(0xf47a_c10b_58cc_4372_a567_0e02_b2c3_d479);
const USER: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
const CHAT: Uuid = Uuid::from_u128(0x2222_2222_0000_4000_8000_0000_0000_0001);
const TURN: Uuid = Uuid::from_u128(0x3333_3333_0000_4000_8000_0000_0000_00ab);
const REQUEST: Uuid = Uuid::from_u128(0x4444_4444_0000_4000_8000_0000_0000_00cd);
const SYSTEM_REQUEST: Uuid = Uuid::from_u128(0x5555_5555_0000_4000_8000_0000_0000_00ef);

fn ts() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_772_445_600).unwrap()
}

fn tokens() -> UsageTokens {
    UsageTokens {
        input_tokens: 123,
        output_tokens: 456,
        cache_read_input_tokens: 80,
        cache_write_input_tokens: 0,
        reasoning_tokens: 20,
    }
}

fn user_event() -> UsageEvent {
    UsageEvent {
        tenant_id: TENANT,
        user_id: Some(USER),
        chat_id: CHAT,
        turn_id: Some(TURN),
        request_id: REQUEST,
        effective_model: "gpt-5.2".to_owned(),
        selected_model: "gpt-5.2".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: Some(tokens()),
        actual_credits_micro: 3_750_000,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: 12345,
        web_search_calls: 1,
        code_interpreter_calls: 2,
        file_search_calls: 3,
        timestamp: ts(),
        requester_type: RequesterType::User,
        dedupe_key: usage_dedupe_key(TENANT, TURN, REQUEST),
        system_task_type: None,
    }
}

fn system_event() -> UsageEvent {
    UsageEvent {
        user_id: None,
        turn_id: None,
        request_id: SYSTEM_REQUEST,
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::SystemTask,
        requester_type: RequesterType::System,
        dedupe_key: system_task_dedupe_key(
            TENANT,
            SystemTaskType::ThreadSummaryUpdate,
            SYSTEM_REQUEST,
        ),
        system_task_type: Some(SystemTaskType::ThreadSummaryUpdate),
        ..user_event()
    }
}

fn encoded(event: &UsageEvent) -> Value {
    serde_json::from_slice(&encode_usage_event(event).expect("encodes")).expect("payload is JSON")
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

// ---- usage event -----------------------------------------------------------

#[test]
fn user_turn_usage_json_has_every_design_field_and_nothing_else() {
    let json = encoded(&user_event());

    let want: BTreeSet<String> = [
        "tenant_id",
        "user_id",
        "chat_id",
        "turn_id",
        "request_id",
        "effective_model",
        "selected_model",
        "terminal_state",
        "billing_outcome",
        "usage",
        "actual_credits_micro",
        "settlement_method",
        "policy_version_applied",
        "web_search_calls",
        "code_interpreter_calls",
        "file_search_calls",
        "timestamp",
        "requester_type",
        "dedupe_key",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        keys(&json),
        want,
        "user turn omits system_task_type and has no event_type"
    );
}

#[test]
fn usage_json_never_carries_unimplemented_fields() {
    for event in [user_event(), system_event()] {
        let json = encoded(&event);
        for field in [
            "event_type",
            "completion_signal",
            "reserved_credits_micro",
            "committed_credits_micro",
            "overshoot_capped",
            "error_code",
        ] {
            assert!(json.get(field).is_none(), "`{field}` must not be encoded");
        }
    }
}

#[test]
fn user_turn_usage_json_values_follow_the_wire_contract() {
    let json = encoded(&user_event());

    assert_eq!(json["tenant_id"], "f47ac10b-58cc-4372-a567-0e02b2c3d479");
    assert_eq!(json["user_id"], USER.to_string());
    assert_eq!(json["terminal_state"], "completed");
    assert_eq!(json["billing_outcome"], "completed");
    assert_eq!(json["settlement_method"], "actual");
    assert_eq!(json["requester_type"], "user");
    assert_eq!(json["actual_credits_micro"], 3_750_000);
    assert_eq!(json["policy_version_applied"], 12345);
    assert_eq!(json["web_search_calls"], 1);
    assert_eq!(json["code_interpreter_calls"], 2);
    assert_eq!(json["file_search_calls"], 3);
    assert_eq!(
        keys(&json["usage"]),
        [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_write_input_tokens",
            "reasoning_tokens"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(json["usage"]["input_tokens"], 123);
    assert_eq!(json["usage"]["reasoning_tokens"], 20);
    let parsed = OffsetDateTime::parse(json["timestamp"].as_str().unwrap(), &Rfc3339).unwrap();
    assert_eq!(parsed, ts());
    assert_eq!(
        json["dedupe_key"],
        format!(
            "{}/{}/{}",
            TENANT.as_simple(),
            TURN.as_simple(),
            REQUEST.as_simple()
        )
    );
}

#[test]
fn system_task_usage_omits_user_and_turn_and_carries_task_type_and_dedupe_key() {
    let json = encoded(&system_event());

    assert!(
        json.get("user_id").is_none(),
        "user_id must be absent, not null"
    );
    assert!(
        json.get("turn_id").is_none(),
        "turn_id must be absent, not null"
    );
    assert_eq!(json["system_task_type"], "thread_summary_update");
    assert_eq!(json["requester_type"], "system");
    assert_eq!(json["billing_outcome"], "system_task");
    assert_eq!(
        json["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            TENANT.as_simple(),
            SYSTEM_REQUEST.as_simple()
        )
    );
}

#[test]
fn missing_provider_usage_is_encoded_as_explicit_null() {
    let event = UsageEvent {
        billing_outcome: BillingOutcome::Aborted,
        usage: None,
        settlement_method: SettlementMethod::Estimated,
        ..user_event()
    };

    let json = encoded(&event);

    assert!(
        json.as_object().unwrap().contains_key("usage"),
        "usage key must be present"
    );
    assert!(json["usage"].is_null());
}

#[test]
fn dedupe_key_is_tenant_turn_request_in_32_char_lowercase_simple_hex() {
    let key = usage_dedupe_key(TENANT, TURN, REQUEST);

    let parts: Vec<&str> = key.split('/').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], "f47ac10b58cc4372a5670e02b2c3d479");
    assert_eq!(parts[1], TURN.as_simple().to_string());
    assert_eq!(parts[2], REQUEST.as_simple().to_string());
    for part in parts {
        assert_eq!(part.len(), 32);
        assert!(
            part.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }
    assert_eq!(key.len(), 98, "3 x 32 hex chars + 2 slashes");
}

#[test]
fn system_task_dedupe_key_uses_task_type_between_tenant_and_request() {
    let key = system_task_dedupe_key(TENANT, SystemTaskType::ThreadSummaryUpdate, SYSTEM_REQUEST);

    assert_eq!(
        key,
        format!(
            "f47ac10b58cc4372a5670e02b2c3d479/thread_summary_update/{}",
            SYSTEM_REQUEST.as_simple()
        )
    );
}

#[test]
fn usage_event_json_round_trip_is_exact() {
    for event in [
        user_event(),
        system_event(),
        UsageEvent {
            usage: None,
            ..user_event()
        },
        UsageEvent {
            terminal_state: TerminalState::Cancelled,
            billing_outcome: BillingOutcome::Aborted,
            settlement_method: SettlementMethod::Released,
            ..user_event()
        },
    ] {
        let payload = encode_usage_event(&event).expect("encodes");
        let decoded = decode_usage_event(&payload).expect("decodes");
        assert_eq!(decoded, event);
    }
}

#[test]
fn malformed_usage_payload_fails_to_decode() {
    assert!(decode_usage_event(b"garbage").is_err());
    assert!(decode_usage_event(b"{}").is_err());

    let mut json = encoded(&user_event());
    json.as_object_mut().unwrap().remove("dedupe_key");
    assert!(decode_usage_event(&serde_json::to_vec(&json).unwrap()).is_err());

    let mut json = encoded(&user_event());
    json["terminal_state"] = Value::from("exploded");
    assert!(decode_usage_event(&serde_json::to_vec(&json).unwrap()).is_err());
}

fn encode_error(event: &UsageEvent) -> String {
    encode_usage_event(event)
        .expect_err("expected an encode error")
        .message
}

fn decode_error(json: &Value) -> String {
    decode_usage_event(&serde_json::to_vec(json).unwrap())
        .expect_err("expected a decode error")
        .message
}

#[test]
fn encode_rejects_every_invalid_requester_type_combination() {
    let cases = [
        (
            UsageEvent {
                user_id: None,
                ..user_event()
            },
            "missing field `user_id`",
        ),
        (
            UsageEvent {
                turn_id: None,
                ..user_event()
            },
            "missing field `turn_id`",
        ),
        (
            UsageEvent {
                system_task_type: Some(SystemTaskType::ThreadSummaryUpdate),
                ..user_event()
            },
            "unexpected field `system_task_type`",
        ),
        (
            UsageEvent {
                user_id: Some(USER),
                ..system_event()
            },
            "unexpected field `user_id`",
        ),
        (
            UsageEvent {
                turn_id: Some(TURN),
                ..system_event()
            },
            "unexpected field `turn_id`",
        ),
        (
            UsageEvent {
                system_task_type: None,
                ..system_event()
            },
            "missing field `system_task_type`",
        ),
    ];

    for (event, expected) in cases {
        let message = encode_error(&event);
        assert!(message.contains(expected), "got: {message}");
    }
}

#[test]
fn decode_rejects_every_invalid_requester_type_combination() {
    let with = |mut json: Value, key: &str, value: Option<Value>| {
        let object = json.as_object_mut().unwrap();
        match value {
            Some(value) => object.insert(key.to_owned(), value),
            None => object.remove(key),
        };
        json
    };
    let user = encoded(&user_event());
    let system = encoded(&system_event());
    let cases = [
        (
            with(user.clone(), "user_id", None),
            "missing field `user_id`",
        ),
        (
            with(user.clone(), "turn_id", None),
            "missing field `turn_id`",
        ),
        (
            with(
                user,
                "system_task_type",
                Some(Value::from("thread_summary_update")),
            ),
            "unexpected field `system_task_type`",
        ),
        (
            with(
                system.clone(),
                "user_id",
                Some(Value::from(USER.to_string())),
            ),
            "unexpected field `user_id`",
        ),
        (
            with(
                system.clone(),
                "turn_id",
                Some(Value::from(TURN.to_string())),
            ),
            "unexpected field `turn_id`",
        ),
        (
            with(system, "system_task_type", None),
            "missing field `system_task_type`",
        ),
    ];

    for (json, expected) in cases {
        let message = decode_error(&json);
        assert!(message.contains(expected), "got: {message}");
    }
}

#[test]
fn decode_requires_the_usage_key_but_allows_explicit_null() {
    let mut json = encoded(&user_event());
    json.as_object_mut().unwrap().remove("usage");
    let message = decode_error(&json);
    assert!(message.contains("missing field `usage`"), "got: {message}");

    json["usage"] = Value::Null;
    let decoded = decode_usage_event(&serde_json::to_vec(&json).unwrap()).expect("decodes");
    assert_eq!(decoded.usage, None);
}

#[test]
fn valid_requester_type_shapes_still_round_trip() {
    for event in [user_event(), system_event()] {
        let payload = encode_usage_event(&event).expect("encodes");
        assert_eq!(decode_usage_event(&payload).expect("decodes"), event);
    }
}

// ---- audit events ----------------------------------------------------------

fn turn_audit(event_type: TurnAuditEventType) -> TurnAuditEvent {
    TurnAuditEvent {
        event_type,
        tenant_id: TENANT,
        requester_user_id: USER,
        chat_id: CHAT,
        turn_id: TURN,
        request_id: REQUEST,
        selected_model: "gpt-5.2".to_owned(),
        effective_model: "gpt-5.2-mini".to_owned(),
        usage: AuditUsage {
            input_tokens: 10,
            output_tokens: 20,
        },
        latency_ms: 1500,
        tool_calls: AuditToolCalls {
            web_search: 1,
            file_search: 2,
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: "unknown".to_owned(),
            },
        },
        timestamp: ts(),
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
    }
}

fn audit_json(event: &MiniChatAuditEvent) -> Value {
    serde_json::from_slice(&encode_audit_event(event).expect("encodes")).expect("payload is JSON")
}

fn is_empty_value(value: &Value) -> bool {
    value.is_null()
        || value.as_str().is_some_and(str::is_empty)
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

fn retry_event() -> TurnMutationAuditEvent {
    TurnMutationAuditEvent::TurnRetry {
        tenant_id: TENANT,
        actor_user_id: USER,
        chat_id: CHAT,
        original_request_id: REQUEST,
        new_request_id: SYSTEM_REQUEST,
        timestamp: ts(),
    }
}

#[test]
fn turn_audit_event_type_is_turn_completed_or_turn_failed() {
    let completed = audit_json(&MiniChatAuditEvent::Turn(turn_audit(
        TurnAuditEventType::TurnCompleted,
    )));
    let failed = audit_json(&MiniChatAuditEvent::Turn(turn_audit(
        TurnAuditEventType::TurnFailed,
    )));

    assert_eq!(completed["event_type"], "turn_completed");
    assert_eq!(failed["event_type"], "turn_failed");
}

#[test]
fn turn_audit_event_carries_identities_and_has_empty_content_fields() {
    let json = audit_json(&MiniChatAuditEvent::Turn(turn_audit(
        TurnAuditEventType::TurnFailed,
    )));

    assert_eq!(json["tenant_id"], TENANT.to_string());
    assert_eq!(json["requester_user_id"], USER.to_string());
    assert_eq!(json["chat_id"], CHAT.to_string());
    assert_eq!(json["turn_id"], TURN.to_string());
    assert_eq!(json["request_id"], REQUEST.to_string());
    assert_eq!(json["selected_model"], "gpt-5.2");
    assert_eq!(json["effective_model"], "gpt-5.2-mini");
    assert_eq!(json["policy_decisions"]["quota"]["decision"], "unknown");
    assert_eq!(json["tool_calls"]["web_search"], 1);
    assert_eq!(json["tool_calls"]["file_search"], 2);
    assert!(
        json["tool_calls"].get("code_interpreter").is_none(),
        "no code-interpreter count"
    );
    for field in [
        "prompt",
        "response",
        "attachments",
        "license",
        "quota_scope",
    ] {
        let value = json.get(field).cloned().unwrap_or(Value::Null);
        assert!(
            is_empty_value(&value),
            "`{field}` must be empty, got {value}"
        );
    }
}

#[test]
fn audit_events_round_trip_exactly() {
    let events = [
        MiniChatAuditEvent::Turn(turn_audit(TurnAuditEventType::TurnCompleted)),
        MiniChatAuditEvent::Turn(turn_audit(TurnAuditEventType::TurnFailed)),
        MiniChatAuditEvent::Mutation(retry_event()),
        MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnEdit {
            tenant_id: TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            original_request_id: REQUEST,
            new_request_id: SYSTEM_REQUEST,
            timestamp: ts(),
        }),
        MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnDelete {
            tenant_id: TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            request_id: REQUEST,
            timestamp: ts(),
        }),
    ];

    for event in events {
        let payload = encode_audit_event(&event).expect("encodes");
        assert_eq!(decode_audit_event(&payload).expect("decodes"), event);
    }
}

#[test]
fn retry_and_edit_audit_events_carry_original_and_new_request_ids() {
    let retry = audit_json(&MiniChatAuditEvent::Mutation(retry_event()));
    let edit = audit_json(&MiniChatAuditEvent::Mutation(
        TurnMutationAuditEvent::TurnEdit {
            tenant_id: TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            original_request_id: REQUEST,
            new_request_id: SYSTEM_REQUEST,
            timestamp: ts(),
        },
    ));

    for (json, name) in [(retry, "turn_retry"), (edit, "turn_edit")] {
        assert_eq!(json["event_type"], name);
        assert_eq!(json["tenant_id"], TENANT.to_string());
        assert_eq!(json["actor_user_id"], USER.to_string());
        assert_eq!(json["chat_id"], CHAT.to_string());
        assert_eq!(json["original_request_id"], REQUEST.to_string());
        assert_eq!(json["new_request_id"], SYSTEM_REQUEST.to_string());
        assert!(json.get("timestamp").is_some());
        assert!(json.get("request_id").is_none());
    }
}

#[test]
fn delete_audit_event_carries_request_id_only() {
    let json = audit_json(&MiniChatAuditEvent::Mutation(
        TurnMutationAuditEvent::TurnDelete {
            tenant_id: TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            request_id: REQUEST,
            timestamp: ts(),
        },
    ));

    assert_eq!(json["event_type"], "turn_delete");
    assert_eq!(json["request_id"], REQUEST.to_string());
    assert_eq!(json["actor_user_id"], USER.to_string());
    assert_eq!(json["tenant_id"], TENANT.to_string());
    assert!(json.get("original_request_id").is_none());
    assert!(json.get("new_request_id").is_none());
}

#[test]
fn malformed_audit_payload_or_unknown_event_type_fails_to_decode() {
    assert!(decode_audit_event(b"garbage").is_err());
    assert!(decode_audit_event(b"{}").is_err());

    let mut json = audit_json(&MiniChatAuditEvent::Turn(turn_audit(
        TurnAuditEventType::TurnFailed,
    )));
    json["event_type"] = Value::from("turn_cancelled");
    assert!(
        decode_audit_event(&serde_json::to_vec(&json).unwrap()).is_err(),
        "there is no turn_cancelled event type"
    );
}
