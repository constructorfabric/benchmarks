//! The thread-summary handler over fakes (store, transport, policy, usage queue, metrics) and a
//! virtual timer: the mapping of every pre-check, range, model, call, parse and commit outcome to
//! `Ok`, `Retry` or `Reject` and to the metrics (`success`, `provider_error`, `empty_summary`,
//! `retry`, `model_unavailable`, `frontier_deleted`, `base_missing`, CAS conflicts, fallbacks), the
//! retry budget with the 0-based `attempts`, the summary model resolution (empty id, missing,
//! disabled, policy and provider failures, no user id on the snapshot read), the system-task usage
//! event with its stable `system_request_id`, the accepted duplicate provider calls, and the logs
//! that carry the chat id and the request id but never message, prompt or summary content.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithms
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-handler` (`inst-sh-parse`,
//! `inst-sh-malformed`, `inst-sh-return-malformed`, `inst-sh-precheck`, `inst-sh-precheck-match`,
//! `inst-sh-case-chat-gone`, `inst-sh-case-base-missing`, `inst-sh-case-conflict`,
//! `inst-sh-case-target-deleted`, `inst-sh-case-precheck-db`, `inst-sh-case-ready`, `inst-sh-range`,
//! `inst-sh-range-db`, `inst-sh-return-range-db`, `inst-sh-range-empty`,
//! `inst-sh-return-range-empty`, `inst-sh-model`, `inst-sh-model-unavailable`,
//! `inst-sh-return-model-unavailable`, `inst-sh-resolution-error`,
//! `inst-sh-return-resolution-error`, `inst-sh-prompt`, `inst-sh-call`, `inst-sh-call-failed`,
//! `inst-sh-return-call-failed`, `inst-sh-parse-response`, `inst-sh-empty`, `inst-sh-return-empty`,
//! `inst-sh-commit`, `inst-sh-commit-match`, `inst-sh-case-committed`,
//! `inst-sh-case-commit-target-deleted`, `inst-sh-case-commit-base-missing`,
//! `inst-sh-case-commit-conflict`, `inst-sh-case-commit-db`, `inst-sh-duplicates-accepted`,
//! `inst-sh-no-content-logs`, `inst-sh-return`), `cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget` (`inst-rb-last`,
//! `inst-rb-return-reject`, `inst-rb-else`, `inst-rb-return-retry`), `cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution`
//! (`inst-mr-model-id`, `inst-mr-snapshot`, `inst-mr-snapshot-failed`,
//! `inst-mr-return-snapshot-failed`, `inst-mr-lookup`, `inst-mr-unavailable`,
//! `inst-mr-return-unavailable`, `inst-mr-provider`, `inst-mr-provider-failed`,
//! `inst-mr-return-provider-failed`, `inst-mr-no-quota`, `inst-mr-return`), `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage`
//! (`inst-su-build`, `inst-su-dedupe`, `inst-su-stable-id`, `inst-su-no-turn`, `inst-su-no-audit`)
//! and `cpt-cf-mini-chat-algo-background-maintenance-metrics` (`inst-mt-execution`, `inst-mt-cas`, `inst-mt-fallback`, `inst-mt-labels`); state
//! `cpt-cf-mini-chat-state-background-maintenance-summary-frontier` (`inst-fr-unchanged`); flow
//! `cpt-cf-mini-chat-flow-background-maintenance-long-conversation` (`inst-lc-handle`,
//! `inst-lc-retry`, `inst-lc-redeliver`, `inst-lc-rejected`, `inst-lc-dead-letter`); definitions of
//! done `cpt-cf-mini-chat-dod-background-maintenance-summary-failure-retry` and
//! `cpt-cf-mini-chat-dod-background-maintenance-system-task-isolation`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::{Value, json};
use time::OffsetDateTime;
use tracing_test::traced_test;
use uuid::Uuid;

use super::{HandlerOutcome, retry_budget};
use crate::domain::maintenance::settings::SummaryWorkerSettings;
use crate::domain::maintenance::summary_call::TransportFailure;
use crate::domain::maintenance::summary_prompt::RangeMessage;
use crate::domain::maintenance::summary_store::PreCheck;
use crate::test_support::TENANT_A;
use crate::test_support::maintenance::{
    CC_ALIAS, CommitKind, PolicyMode, SUMMARY_CC, SUMMARY_MODEL, Scripted, UnitSummary, catalog,
    catalog_with, catalog_without, chat_answer, context_length_error, default_answer, label,
    range_msg, reason, responses_answer, settings_for, summary_entry, summary_payload,
    summary_text, unit_summary, unit_summary_with,
};
use crate::test_support::quota::default_now;
use crate::test_support::send::t;

const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_00B1);
const REQUEST: Uuid = Uuid::from_u128(0x5151_0000_0000_0000_0000_0000_0000_0B07);
const BASE_ID: Uuid = Uuid::from_u128(0xBA5E_0000_0000_0000_0000_0000_0000_0B01);
const TARGET_ID: Uuid = Uuid::from_u128(0x7A67_0000_0000_0000_0000_0000_0000_0B01);

fn payload(with_base: bool) -> Value {
    summary_payload(
        TENANT_A,
        CHAT,
        with_base.then_some((t(5), BASE_ID)),
        (t(40), TARGET_ID),
        REQUEST,
    )
}

fn unit() -> UnitSummary {
    unit_summary(settings_for(SUMMARY_MODEL))
}

async fn run(unit: &UnitSummary, payload: &Value, attempts: i16) -> HandlerOutcome {
    unit.worker
        .handle(&serde_json::to_vec(payload).unwrap(), attempts)
        .await
}

fn max_attempts_reason(n: u32) -> String {
    format!("thread summary: max attempts ({n}) reached")
}

// ── the retry budget ─────────────────────────────────────────────────────────

#[test]
fn the_budget_rejects_on_the_max_attempts_th_delivery() {
    // inst-rb-last, inst-rb-return-reject: `attempts` is 0-based, so the third delivery of
    // three carries `attempts` 2
    assert_eq!(retry_budget(0, 3), HandlerOutcome::Retry);
    assert_eq!(retry_budget(1, 3), HandlerOutcome::Retry);
    assert_eq!(
        retry_budget(2, 3),
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
}

#[test]
fn a_later_delivery_than_the_limit_is_rejected_too() {
    // inst-rb-last: `attempts` at least `max_attempts` minus 1
    assert_eq!(
        retry_budget(3, 3),
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
    assert_eq!(
        retry_budget(40, 3),
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
}

#[test]
fn a_budget_of_one_rejects_the_first_delivery() {
    // inst-rb-last
    assert_eq!(
        retry_budget(0, 1),
        HandlerOutcome::Reject(max_attempts_reason(1))
    );
}

#[test]
fn a_budget_of_two_retries_once() {
    // inst-rb-else, inst-rb-return-retry
    assert_eq!(retry_budget(0, 2), HandlerOutcome::Retry);
    assert_eq!(
        retry_budget(1, 2),
        HandlerOutcome::Reject(max_attempts_reason(2))
    );
}

#[test]
fn the_reject_reason_names_the_limit_and_nothing_else() {
    // inst-rb-return-reject: no content, no summary text, no provider identifier
    let HandlerOutcome::Reject(reason) = retry_budget(2, 3) else {
        panic!("rejected");
    };
    assert_eq!(reason, "thread summary: max attempts (3) reached");
}

// ── the payload ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_malformed_payload_is_rejected_at_once_without_any_call() {
    // inst-sh-parse, inst-sh-malformed, inst-sh-return-malformed: no execution result either
    let unit = unit();
    for bytes in [&b"not json"[..], b"{}", b"[]", b""] {
        let outcome = unit.worker.handle(bytes, 0).await;
        assert!(matches!(outcome, HandlerOutcome::Reject(_)), "{outcome:?}");
    }
    assert!(unit.store.calls().is_empty());
    assert_eq!(unit.transport.call_count(), 0);
    assert!(
        unit.metrics.is_silent(),
        "the allowlist has no value for a malformed payload"
    );
    assert_eq!(unit.policy.call_count(), 0);
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_on_the_first_delivery_whatever_the_budget() {
    // inst-sh-return-malformed: it can never succeed on retry
    let unit = unit();
    let mut bad = payload(true);
    bad["base_frontier_message_id"] = Value::Null;
    let outcome = run(&unit, &bad, 0).await;
    let Some(reason) = reason(&outcome) else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("base_frontier"), "{reason}");
    assert!(!reason.contains(&TENANT_A.to_string()));
}

// ── the pre-check ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_deleted_or_missing_chat_is_acknowledged_without_a_provider_call() {
    // inst-sh-precheck, inst-sh-precheck-match, inst-sh-case-chat-gone: no execution result
    let unit = unit();
    unit.store.set_precheck(Ok(PreCheck::ChatGone));
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.store.calls(), vec!["precheck"]);
    assert_eq!(unit.transport.call_count(), 0);
    assert!(unit.metrics.is_silent());
    assert_eq!(
        unit.policy.call_count(),
        0,
        "the model is not even resolved"
    );
}

#[tokio::test]
async fn a_missing_base_is_recorded_and_acknowledged() {
    // inst-sh-case-base-missing: the next trigger builds a summary with no base
    let unit = unit();
    unit.store.set_precheck(Ok(PreCheck::BaseMissing));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.executions(), vec!["base_missing"]);
    assert_eq!(unit.metrics.cas_conflicts(), 0);
    assert_eq!(unit.metrics.fallbacks(), 0);
    assert_eq!(unit.store.calls(), vec!["precheck"]);
    assert_eq!(unit.transport.call_count(), 0);
}

#[tokio::test]
async fn a_moved_frontier_is_a_cas_conflict_without_a_provider_call() {
    // inst-sh-case-conflict, inst-mt-cas: another commit already advanced the frontier
    let unit = unit();
    unit.store.set_precheck(Ok(PreCheck::FrontierMoved));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.cas_conflicts(), 1);
    assert!(
        unit.metrics.executions().is_empty(),
        "a CAS conflict records no result"
    );
    assert_eq!(unit.transport.call_count(), 0);
    assert_eq!(unit.store.calls(), vec!["precheck"]);
}

#[tokio::test]
async fn a_deleted_target_is_recorded_and_acknowledged_before_the_call() {
    // inst-sh-case-target-deleted
    let unit = unit();
    unit.store.set_precheck(Ok(PreCheck::TargetDeleted));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.executions(), vec!["frontier_deleted"]);
    assert_eq!(unit.transport.call_count(), 0);
    assert_eq!(unit.store.calls(), vec!["precheck"]);
}

#[tokio::test]
async fn a_pre_check_database_error_retries_with_the_budget() {
    // inst-sh-case-precheck-db, inst-lc-retry, inst-lc-redeliver
    let unit = unit();
    unit.store.set_precheck(Err(()));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["retry"]);
    assert_eq!(
        unit.metrics.fallbacks(),
        0,
        "no fallback for an infrastructure error"
    );
    assert_eq!(unit.transport.call_count(), 0);
}

#[tokio::test]
async fn a_pre_check_database_error_is_rejected_on_the_last_delivery() {
    // inst-sh-case-precheck-db, inst-rb-last, inst-lc-rejected, inst-lc-dead-letter
    let unit = unit();
    unit.store.set_precheck(Err(()));
    assert_eq!(
        run(&unit, &payload(true), 2).await,
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
    assert_eq!(unit.metrics.executions(), vec!["retry"]);
}

#[tokio::test]
async fn a_ready_pre_check_goes_on_with_the_base_summary_text() {
    // inst-sh-case-ready, inst-sh-prompt: the stored text of the base reaches the prompt
    let unit = unit();
    unit.store.set_precheck(Ok(PreCheck::Ready {
        base_summary: Some("THE OLD SUMMARY".to_owned()),
    }));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    let body = unit.transport.calls().remove(0).body;
    let user = body["input"][0]["content"].as_str().unwrap().to_owned();
    assert!(
        user.contains("<existing_summary>") && user.contains("THE OLD SUMMARY"),
        "{user}"
    );
    assert_eq!(unit.store.calls(), vec!["precheck", "range", "commit"]);
}

#[tokio::test]
async fn an_empty_base_sends_no_existing_summary_block() {
    // inst-sh-case-ready, inst-pr-no-summary
    let unit = unit();
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    let body = unit.transport.calls().remove(0).body;
    assert!(!body.to_string().contains("existing_summary"));
}

// ── the range ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_range_database_error_retries_with_the_budget() {
    // inst-sh-range, inst-sh-range-db, inst-sh-return-range-db
    let unit = unit();
    unit.store.set_range(Err(()));
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["retry"]);
    assert_eq!(unit.transport.call_count(), 0);
    let unit = self::unit();
    unit.store.set_range(Err(()));
    assert_eq!(
        run(&unit, &payload(false), 2).await,
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
}

#[tokio::test]
async fn an_empty_range_is_acknowledged_without_a_provider_call() {
    // inst-sh-range-empty, inst-sh-return-range-empty: nothing to summarize, no result recorded
    let unit = unit();
    unit.store.set_range(Ok(Vec::new()));
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.transport.call_count(), 0);
    assert!(unit.metrics.is_silent());
    assert_eq!(unit.store.calls(), vec!["precheck", "range"]);
}

// ── the model resolution ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_summary_model_is_rejected_at_once() {
    // inst-sh-model, inst-sh-model-unavailable, inst-mr-unavailable, inst-mr-return-unavailable,
    // inst-sh-return-model-unavailable: dead letter, no retry, even on the first delivery
    let unit = unit_summary_with(settings_for("no-such-model"), catalog());
    let outcome = run(&unit, &payload(false), 0).await;
    assert!(matches!(outcome, HandlerOutcome::Reject(_)), "{outcome:?}");
    assert_eq!(unit.metrics.executions(), vec!["model_unavailable"]);
    assert_eq!(
        unit.metrics.fallbacks(),
        0,
        "no fallback for a missing model"
    );
    assert_eq!(unit.transport.call_count(), 0);
    assert!(!unit.store.calls().contains(&"commit"));
}

#[tokio::test]
async fn a_disabled_summary_model_is_rejected_at_once() {
    // inst-mr-lookup: the enabled filter
    let mut disabled =
        serde_json::to_value(summary_entry(SUMMARY_MODEL, "azure_openai", &json!({}))).unwrap();
    disabled["enabled"] = json!(false);
    let snapshot = catalog_with(serde_json::from_value(disabled).unwrap());
    let unit = unit_summary_with(settings_for(SUMMARY_MODEL), snapshot);
    let outcome = run(&unit, &payload(false), 0).await;
    assert!(matches!(outcome, HandlerOutcome::Reject(_)), "{outcome:?}");
    assert_eq!(unit.metrics.executions(), vec!["model_unavailable"]);
    assert_eq!(unit.transport.call_count(), 0);
}

#[tokio::test]
async fn the_reject_reason_of_a_missing_model_holds_no_content() {
    // inst-hw-outcomes (via the handler): a reason carries no content or provider identifier
    let unit = unit_summary_with(settings_for("no-such-model"), catalog());
    let outcome = run(&unit, &payload(false), 0).await;
    let reason = reason(&outcome).expect("rejected");
    assert!(!reason.is_empty());
    for leaked in ["example.com", "provider-", "m1", "<summary>"] {
        assert!(!reason.contains(leaked), "{reason}");
    }
}

#[tokio::test]
async fn an_empty_model_id_resolves_gpt_4_1_mini() {
    // inst-mr-model-id
    let snapshot = catalog_with(summary_entry("gpt-4.1-mini", "azure_openai", &json!({})));
    let unit = unit_summary_with(
        SummaryWorkerSettings {
            summary_model_id: String::new(),
            ..settings_for("")
        },
        snapshot,
    );
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    assert_eq!(
        unit.transport.calls()[0].body["model"],
        "provider-gpt-4.1-mini"
    );
    let missing = unit_summary_with(
        SummaryWorkerSettings {
            summary_model_id: String::new(),
            ..settings_for("")
        },
        catalog(),
    );
    assert!(matches!(
        run(&missing, &payload(false), 0).await,
        HandlerOutcome::Reject(_)
    ));
    assert_eq!(missing.metrics.executions(), vec!["model_unavailable"]);
}

#[tokio::test]
async fn the_snapshot_is_read_once_per_delivery_without_a_user_id() {
    // inst-mr-snapshot, inst-mr-no-quota: no preflight, no kill-switch check, no tier selection
    let unit = unit();
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.policy.users(), vec![Uuid::nil()]);
}

#[tokio::test]
async fn an_unreadable_snapshot_retries_with_the_budget() {
    // inst-mr-snapshot-failed, inst-mr-return-snapshot-failed, inst-sh-resolution-error,
    // inst-sh-return-resolution-error: `retry`, no fallback increment
    let unit = unit();
    unit.policy.set_mode(PolicyMode::Fail);
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["retry"]);
    assert_eq!(unit.metrics.fallbacks(), 0);
    assert_eq!(unit.transport.call_count(), 0);
    assert_eq!(
        run(&unit, &payload(false), 2).await,
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
}

#[tokio::test]
async fn an_unresolvable_provider_retries_with_the_budget() {
    // inst-mr-provider, inst-mr-provider-failed, inst-mr-return-provider-failed
    let snapshot = catalog_with(summary_entry("ghost-sum", "ghost", &json!({})));
    let unit = unit_summary_with(settings_for("ghost-sum"), snapshot);
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["retry"]);
    assert_eq!(unit.metrics.fallbacks(), 0);
    assert_eq!(unit.transport.call_count(), 0);
}

#[tokio::test]
async fn the_model_of_the_entry_decides_the_provider_and_the_request() {
    // inst-mr-return, inst-sh-model, inst-sh-call: the catalog entry and its provider
    let unit = unit_summary_with(settings_for(SUMMARY_CC), catalog());
    unit.transport
        .queue(Scripted::ok(&chat_answer(&summary_text("S"), (5, 5))));
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    let call = unit.transport.calls().remove(0);
    assert_eq!(call.uri, format!("/{CC_ALIAS}/v1/chat/completions"));
    assert_eq!(call.body["model"], format!("provider-{SUMMARY_CC}"));
    assert!(call.body["messages"].is_array());
}

#[tokio::test]
async fn a_model_missing_from_the_catalog_is_not_the_same_as_an_unreadable_snapshot() {
    // inst-mr-unavailable versus inst-mr-snapshot-failed: reject versus retry
    let missing = unit_summary_with(settings_for(SUMMARY_MODEL), catalog_without(SUMMARY_MODEL));
    assert!(matches!(
        run(&missing, &payload(false), 0).await,
        HandlerOutcome::Reject(_)
    ));
    let unreadable = unit();
    unreadable.policy.set_mode(PolicyMode::Fail);
    assert_eq!(
        run(&unreadable, &payload(false), 0).await,
        HandlerOutcome::Retry
    );
}

// ── the provider call ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_failure_keeps_the_previous_summary_and_retries() {
    // inst-sh-call, inst-sh-call-failed, inst-sh-return-call-failed, inst-mt-fallback,
    // inst-fr-unchanged: nothing is committed
    let unit = unit();
    unit.transport.queue(Scripted::status(
        500,
        &json!({ "error": { "message": "boom" } }),
    ));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["provider_error"]);
    assert_eq!(unit.metrics.fallbacks(), 1);
    assert!(
        !unit.store.calls().contains(&"commit"),
        "no commit, no frontier, no compression"
    );
    assert!(unit.usage.events().is_empty());
}

#[tokio::test]
async fn the_third_failed_delivery_is_dead_lettered() {
    // inst-rb-last, inst-lc-rejected, inst-lc-dead-letter: 0-based attempts 0, 1, 2
    let unit = unit();
    unit.transport.queue(Scripted::status(500, &json!({})));
    unit.transport.queue(Scripted::status(500, &json!({})));
    unit.transport.queue(Scripted::status(500, &json!({})));
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Retry);
    assert_eq!(run(&unit, &payload(true), 1).await, HandlerOutcome::Retry);
    assert_eq!(
        run(&unit, &payload(true), 2).await,
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
    assert_eq!(unit.metrics.executions(), vec!["provider_error"; 3]);
    assert_eq!(
        unit.metrics.fallbacks(),
        3,
        "every failed call is a fallback"
    );
    assert!(!unit.store.calls().contains(&"commit"));
}

#[tokio::test]
async fn the_budget_is_the_configured_max_attempts() {
    // inst-rb-last with `thread_summary_worker.max_attempts` 5
    let settings = SummaryWorkerSettings {
        max_attempts: 5,
        ..settings_for(SUMMARY_MODEL)
    };
    let unit = unit_summary(settings);
    unit.transport.queue(Scripted::status(500, &json!({})));
    assert_eq!(run(&unit, &payload(true), 3).await, HandlerOutcome::Retry);
    unit.transport.queue(Scripted::status(500, &json!({})));
    assert_eq!(
        run(&unit, &payload(true), 4).await,
        HandlerOutcome::Reject(max_attempts_reason(5))
    );
}

#[tokio::test]
async fn a_gateway_or_transport_failure_is_a_provider_error() {
    // inst-sh-call-failed
    for failure in [TransportFailure::Gateway, TransportFailure::Transport] {
        let unit = unit();
        unit.transport.queue(Scripted::Fail(failure));
        assert_eq!(
            run(&unit, &payload(true), 0).await,
            HandlerOutcome::Retry,
            "{failure:?}"
        );
        assert_eq!(unit.metrics.executions(), vec!["provider_error"]);
        assert_eq!(unit.metrics.fallbacks(), 1);
    }
}

#[tokio::test]
async fn a_hung_provider_ends_as_a_provider_error_before_the_lease() {
    // inst-sh-call-failed: the provider phase is bounded by `claim_timeout_secs` minus 10 s
    let settings = SummaryWorkerSettings {
        claim_timeout: Duration::from_secs(30),
        ..settings_for(SUMMARY_MODEL)
    };
    let unit = unit_summary(settings);
    unit.transport.queue(Scripted::Hang);
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["provider_error"]);
    assert_eq!(unit.metrics.fallbacks(), 1);
    assert_eq!(unit.time.elapsed(), Duration::from_secs(20));
}

#[tokio::test]
async fn a_context_length_rejection_after_the_retries_is_a_provider_error() {
    // inst-sh-call-failed: "a context-length rejection after the bounded prompt-too-long retries"
    let unit = unit();
    unit.store.set_range(Ok((1..=10)
        .map(|i| range_msg("user", &format!("m{i}"), i * 10))
        .collect()));
    for _ in 0..3 {
        unit.transport
            .queue(Scripted::status(400, &context_length_error()));
    }
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.transport.call_count(), 3);
    assert_eq!(unit.metrics.executions(), vec!["provider_error"]);
    assert_eq!(unit.metrics.fallbacks(), 1);
}

#[tokio::test]
async fn a_context_length_rejection_that_a_retry_resolves_commits() {
    // inst-sh-call: the smaller prompt succeeds
    let unit = unit();
    unit.store.set_range(Ok((1..=10)
        .map(|i| range_msg("user", &format!("m{i}"), i * 10))
        .collect()));
    unit.transport
        .queue(Scripted::status(413, &context_length_error()));
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.transport.call_count(), 2);
    assert_eq!(unit.metrics.executions(), vec!["success"]);
    assert_eq!(unit.metrics.fallbacks(), 0);
}

#[tokio::test]
async fn a_malformed_provider_body_is_a_provider_error() {
    // inst-sh-call-failed
    let unit = unit();
    unit.transport.queue(Scripted::Answer {
        status: 200,
        body: b"garbage".to_vec(),
    });
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["provider_error"]);
    assert_eq!(unit.metrics.fallbacks(), 1);
}

#[tokio::test]
async fn an_empty_summary_retries_without_a_fallback() {
    // inst-sh-parse-response, inst-sh-empty, inst-sh-return-empty
    for text in [
        "",
        "   \n",
        "<analysis>only</analysis>",
        "<summary>cut off",
        "<summary> </summary>",
    ] {
        let unit = unit();
        unit.transport
            .queue(Scripted::ok(&responses_answer(text, (10, 5))));
        assert_eq!(
            run(&unit, &payload(true), 0).await,
            HandlerOutcome::Retry,
            "`{text}`"
        );
        assert_eq!(unit.metrics.executions(), vec!["empty_summary"], "`{text}`");
        assert_eq!(
            unit.metrics.fallbacks(),
            0,
            "no fallback for an empty summary"
        );
        assert!(!unit.store.calls().contains(&"commit"));
    }
}

#[tokio::test]
async fn an_empty_summary_is_rejected_on_the_last_delivery() {
    // inst-sh-return-empty, inst-rb-last
    let unit = unit();
    unit.transport
        .queue(Scripted::ok(&responses_answer("", (1, 0))));
    assert_eq!(
        run(&unit, &payload(true), 2).await,
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
    assert_eq!(unit.metrics.executions(), vec!["empty_summary"]);
}

// ── the commit ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_committed_summary_is_success() {
    // inst-sh-commit, inst-sh-commit-match, inst-sh-case-committed, inst-sh-return
    let unit = unit();
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.executions(), vec!["success"]);
    assert_eq!(unit.metrics.fallbacks(), 0);
    assert_eq!(unit.metrics.cas_conflicts(), 0);
    assert_eq!(unit.store.calls(), vec!["precheck", "range", "commit"]);
    assert_eq!(unit.transport.call_count(), 1);
}

#[tokio::test]
async fn the_commit_carries_the_parsed_text_the_estimate_and_the_frozen_range() {
    // inst-sh-commit, inst-rp-take-summary: only the `<summary>` content is stored
    let unit = unit();
    unit.transport.queue(Scripted::ok(&responses_answer(
        &summary_text("Stored text."),
        (200, 60),
    )));
    run(&unit, &payload(true), 0).await;
    let commit = unit.store.commits().remove(0);
    assert_eq!(commit.summary_text, "Stored text.");
    assert_eq!(commit.token_estimate, 60);
    assert_eq!(commit.work.tenant_id, TENANT_A);
    assert_eq!(commit.work.chat_id, CHAT);
    assert_eq!(commit.work.system_request_id, REQUEST);
    assert_eq!(commit.work.base_frontier.unwrap().message_id, BASE_ID);
    assert_eq!(commit.work.frozen_target.message_id, TARGET_ID);
    assert_eq!(commit.usage_event.usage.unwrap().input_tokens, 200);
}

#[tokio::test]
async fn a_deleted_target_at_the_commit_is_acknowledged() {
    // inst-sh-case-commit-target-deleted: no summary, no compression, no usage event
    let unit = unit();
    unit.store.set_commit(CommitKind::TargetDeleted);
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.executions(), vec!["frontier_deleted"]);
    assert_eq!(unit.metrics.fallbacks(), 0);
    assert_eq!(unit.store.commits().len(), 1, "no second commit attempt");
}

#[tokio::test]
async fn a_missing_base_at_the_commit_is_acknowledged() {
    // inst-sh-case-commit-base-missing
    let unit = unit();
    unit.store.set_commit(CommitKind::BaseMissing);
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.executions(), vec!["base_missing"]);
    assert_eq!(unit.store.commits().len(), 1);
}

#[tokio::test]
async fn a_lost_cas_at_the_commit_is_a_conflict_without_another_attempt() {
    // inst-sh-case-commit-conflict, inst-mt-cas, inst-cm-single-commit
    let unit = unit();
    unit.store.set_commit(CommitKind::FrontierMoved);
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.cas_conflicts(), 1);
    assert!(
        unit.metrics.executions().is_empty(),
        "a CAS conflict records no result"
    );
    assert_eq!(unit.metrics.fallbacks(), 0);
    assert_eq!(
        unit.store.commits().len(),
        1,
        "exactly one commit attempt for this range"
    );
}

#[tokio::test]
async fn a_commit_error_retries_with_the_budget_and_is_no_fallback() {
    // inst-sh-case-commit-db, inst-mt-fallback: a failed commit is `retry`, not `provider_error`
    let unit = unit();
    unit.store.set_commit(CommitKind::DbError);
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.metrics.executions(), vec!["retry"]);
    assert_eq!(unit.metrics.fallbacks(), 0);
    let last = self::unit();
    last.store.set_commit(CommitKind::DbError);
    assert_eq!(
        run(&last, &payload(true), 2).await,
        HandlerOutcome::Reject(max_attempts_reason(3))
    );
}

#[tokio::test]
async fn a_redelivery_calls_the_provider_again_and_commits_through_the_cas() {
    // inst-sh-duplicates-accepted: duplicate provider calls are a P1 side effect; correctness
    // rests on the CAS commit, never on a memo of the worker
    let unit = unit();
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(run(&unit, &payload(true), 1).await, HandlerOutcome::Ok);
    assert_eq!(unit.transport.call_count(), 2);
    assert_eq!(unit.store.commits().len(), 2);
}

#[tokio::test]
async fn every_decided_delivery_records_exactly_one_execution_result() {
    // inst-mt-execution: success, provider_error, empty_summary, retry, model_unavailable,
    // frontier_deleted and base_missing, once each per decided delivery
    let unit = unit();
    for _ in 0..2 {
        unit.transport.queue(default_answer());
    }
    run(&unit, &payload(true), 0).await;
    run(&unit, &payload(true), 1).await;
    assert_eq!(unit.metrics.executions(), vec!["success", "success"]);
}

#[tokio::test]
async fn the_metric_values_are_the_allowlist_only() {
    // inst-mt-labels: bounded values, never an identifier or free-form error text
    let allowed = [
        "success",
        "provider_error",
        "empty_summary",
        "retry",
        "model_unavailable",
        "frontier_deleted",
        "base_missing",
    ];
    let unit = unit();
    unit.transport.queue(Scripted::status(
        500,
        &json!({ "error": { "message": "free text" } }),
    ));
    run(&unit, &payload(true), 0).await;
    unit.store.set_precheck(Err(()));
    run(&unit, &payload(true), 0).await;
    for result in unit.metrics.executions() {
        assert!(allowed.contains(&result.as_str()), "{result}");
    }
}

// ── the system-task usage event ──────────────────────────────────────────────

async fn committed_event(unit: &UnitSummary, attempts: i16) -> mini_chat_sdk::UsageEvent {
    run(unit, &payload(true), attempts).await;
    unit.store
        .commits()
        .last()
        .expect("a commit")
        .usage_event
        .clone()
}

#[tokio::test]
async fn the_usage_event_is_a_zero_credit_system_task() {
    // inst-su-build: `requester_type` system, `billing_outcome` system_task, `settlement_method`
    // none, 0 credits, `policy_version_applied` 0, no tool calls, `completed`
    let unit = unit();
    unit.transport.queue(Scripted::ok(&responses_answer(
        &summary_text("S"),
        (300, 70),
    )));
    let event = committed_event(&unit, 0).await;
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.chat_id, CHAT);
    assert_eq!(event.request_id, REQUEST);
    assert_eq!(event.effective_model, SUMMARY_MODEL);
    assert_eq!(event.selected_model, SUMMARY_MODEL);
    assert_eq!(event.terminal_state, "completed");
    assert_eq!(event.billing_outcome, "system_task");
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.settlement_method, "none");
    assert_eq!(event.policy_version_applied, 0);
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (0, 0, 0)
    );
    assert_eq!(event.requester_type, "system");
    assert_eq!(
        event.system_task_type.as_deref(),
        Some("thread_summary_update")
    );
    let usage = event.usage.expect("the token counts of the call");
    assert_eq!((usage.input_tokens, usage.output_tokens), (300, 70));
}

#[tokio::test]
async fn the_usage_event_has_no_user_and_no_turn_not_even_as_null() {
    // inst-su-build, inst-su-no-turn: absent, not null
    let unit = unit();
    let event = committed_event(&unit, 0).await;
    assert_eq!(event.user_id, None);
    assert_eq!(event.turn_id, None);
    let json = serde_json::to_value(&event).unwrap();
    assert!(
        json.get("user_id").is_none() && json.get("turn_id").is_none(),
        "{json}"
    );
}

#[tokio::test]
async fn the_dedupe_key_is_tenant_task_type_and_system_request_in_simple_hex() {
    // inst-su-dedupe
    let unit = unit();
    let event = committed_event(&unit, 0).await;
    assert_eq!(
        event.dedupe_key.as_deref(),
        Some(
            format!(
                "{}/thread_summary_update/{}",
                TENANT_A.simple(),
                REQUEST.simple()
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn the_system_request_id_is_reused_on_every_retry_and_replay() {
    // inst-su-stable-id: the handler never generates a new one
    let unit = unit();
    let first = committed_event(&unit, 0).await;
    let second = committed_event(&unit, 1).await;
    let replay = committed_event(&unit, 7).await;
    assert_eq!(first.request_id, REQUEST);
    assert_eq!(second.request_id, REQUEST);
    assert_eq!(replay.request_id, REQUEST);
    assert_eq!(first.dedupe_key, second.dedupe_key);
    assert_eq!(first.dedupe_key, replay.dedupe_key);
}

#[tokio::test]
async fn the_usage_event_carries_the_application_clock_instant() {
    // inst-su-build: `timestamp` now in UTC
    let unit = unit();
    unit.transport
        .queue(Scripted::takes(Duration::from_secs(3), default_answer()));
    let event = committed_event(&unit, 0).await;
    let expected: OffsetDateTime = default_now() + time::Duration::seconds(3);
    assert_eq!(event.timestamp, expected);
    assert_eq!(unit.store.commits()[0].now, expected);
}

#[tokio::test]
async fn the_usage_event_holds_no_provider_identifier_and_no_content() {
    // inst-su-no-audit: no provider identifier in the payload
    let unit = unit();
    let event = committed_event(&unit, 0).await;
    let text = serde_json::to_string(&event).unwrap();
    for leaked in [
        "provider-summarizer",
        "example.com",
        "the summary",
        "m1",
        "/v1/",
    ] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
}

#[tokio::test]
async fn the_commit_gets_the_usage_event_only_not_a_quota_or_audit_call() {
    // inst-su-no-audit, inst-mr-no-quota: one policy read, one provider call, one commit
    let unit = unit();
    assert_eq!(run(&unit, &payload(true), 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.policy.call_count(), 1);
    assert_eq!(unit.transport.call_count(), 1);
    assert_eq!(unit.store.commits().len(), 1);
}

// ── logs ─────────────────────────────────────────────────────────────────────

#[traced_test]
#[tokio::test]
async fn the_logs_carry_the_chat_and_request_ids_but_never_content() {
    // inst-sh-no-content-logs: neither the message content, the prompt nor the summary text
    let unit = unit();
    unit.store.set_range(Ok(vec![
        range_msg("user", "PRIVATE-WORDS-USER", 10),
        range_msg("assistant", "PRIVATE-WORDS-ASSISTANT", 20),
    ]));
    unit.transport.queue(Scripted::ok(&responses_answer(
        "<analysis>PRIVATE-ANALYSIS</analysis><summary>PRIVATE-SUMMARY-TEXT</summary>",
        (10, 5),
    )));
    unit.transport.queue(Scripted::status(
        500,
        &json!({ "error": { "message": "PROVIDER-SECRET-TEXT" } }),
    ));
    run(&unit, &payload(true), 0).await;
    unit.store.set_commit(CommitKind::DbError);
    run(&unit, &payload(true), 1).await;
    run(&unit, &payload(true), 2).await;
    assert!(logs_contain(&CHAT.to_string()), "the chat id is logged");
    assert!(
        logs_contain(&REQUEST.to_string()),
        "the system request id is logged"
    );
    for secret in [
        "PRIVATE-WORDS",
        "PRIVATE-ANALYSIS",
        "PRIVATE-SUMMARY-TEXT",
        "PROVIDER-SECRET-TEXT",
        "Summarize the following conversation",
        "You are a conversation summarizer",
    ] {
        assert!(!logs_contain(secret), "{secret} was logged");
    }
}

#[traced_test]
#[tokio::test]
async fn the_last_failed_delivery_logs_a_warning_with_the_ids_and_the_last_result() {
    // inst-rb-last: chat id, system request id and the last result
    let unit = unit();
    unit.transport.queue(Scripted::status(500, &json!({})));
    run(&unit, &payload(true), 2).await;
    assert!(logs_contain(&CHAT.to_string()));
    assert!(logs_contain(&REQUEST.to_string()));
    assert!(logs_contain("provider_error"), "the last result is logged");
}

#[traced_test]
#[tokio::test]
async fn a_missing_summary_model_is_logged_as_an_error_naming_the_model_id() {
    // inst-sh-return-model-unavailable
    let unit = unit_summary_with(settings_for("the-missing-model-id"), catalog());
    run(&unit, &payload(false), 0).await;
    assert!(logs_contain("the-missing-model-id"));
}

#[traced_test]
#[tokio::test]
async fn a_malformed_payload_is_logged_with_its_reason_but_not_its_values() {
    // inst-sh-malformed: "Log an error with the reason"
    let unit = unit();
    let mut bad = payload(true);
    bad["chat_id"] = json!("SECRET-CHAT-VALUE");
    run(&unit, &bad, 0).await;
    assert!(logs_contain("chat_id"));
    assert!(!logs_contain("SECRET-CHAT-VALUE"));
}

#[tokio::test]
async fn the_range_messages_of_a_fitted_prompt_never_change_the_range_that_is_committed() {
    // inst-pr-range-unchanged: the commit names the work item (the frozen range), not the kept
    // messages
    let unit = unit();
    let many: Vec<RangeMessage> = (1..=30)
        .map(|i| range_msg("user", &"x".repeat(2000), i * 10))
        .collect();
    unit.store.set_range(Ok(many));
    assert_eq!(run(&unit, &payload(false), 0).await, HandlerOutcome::Ok);
    let commit = unit.store.commits().remove(0);
    assert_eq!(commit.work.frozen_target.message_id, TARGET_ID);
    assert_eq!(label(&HandlerOutcome::Ok), "ok");
}
