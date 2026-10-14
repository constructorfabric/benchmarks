//! Usage event construction tests: the payload fields and omissions, the committed credits as
//! the authoritative amount, the `usage` shape per settlement method, the dedupe key in the
//! simple hexadecimal form (user and system forms, accepted by the outbox prefix check), the
//! placeholder event of a turn without a reserve and the system-task event.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-usage-event`, definition of
//! done `cpt-cf-mini-chat-dod-quota-and-billing-usage-event`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use mini_chat_sdk::UsageEvent;
use serde_json::Value;
use uuid::Uuid;

use super::{
    NoReserveUsageEventInput, SystemTaskUsageInput, ToolCallCounts, TurnUsageEventInput,
    build_no_reserve_usage_event, build_system_task_usage_event, build_turn_usage_event,
};
use crate::domain::quota::billing::{BillingOutcome, SettlementMethod, TerminalState};
use crate::test_support::quota::{usage, utc, validate_dedupe_key};

const TENANT: Uuid = Uuid::from_u128(0xA000_0000_0000_4000_8000_0000_0000_00A1);
const USER: Uuid = Uuid::from_u128(0x1111_0000_0000_4000_8000_0000_0000_0001);
const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_4000_8000_0000_0000_0001);
const TURN: Uuid = Uuid::from_u128(0x7001_0000_0000_4000_8000_0000_0000_0001);
const REQUEST: Uuid = Uuid::from_u128(0x7002_0000_0000_4000_8000_0000_0000_0001);
const SYSTEM_REQUEST: Uuid = Uuid::from_u128(0x5157_0000_0000_4000_8000_0000_0000_0001);

fn input(method: SettlementMethod) -> TurnUsageEventInput<'static> {
    TurnUsageEventInput {
        tenant_id: TENANT,
        user_id: USER,
        chat_id: CHAT,
        turn_id: TURN,
        request_id: REQUEST,
        effective_model: "standard-1",
        selected_model: "premium-1",
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        settlement_method: method,
        usage: Some(usage(900, 300)),
        committed_credits_micro: 1_200_000,
        policy_version_applied: 3,
        tool_counts: ToolCallCounts {
            web_search: 1,
            code_interpreter: 2,
            file_search: 3,
        },
        timestamp: utc(2026, 2, 28, 15, 30, 5),
    }
}

