#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4 and 5: `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`,
//! `cpt-cf-mini-chat-algo-quota-billing-system-task-usage` and
//! `cpt-cf-mini-chat-dod-quota-billing-usage-events`: the event construction.

use std::collections::BTreeSet;

use mini_chat_sdk::{
    BillingOutcome, RequesterType, SettlementMethod, SystemTaskType, TerminalState, UsageTokens,
    decode_usage_event, encode_usage_event, system_task_dedupe_key, usage_dedupe_key,
};
use serde_json::Value;
use uuid::Uuid;

use super::{
    SystemTaskUsageArgs, TurnUsageEventArgs, build_system_task_event, build_turn_usage_event,
    event_usage,
};
use crate::domain::service::quota::settle::ToolCallCounts;
use crate::domain::service::quota::test_support::noon;

const TENANT: Uuid = Uuid::from_u128(0xAAAA_0000_0000_0000_0000_0000_0000_0001);
const USER: Uuid = Uuid::from_u128(0xBBBB_0000_0000_0000_0000_0000_0000_0002);
const CHAT: Uuid = Uuid::from_u128(0xCCCC_0000_0000_0000_0000_0000_0000_0003);
const TURN: Uuid = Uuid::from_u128(0xDDDD_0000_0000_0000_0000_0000_0000_0004);
const REQUEST: Uuid = Uuid::from_u128(0xEEEE_0000_0000_0000_0000_0000_0000_0005);

fn tokens(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 11,
        cache_write_input_tokens: 12,
        reasoning_tokens: 13,
    }
}

fn args() -> TurnUsageEventArgs {
    TurnUsageEventArgs {
        tenant_id: TENANT,
        user_id: USER,
        chat_id: CHAT,
        turn_id: TURN,
        request_id: REQUEST,
        effective_model: "s".to_owned(),
        selected_model: "p".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        settlement_method: SettlementMethod::Actual,
        committed_credits_micro: 1_200_000,
        policy_version_applied: 7,
        usage: Some(tokens(900, 300)),
        tool_calls: ToolCallCounts {
            web_search: 1,
            code_interpreter: 2,
            file_search: 3,
        },
        now: noon(),
    }
}

fn payload_keys(payload: &[u8]) -> BTreeSet<String> {
    let value: Value = serde_json::from_slice(payload).unwrap();
    value.as_object().unwrap().keys().cloned().collect()
}

// ---- the usage field ---------------------------------------------------------------------------------

#[test]
fn the_usage_field_follows_the_settlement_method() {
    let provider = tokens(900, 300);

    assert_eq!(
        event_usage(SettlementMethod::Actual, Some(&provider)),
        Some(provider)
    );
    assert_eq!(
        event_usage(SettlementMethod::Actual, None),
        None,
        "a completed turn that received no usage object"
    );
    assert_eq!(
        event_usage(SettlementMethod::Estimated, Some(&provider)),
        None,
        "the estimated split is not emitted"
    );
    assert_eq!(event_usage(SettlementMethod::Estimated, None), None);
    assert_eq!(
        event_usage(SettlementMethod::Released, Some(&provider)),
        Some(UsageTokens {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        })
    );
    assert_eq!(
        event_usage(SettlementMethod::Released, None),
        Some(UsageTokens {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        })
    );
}

// ---- the turn event ----------------------------------------------------------------------------------

#[test]
fn the_turn_event_carries_the_persisted_ids_models_credits_and_counters() {
    let event = build_turn_usage_event(&args());

    assert_eq!(event.tenant_id, TENANT);
    assert_eq!(event.user_id, Some(USER));
    assert_eq!(event.chat_id, CHAT);
    assert_eq!(event.turn_id, Some(TURN));
    assert_eq!(event.request_id, REQUEST);
    assert_eq!(event.effective_model, "s");
    assert_eq!(event.selected_model, "p");
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.billing_outcome, BillingOutcome::Completed);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, 1_200_000);
    assert_eq!(event.policy_version_applied, 7);
    assert_eq!(event.usage, Some(tokens(900, 300)));
    assert_eq!(event.web_search_calls, 1);
    assert_eq!(event.code_interpreter_calls, 2);
    assert_eq!(event.file_search_calls, 3);
    assert_eq!(event.timestamp, noon());
    assert_eq!(event.requester_type, RequesterType::User);
    assert_eq!(event.system_task_type, None);
}