fn payload(event: &UsageEvent) -> Value {
    serde_json::to_value(event).unwrap()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

// ── a settled turn ───────────────────────────────────────────────────────────

#[test]
fn the_dedupe_key_is_tenant_turn_request_in_the_simple_form() {
    let event = build_turn_usage_event(&input(SettlementMethod::Actual));
    let expected = format!("{}/{}/{}", TENANT.simple(), TURN.simple(), REQUEST.simple());
    assert_eq!(event.dedupe_key.as_deref(), Some(expected.as_str()));
    let key = event.dedupe_key.unwrap();
    assert_eq!(key.len(), 32 + 1 + 32 + 1 + 32);
    assert!(
        key.chars()
            .all(|c| c == '/' || c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "lowercase hex without hyphens or braces: {key}"
    );
}

#[test]
fn the_tenant_prefix_check_of_the_outbox_enqueuer_holds_by_construction() {
    let event = build_turn_usage_event(&input(SettlementMethod::Actual));
    assert_eq!(
        validate_dedupe_key(event.tenant_id, event.dedupe_key.as_deref().unwrap()),
        Ok(())
    );
}

#[test]
fn the_payload_carries_exactly_the_specified_fields() {
    let json = payload(&build_turn_usage_event(&input(SettlementMethod::Actual)));
    let expected: BTreeSet<String> = [
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
    .map(ToOwned::to_owned)
    .collect();
    assert_eq!(keys(&json), expected);
}

#[test]
fn the_payload_has_no_excluded_field_and_no_event_type() {
    let json = payload(&build_turn_usage_event(&input(SettlementMethod::Actual)));
    for absent in [
        "reserved_credits_micro",
        "reserve_tokens",
        "error_code",
        "quota_decision",
        "downgrade_from",
        "downgrade_reason",
        "completion_signal",
        "committed_credits_micro",
        "overshoot_capped",
        "event_type",
        "system_task_type",
    ] {
        assert!(
            json.get(absent).is_none(),
            "{absent} must not be in the payload: {json}"
        );
    }
}

#[test]
fn the_values_come_from_the_turn_and_the_settlement() {
    let event = build_turn_usage_event(&input(SettlementMethod::Actual));
    assert_eq!(event.tenant_id, TENANT);
    assert_eq!(event.user_id, Some(USER));
    assert_eq!(event.chat_id, CHAT);
    assert_eq!(event.turn_id, Some(TURN));
    assert_eq!(event.request_id, REQUEST);
    assert_eq!(event.effective_model, "standard-1");
    assert_eq!(event.selected_model, "premium-1");
    assert_eq!(event.terminal_state, "completed");
    assert_eq!(event.billing_outcome, "completed");
    assert_eq!(event.settlement_method, "actual");
    assert_eq!(event.actual_credits_micro, 1_200_000);
    assert_eq!(event.policy_version_applied, 3);
    assert_eq!(event.web_search_calls, 1);
    assert_eq!(event.code_interpreter_calls, 2);
    assert_eq!(event.file_search_calls, 3);
    assert_eq!(event.requester_type, "user");
    assert_eq!(event.system_task_type, None);
}

#[test]
fn the_uuid_fields_of_the_json_keep_the_hyphenated_form() {
    let json = payload(&build_turn_usage_event(&input(SettlementMethod::Actual)));
    assert_eq!(json["tenant_id"], TENANT.hyphenated().to_string());
    assert_eq!(json["user_id"], USER.hyphenated().to_string());
    assert_eq!(json["chat_id"], CHAT.hyphenated().to_string());
    assert_eq!(json["turn_id"], TURN.hyphenated().to_string());
    assert_eq!(json["request_id"], REQUEST.hyphenated().to_string());
}

#[test]
fn the_timestamp_is_an_rfc_3339_utc_instant() {
    let json = payload(&build_turn_usage_event(&input(SettlementMethod::Actual)));
    assert_eq!(json["timestamp"], "2026-02-28T15:30:05Z");
}

#[test]
fn an_actual_settlement_reports_the_provider_usage_with_its_breakdown() {
    let mut turn = input(SettlementMethod::Actual);
    let mut reported = usage(900, 300);
    reported.cache_read_input_tokens = 100;
    reported.cache_write_input_tokens = 7;
    reported.reasoning_tokens = 40;
    turn.usage = Some(reported);
    let json = payload(&build_turn_usage_event(&turn));
    assert_eq!(json["usage"]["input_tokens"], 900);
    assert_eq!(json["usage"]["output_tokens"], 300);
    assert_eq!(json["usage"]["cache_read_input_tokens"], 100);
    assert_eq!(json["usage"]["cache_write_input_tokens"], 7);
    assert_eq!(json["usage"]["reasoning_tokens"], 40);
}

#[test]
fn an_estimated_settlement_has_null_usage_because_the_split_is_not_emitted() {
    let mut turn = input(SettlementMethod::Estimated);
    turn.billing_outcome = BillingOutcome::Aborted;
    turn.terminal_state = TerminalState::Cancelled;
    turn.committed_credits_micro = 1_050_000;
    let event = build_turn_usage_event(&turn);
    assert_eq!(event.usage, None);
    assert_eq!(payload(&event)["usage"], Value::Null);
    assert_eq!(event.settlement_method, "estimated");
    assert_eq!(event.billing_outcome, "aborted");
    assert_eq!(event.terminal_state, "cancelled");
    assert_eq!(event.actual_credits_micro, 1_050_000);
}

#[test]
fn a_released_settlement_has_zero_input_and_output_tokens() {
    let mut turn = input(SettlementMethod::Released);
    turn.billing_outcome = BillingOutcome::Failed;
    turn.terminal_state = TerminalState::Failed;
    turn.committed_credits_micro = 0;
    let event = build_turn_usage_event(&turn);
    let reported = event.usage.expect("zero usage object");
    assert_eq!(reported.input_tokens, 0);
    assert_eq!(reported.output_tokens, 0);
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.settlement_method, "released");
}

#[test]
fn the_billing_outcome_and_the_terminal_state_are_kept_apart() {
    // the orphan watchdog sets state `failed` with the billing outcome `aborted`
    let mut turn = input(SettlementMethod::Estimated);
    turn.terminal_state = TerminalState::Failed;
    turn.billing_outcome = BillingOutcome::Aborted;
    let event = build_turn_usage_event(&turn);
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.billing_outcome, "aborted");
}

#[test]
fn the_committed_credits_are_carried_unchanged_as_the_authoritative_amount() {
    // an overshoot capped at the reserve: the amount is the committed one, usage stays uncapped
    let mut turn = input(SettlementMethod::Actual);
    turn.usage = Some(usage(11_000, 500));
    turn.committed_credits_micro = 2_500_000;
    let event = build_turn_usage_event(&turn);
    assert_eq!(event.actual_credits_micro, 2_500_000);
    assert_eq!(event.usage.unwrap().input_tokens, 11_000);
}

// ── a turn without a reserve ─────────────────────────────────────────────────

#[test]
fn a_turn_without_a_reserve_gets_the_aborted_estimated_placeholder() {
    let event = build_no_reserve_usage_event(&NoReserveUsageEventInput {
        tenant_id: TENANT,
        user_id: Some(USER),
        chat_id: CHAT,
        turn_id: TURN,
        request_id: REQUEST,
        terminal_state: TerminalState::Failed,
        tool_counts: ToolCallCounts::default(),
        timestamp: utc(2026, 2, 28, 15, 30, 5),
    });
    assert_eq!(event.billing_outcome, "aborted");
    assert_eq!(event.settlement_method, "estimated");
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.usage, None);
    assert_eq!(event.effective_model, "");
    assert_eq!(event.selected_model, "");
    assert_eq!(event.policy_version_applied, 0);
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.requester_type, "user");
    let expected = format!("{}/{}/{}", TENANT.simple(), TURN.simple(), REQUEST.simple());
    assert_eq!(event.dedupe_key.as_deref(), Some(expected.as_str()));
    let json = payload(&event);
    assert_eq!(json["usage"], Value::Null);
}

// ── a system task ────────────────────────────────────────────────────────────

fn system_event() -> UsageEvent {
    build_system_task_usage_event(&SystemTaskUsageInput {
        tenant_id: TENANT,
        chat_id: CHAT,
        summary_model: "standard-1",
        usage: usage(1200, 300),
        system_request_id: SYSTEM_REQUEST,
        timestamp: utc(2026, 2, 28, 15, 30, 5),
    })
}

#[test]
fn a_system_task_event_has_the_system_attribution_fields() {
    let event = system_event();
    assert_eq!(event.requester_type, "system");
    assert_eq!(event.billing_outcome, "system_task");
    assert_eq!(event.settlement_method, "none");
    assert_eq!(event.terminal_state, "completed");
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.policy_version_applied, 0);
    assert_eq!(event.web_search_calls, 0);
    assert_eq!(event.code_interpreter_calls, 0);
    assert_eq!(event.file_search_calls, 0);
    assert_eq!(
        event.system_task_type.as_deref(),
        Some("thread_summary_update")
    );
    assert_eq!(event.tenant_id, TENANT);
    assert_eq!(event.chat_id, CHAT);
    assert_eq!(event.effective_model, "standard-1");
    assert_eq!(event.selected_model, "standard-1");
}

#[test]
fn a_system_task_event_carries_the_token_counts_of_the_summary_call() {
    let event = system_event();
    let reported = event.usage.expect("usage of the summary call");
    assert_eq!(reported.input_tokens, 1200);
    assert_eq!(reported.output_tokens, 300);
}

#[test]
fn a_system_task_event_omits_user_id_and_turn_id_entirely() {
    let json = payload(&system_event());
    assert!(json.get("user_id").is_none(), "absent, not null: {json}");
    assert!(json.get("turn_id").is_none(), "absent, not null: {json}");
    assert_eq!(json["system_task_type"], "thread_summary_update");
    assert_eq!(json["requester_type"], "system");
}

#[test]
fn the_system_task_dedupe_key_uses_the_task_type_and_the_system_request_id() {
    let event = system_event();
    let expected = format!(
        "{}/thread_summary_update/{}",
        TENANT.simple(),
        SYSTEM_REQUEST.simple()
    );
    assert_eq!(event.dedupe_key.as_deref(), Some(expected.as_str()));
    assert_eq!(
        validate_dedupe_key(TENANT, event.dedupe_key.as_deref().unwrap()),
        Ok(())
    );
}

#[test]
fn a_retry_of_the_same_system_request_builds_the_same_key() {
    assert_eq!(system_event().dedupe_key, system_event().dedupe_key);
}

#[test]
fn the_payload_never_carries_a_provider_identifier_or_a_reserve_field() {
    for event in [
        build_turn_usage_event(&input(SettlementMethod::Actual)),
        system_event(),
    ] {
        let text = serde_json::to_string(&event).unwrap();
        for banned in [
            "file-",
            "vs_",
            "resp_",
            "assistant-",
            "reserved_credits",
            "reserve_tokens",
        ] {
            assert!(!text.contains(banned), "{banned} in {text}");
        }
    }
}