#[test]
fn the_dedupe_key_is_the_simple_hex_tenant_turn_and_request_joined_by_slashes() {
    let event = build_turn_usage_event(&args());

    assert_eq!(event.dedupe_key, usage_dedupe_key(TENANT, TURN, REQUEST));
    let parts: Vec<&str> = event.dedupe_key.split('/').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], TENANT.simple().to_string());
    assert_eq!(parts[1], TURN.simple().to_string());
    assert_eq!(parts[2], REQUEST.simple().to_string());
    for part in parts {
        assert_eq!(part.len(), 32);
        assert!(
            part.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}

#[test]
fn the_encoded_turn_event_has_exactly_the_documented_fields() {
    let payload = encode_usage_event(&build_turn_usage_event(&args())).unwrap();

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
    .map(str::to_owned)
    .collect();
    assert_eq!(payload_keys(&payload), expected);
    let value: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(value["requester_type"], "user");
    assert_eq!(value["terminal_state"], "completed");
    assert_eq!(value["billing_outcome"], "completed");
    assert_eq!(value["settlement_method"], "actual");
    assert_eq!(value["actual_credits_micro"], 1_200_000);
    assert_eq!(value["usage"]["input_tokens"], 900);
    assert_eq!(value["usage"]["output_tokens"], 300);
    assert_eq!(value["usage"]["cache_read_input_tokens"], 11);
    assert_eq!(value["usage"]["cache_write_input_tokens"], 12);
    assert_eq!(value["usage"]["reasoning_tokens"], 13);
    assert_eq!(value["timestamp"], "2026-03-10T12:00:00Z");
    for absent in [
        "event_type",
        "reserved_credits_micro",
        "reserve_tokens",
        "committed_credits_micro",
        "overshoot_capped",
        "error_code",
        "completion_signal",
        "quota_decision",
        "downgrade_from",
        "downgrade_reason",
        "system_task_type",
    ] {
        assert!(value.get(absent).is_none(), "{absent}");
    }
}

#[test]
fn the_event_round_trips_through_the_sdk_codec() {
    let event = build_turn_usage_event(&args());

    let decoded = decode_usage_event(&encode_usage_event(&event).unwrap()).unwrap();

    assert_eq!(decoded, event);
}

#[test]
fn an_estimated_turn_event_has_a_null_usage() {
    let mut estimated = args();
    estimated.billing_outcome = BillingOutcome::Aborted;
    estimated.terminal_state = TerminalState::Cancelled;
    estimated.settlement_method = SettlementMethod::Estimated;
    estimated.committed_credits_micro = 1_050_000;

    let event = build_turn_usage_event(&estimated);
    let payload = encode_usage_event(&event).unwrap();

    assert_eq!(event.usage, None);
    let value: Value = serde_json::from_slice(&payload).unwrap();
    assert!(value["usage"].is_null());
    assert!(value.as_object().unwrap().contains_key("usage"));
    assert_eq!(value["billing_outcome"], "aborted");
    assert_eq!(value["terminal_state"], "cancelled");
    assert_eq!(value["actual_credits_micro"], 1_050_000);
}

#[test]
fn a_released_turn_event_reports_all_five_usage_fields_as_zero() {
    let mut released = args();
    released.billing_outcome = BillingOutcome::Failed;
    released.terminal_state = TerminalState::Failed;
    released.settlement_method = SettlementMethod::Released;
    released.committed_credits_micro = 0;

    let event = build_turn_usage_event(&released);

    assert_eq!(
        event.usage,
        Some(UsageTokens {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        })
    );
    assert_eq!(event.actual_credits_micro, 0);
}

#[test]
fn a_completed_turn_without_a_usage_object_has_a_null_usage_and_zero_credits() {
    let mut no_usage = args();
    no_usage.usage = None;
    no_usage.committed_credits_micro = 0;

    let event = build_turn_usage_event(&no_usage);

    assert_eq!(event.usage, None);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, 0);
}

#[test]
fn a_turn_without_reserve_has_empty_model_fields_and_policy_version_zero() {
    let mut no_reserve = args();
    no_reserve.effective_model = String::new();
    no_reserve.selected_model = String::new();
    no_reserve.policy_version_applied = 0;
    no_reserve.billing_outcome = BillingOutcome::Aborted;
    no_reserve.terminal_state = TerminalState::Failed;
    no_reserve.settlement_method = SettlementMethod::Estimated;
    no_reserve.committed_credits_micro = 0;
    no_reserve.usage = None;

    let event = build_turn_usage_event(&no_reserve);

    assert_eq!(event.effective_model, "");
    assert_eq!(event.selected_model, "");
    assert_eq!(event.policy_version_applied, 0);
    assert_eq!(event.actual_credits_micro, 0);
    assert!(encode_usage_event(&event).is_ok());
}

// ---- the system-task event ----------------------------------------------------------------------------

fn system_args() -> SystemTaskUsageArgs {
    SystemTaskUsageArgs {
        tenant_id: TENANT,
        chat_id: CHAT,
        system_request_id: REQUEST,
        summary_model: "summary-model".to_owned(),
        usage: Some(tokens(2000, 150)),
        now: noon(),
    }
}

#[test]
fn the_system_task_event_is_not_charged_and_carries_no_user_or_turn() {
    let event = build_system_task_event(&system_args());

    assert_eq!(event.tenant_id, TENANT);
    assert_eq!(event.user_id, None);
    assert_eq!(event.turn_id, None);
    assert_eq!(event.chat_id, CHAT);
    assert_eq!(event.request_id, REQUEST);
    assert_eq!(event.effective_model, "summary-model");
    assert_eq!(event.selected_model, "summary-model");
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.billing_outcome, BillingOutcome::SystemTask);
    assert_eq!(event.settlement_method, SettlementMethod::None);
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.policy_version_applied, 0);
    assert_eq!(event.web_search_calls, 0);
    assert_eq!(event.code_interpreter_calls, 0);
    assert_eq!(event.file_search_calls, 0);
    assert_eq!(event.timestamp, noon());
    assert_eq!(event.requester_type, RequesterType::System);
    assert_eq!(
        event.system_task_type,
        Some(SystemTaskType::ThreadSummaryUpdate)
    );
    assert_eq!(event.usage, Some(tokens(2000, 150)));
}

#[test]
fn the_system_task_dedupe_key_is_stable_across_retries_of_the_same_request() {
    let first = build_system_task_event(&system_args());
    let second = build_system_task_event(&system_args());

    assert_eq!(first.dedupe_key, second.dedupe_key);
    assert_eq!(
        first.dedupe_key,
        system_task_dedupe_key(TENANT, SystemTaskType::ThreadSummaryUpdate, REQUEST)
    );
    assert_eq!(
        first.dedupe_key,
        format!(
            "{}/thread_summary_update/{}",
            TENANT.simple(),
            REQUEST.simple()
        )
    );
}

#[test]
fn the_encoded_system_task_event_omits_user_and_turn_instead_of_nulling_them() {
    let payload = encode_usage_event(&build_system_task_event(&system_args())).unwrap();

    let keys = payload_keys(&payload);
    assert!(!keys.contains("user_id"));
    assert!(!keys.contains("turn_id"));
    assert!(keys.contains("system_task_type"));
    let value: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(value["requester_type"], "system");
    assert_eq!(value["system_task_type"], "thread_summary_update");
    assert_eq!(value["billing_outcome"], "system_task");
    assert_eq!(value["settlement_method"], "none");
    assert_eq!(value["actual_credits_micro"], 0);
    assert_eq!(value["usage"]["input_tokens"], 2000);
    assert!(value.get("event_type").is_none());
}

#[test]
fn a_system_task_without_reported_usage_has_a_null_usage() {
    let mut no_usage = system_args();
    no_usage.usage = None;

    let payload = encode_usage_event(&build_system_task_event(&no_usage)).unwrap();

    let value: Value = serde_json::from_slice(&payload).unwrap();
    assert!(value["usage"].is_null());
}

#[test]
fn a_system_task_event_round_trips_through_the_sdk_codec() {
    let event = build_system_task_event(&system_args());

    let decoded = decode_usage_event(&encode_usage_event(&event).unwrap()).unwrap();

    assert_eq!(decoded, event);
}
