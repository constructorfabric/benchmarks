//! Reliable usage publication: one usage message per finalized turn, enqueued in the finalization
//! transaction, delivered to the model-policy plugin by the usage outbox handler.
//!
//! Source of truth: DESIGN 5.6 "Reliable Usage Event Publication (Outbox Pattern)" (queue,
//! `UsageEvent` payload, `dedupe_key`, UUID normalization, handler outcomes `Ok` / `Retry` /
//! `Reject`), 5.7 (settlement transaction invariant, "Usage accounting rules per outcome", "Dedupe
//! Key Requirement"), 5.8 / 5.9 ("Outbox Emission Requirement"), 5.5.12 (replay is side-effect
//! free), Appendix A.3 (`PublishUsage`), the SDK contract `UsageEvent`.

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

mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::sync::Arc;

use mini_chat::infra::db::timestamp::to_db;
use mini_chat::infra::model_policy::{ModelPolicyGateway, UsageDeliveryOutcome};
use mini_chat::infra::outbox::UsageOutboxHandler;
use mini_chat_sdk::{
    BillingOutcome, PublishError, RequesterType, SettlementMethod, TerminalState, UsageEvent,
};
use quota_support::{
    LimitsSpec, Opts, PREMIUM, PREMIUM_IN, PREMIUM_OUT, ProviderReply, QuotaHarness, QuotaOptions,
    QuotaPolicy, STANDARD, STANDARD_IN, STANDARD_OUT, TENANT_A, USER_A, answer_with_usage, ctx_a,
    dedupe_key, failed_stream, failed_stream_with_details, hex, quota_models, ref_credits,
    stream_model_json,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_db::outbox::LeasedMessageHandler;
use toolkit_db::outbox::{MessageResult, OutboxMessage};
use uuid::Uuid;

const MSG: &str = "hello";

fn roomy() -> LimitsSpec {
    LimitsSpec::new(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000)
}

fn premium_credits(input: u64, output: u64) -> i64 {
    ref_credits(input, output, PREMIUM_IN, PREMIUM_OUT)
}

fn standard_credits(input: u64, output: u64) -> i64 {
    ref_credits(input, output, STANDARD_IN, STANDARD_OUT)
}

// ═════════════════════════════════════════════════════════════════════════════
// the event of each terminal outcome
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_turn_publishes_one_usage_event_with_the_actual_usage() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(800, 200));
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(1);

    let sent = h.say(chat, MSG).await;
    let events = h.policy.wait_published(1).await;
    let after = OffsetDateTime::now_utc() + time::Duration::seconds(1);

    assert_eq!(events.len(), 1);
    let event = &events[0];
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.user_id, Some(USER_A));
    assert_eq!(event.chat_id, chat);
    assert_eq!(event.turn_id, Some(turn.id));
    assert_eq!(event.request_id, sent.request_id());
    assert_eq!(event.effective_model, PREMIUM);
    assert_eq!(event.selected_model, PREMIUM);
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.billing_outcome, BillingOutcome::Completed);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, premium_credits(800, 200));
    assert_eq!(event.policy_version_applied, 1);
    let usage = event.usage.as_ref().expect("provider usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (800, 200));
    assert_eq!(
        (
            usage.cache_read_input_tokens,
            usage.cache_write_input_tokens,
            usage.reasoning_tokens
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (0, 0, 0)
    );
    assert_eq!(event.requester_type, RequesterType::User);
    assert_eq!(event.system_task_type, None);
    assert!(
        event.timestamp >= before && event.timestamp <= after,
        "timestamp {} outside [{before}, {after}]",
        event.timestamp
    );
}

#[tokio::test]
async fn the_dedupe_key_is_tenant_turn_request_in_simple_hex() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    let turn = h.turn_of(sent.request_id()).await.unwrap();

    let expected = dedupe_key(TENANT_A, turn.id, sent.request_id());
    assert_eq!(event.dedupe_key, expected);
    let parts: Vec<&str> = event.dedupe_key.split('/').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], hex(TENANT_A));
    for part in parts {
        assert_eq!(part.len(), 32, "{part}");
        assert!(
            part.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "lowercase hex without hyphens: {part}"
        );
    }
    assert_eq!(event.dedupe_key.len(), 98);
}

#[tokio::test]
async fn the_payload_is_the_sdk_usage_event_json() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(10, 5));
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);

    let wire: Value = serde_json::to_value(&event).unwrap();
    for key in [
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
    ] {
        assert!(wire.get(key).is_some(), "missing `{key}`: {wire}");
    }
    assert!(
        wire.get("system_task_type").is_none(),
        "user turns omit the task type"
    );
    assert!(
        wire.get("reserved_credits_micro").is_none()
            && wire.get("quota_decision").is_none()
            && wire.get("downgrade_reason").is_none()
            && wire.get("event_type").is_none(),
        "DESIGN 5.6: no reserve, no quota decision, no event_type in the payload: {wire}"
    );
    assert_eq!(wire["terminal_state"], json!("completed"));
    assert_eq!(wire["billing_outcome"], json!("completed"));
    assert_eq!(wire["settlement_method"], json!("actual"));
    assert_eq!(wire["requester_type"], json!("user"));
    let roundtrip: UsageEvent = serde_json::from_value(wire).unwrap();
    assert_eq!(roundtrip, event);
}

#[tokio::test]
async fn a_downgraded_turn_reports_both_models_and_the_effective_models_credits() {
    let h = QuotaHarness::with_limits(roomy()).await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(1_000, 100));
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.selected_model, PREMIUM);
    assert_eq!(event.effective_model, STANDARD);
    assert_eq!(event.actual_credits_micro, standard_credits(1_000, 100));
}

#[tokio::test]
async fn the_event_carries_the_policy_version_the_turn_was_planned_under() {
    let h = QuotaHarness::with(QuotaOptions {
        policy_version: 9,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.policy_version_applied, 9);
}

#[tokio::test]
async fn an_overshoot_beyond_the_tolerance_publishes_the_committed_credits_and_the_actual_tokens() {
    // reserve = 113 + 100 tokens, 1_230 credits; the provider reports 300 input tokens
    let mut standard = stream_model_json(STANDARD, "Standard");
    standard["input_tokens_credit_multiplier_micro"] = json!(10_000_000);
    standard["output_tokens_credit_multiplier_micro"] = json!(1_000_000);
    standard["max_output_tokens"] = json!(100);
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    let h = QuotaHarness::with(QuotaOptions {
        models: vec![standard],
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(300, 0));
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.billing_outcome, BillingOutcome::Completed);
    assert_eq!(
        event.actual_credits_micro, 1_230,
        "committed (capped) credits"
    );
    assert_eq!(
        event.usage.as_ref().unwrap().input_tokens,
        300,
        "actual tokens for telemetry"
    );
}

#[tokio::test]
async fn a_failed_turn_with_provider_usage_is_failed_actual() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "boom",
        &["p"],
        Some((700, 40)),
    )));
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.billing_outcome, BillingOutcome::Failed);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, premium_credits(700, 40));
    let usage = event.usage.as_ref().expect("reported usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (700, 40));
}

#[tokio::test]
async fn a_completed_turn_carries_the_cache_and_reasoning_breakdown_in_the_usage_event_and_the_row()
{
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["Hello", " there"],
        &Opts::default().usage(500, 40).details(300, 7),
    ));

    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);

    let usage = event.usage.as_ref().expect("provider usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (500, 40));
    assert_eq!(
        (
            usage.cache_read_input_tokens,
            usage.cache_write_input_tokens,
            usage.reasoning_tokens
        ),
        (300, 0, 7),
        "cache_write stays 0 for the OpenAI Responses adapter"
    );
    assert_eq!(
        event.actual_credits_micro,
        premium_credits(500, 40),
        "credits are computed from the totals only (the breakdown is a subset)"
    );
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["usage"]["cache_read_input_tokens"], json!(300));
    assert_eq!(wire["usage"]["reasoning_tokens"], json!(7));

    let assistant = h
        .chat
        .all_messages()
        .await
        .into_iter()
        .find(|m| m.chat_id == chat && m.role == "assistant")
        .expect("assistant message");
    assert_eq!((assistant.input_tokens, assistant.output_tokens), (500, 40));
    assert_eq!(
        (
            assistant.cache_read_input_tokens,
            assistant.cache_write_input_tokens,
            assistant.reasoning_tokens
        ),
        (300, 0, 7)
    );
}

#[tokio::test]
async fn an_incomplete_turn_carries_the_breakdown() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["Hel"],
        &Opts::default().usage(90, 20).details(64, 5).incomplete(),
    ));

    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    let usage = event.usage.as_ref().expect("provider usage");
    assert_eq!(
        (usage.cache_read_input_tokens, usage.reasoning_tokens),
        (64, 5)
    );
}

#[tokio::test]
async fn a_failed_turn_with_provider_usage_carries_the_breakdown() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream_with_details(
        "server_error",
        "boom",
        &["p"],
        Some((700, 40)),
        (256, 9),
    )));
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    let usage = event.usage.as_ref().expect("reported usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (700, 40));
    assert_eq!(
        (
            usage.cache_read_input_tokens,
            usage.cache_write_input_tokens,
            usage.reasoning_tokens
        ),
        (256, 0, 9)
    );
}

#[tokio::test]
async fn a_failed_turn_without_usage_is_failed_estimated_with_null_usage() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    h.say(chat, MSG).await;
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.billing_outcome, BillingOutcome::Failed);
    assert_eq!(
        event.settlement_method,
        SettlementMethod::Estimated,
        "the provider request started: never released"
    );
    assert_eq!(event.usage, None, "the estimated split is not emitted");
    assert_eq!(event.actual_credits_micro, premium_credits(113, 50));
    let wire = serde_json::to_value(&event).unwrap();
    assert!(wire["usage"].is_null(), "serialised as null: {wire}");
}

#[tokio::test]
async fn a_cancelled_turn_is_aborted_estimated() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG, "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(rid, "cancelled").await;

    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.request_id, rid);
    assert_eq!(event.terminal_state, TerminalState::Cancelled);
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.usage, None);
    assert_eq!(event.actual_credits_micro, premium_credits(113, 50));
}

// ═════════════════════════════════════════════════════════════════════════════
// exactly once per turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn every_finalized_turn_publishes_exactly_one_event_with_its_own_key() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    let mut request_ids = Vec::new();
    for i in 0..5 {
        if i == 2 {
            h.oagw.push(ProviderReply::http_error(500, "boom"));
        }
        let sent = h.say(chat, &format!("turn {i}")).await;
        request_ids.push(sent.request_id());
    }
    let events = h.policy.wait_published(5).await;
    h.policy.assert_published_stays(5).await;

    let mut keys: Vec<&str> = events.iter().map(|e| e.dedupe_key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 5, "distinct dedupe keys");
    for rid in request_ids {
        assert_eq!(
            events.iter().filter(|e| e.request_id == rid).count(),
            1,
            "one event for {rid}"
        );
    }
}

#[tokio::test]
async fn nothing_is_published_for_rejections_replays_and_lost_finalizations() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    let chat = h.seed_chat_on(STANDARD).await;
    let rid = Uuid::new_v4();
    h.say_with(chat, MSG, rid).await;
    h.policy.wait_published(1).await;

    // replay
    h.say_with(chat, MSG, rid).await;
    // quota rejection
    h.seed_usage(quota_support::UsageSeed::total("daily").spent(1_000_000))
        .await;
    let resp = h.reject(chat, json!({"content": "again"})).await;
    assert_eq!(resp.status.as_u16(), 429);
    // validation rejection
    let resp = h.reject(chat, json!({"content": " "})).await;
    assert_eq!(resp.status.as_u16(), 400);

    h.policy.assert_published_stays(1).await;
}

#[tokio::test]
async fn the_event_is_enqueued_in_the_finalization_transaction() {
    // the usage queue is not registered: the enqueue fails, so the whole finalization
    // transaction (CAS, assistant message, settlement) must roll back - quota is never debited
    // without its usage message (DESIGN 5.7 "Settlement transaction invariant")
    let h = QuotaHarness::with(QuotaOptions {
        limits: roomy(),
        register_usage_queue: false,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(800, 200));
    let sent = h.say(chat, MSG).await;

    assert_eq!(sent.names_no_ping().last().unwrap(), "error");
    assert_eq!(sent.error()["code"], json!("finalization_failed"));
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.state, "running", "not finalized");
    let total = h.bucket("total", "daily").await.unwrap();
    assert_eq!(
        (total.spent_credits_micro, total.calls),
        (0, 0),
        "no debit without the usage message"
    );
    assert!(
        h.chat
            .all_messages()
            .await
            .iter()
            .all(|m| m.role != "assistant")
    );
}

#[tokio::test]
async fn a_failing_audit_enqueue_rolls_the_finalization_back_too() {
    let h = QuotaHarness::with(QuotaOptions {
        limits: roomy(),
        register_audit_queue: false,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("finalization_failed"));
    assert_eq!(h.turn_of(sent.request_id()).await.unwrap().state, "running");
    assert_eq!(
        h.bucket("total", "daily")
            .await
            .unwrap()
            .spent_credits_micro,
        0
    );
    h.policy.assert_published_stays(0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// delivery to the model-policy plugin (outbox handler outcomes)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_transient_publish_failure_is_retried_until_the_plugin_accepts() {
    let h = QuotaHarness::new().await;
    h.policy.script_publish(vec![
        Err(PublishError::Transient("billing is down".to_owned())),
        Err(PublishError::Transient("still down".to_owned())),
    ]);
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;

    let events = h.policy.wait_published(1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].request_id, sent.request_id());
    assert!(
        h.policy
            .publish_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 3,
        "two failed attempts and the accepted one"
    );
    h.policy.assert_published_stays(1).await;
}

#[tokio::test]
async fn a_permanent_publish_failure_is_dead_lettered_and_does_not_block_the_next_event() {
    let h = QuotaHarness::new().await;
    h.policy
        .script_publish(vec![Err(PublishError::Permanent("malformed".to_owned()))]);
    let chat = h.seed_chat().await;
    h.say(chat, "first").await;
    // wait until the permanent failure was consumed
    quota_support::eventually("the first publish attempt", || async {
        h.policy
            .publish_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1
    })
    .await;
    let second = h.say(chat, "second").await;

    let events = h.policy.wait_published(1).await;
    assert_eq!(
        events[0].request_id,
        second.request_id(),
        "only the second event is accepted"
    );
    h.policy.assert_published_stays(1).await;
    assert_eq!(
        h.policy
            .publish_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the rejected message is not retried"
    );
}

// ── the handler in isolation ─────────────────────────────────────────────────

fn message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: to_db(OffsetDateTime::now_utc()),
        attempts,
    }
}

fn sample_event() -> UsageEvent {
    serde_json::from_value(json!({
        "tenant_id": TENANT_A, "user_id": USER_A, "chat_id": Uuid::new_v4(),
        "turn_id": Uuid::new_v4(), "request_id": Uuid::new_v4(),
        "effective_model": "m", "selected_model": "m",
        "terminal_state": "completed", "billing_outcome": "completed",
        "usage": {"input_tokens": 1, "output_tokens": 2},
        "actual_credits_micro": 3, "settlement_method": "actual",
        "policy_version_applied": 1, "timestamp": "2026-03-01T10:00:00Z",
        "requester_type": "user", "dedupe_key": "k/k/k"
    }))
    .expect("usage event")
}

fn handler_over(plugin: Arc<QuotaPolicy>) -> UsageOutboxHandler {
    UsageOutboxHandler::new(Arc::new(ModelPolicyGateway::from_client(plugin)))
}

#[tokio::test]
async fn the_handler_acknowledges_a_delivered_event() {
    let plugin = QuotaPolicy::new(&quota_models(), 1, roomy());
    let handler = handler_over(plugin.clone());
    let event = sample_event();
    let result = handler
        .handle(&message(serde_json::to_vec(&event).unwrap(), 0))
        .await;
    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(plugin.published(), vec![event]);
}

#[tokio::test]
async fn the_handler_retries_a_transient_failure() {
    let plugin = QuotaPolicy::new(&quota_models(), 1, roomy());
    plugin.script_publish(vec![Err(PublishError::Transient("later".to_owned()))]);
    let handler = handler_over(plugin.clone());
    let result = handler
        .handle(&message(serde_json::to_vec(&sample_event()).unwrap(), 3))
        .await;
    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert!(plugin.published().is_empty());
}

#[tokio::test]
async fn the_handler_rejects_a_permanent_failure() {
    let plugin = QuotaPolicy::new(&quota_models(), 1, roomy());
    plugin.script_publish(vec![Err(PublishError::Permanent("never".to_owned()))]);
    let handler = handler_over(plugin);
    let result = handler
        .handle(&message(serde_json::to_vec(&sample_event()).unwrap(), 0))
        .await;
    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
}

#[tokio::test]
async fn the_handler_rejects_a_payload_that_is_not_a_usage_event() {
    let plugin = QuotaPolicy::new(&quota_models(), 1, roomy());
    let handler = handler_over(plugin.clone());
    for payload in [
        b"not json".to_vec(),
        br#"{"tenant_id": 1}"#.to_vec(),
        Vec::new(),
    ] {
        let result = handler.handle(&message(payload, 0)).await;
        assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    }
    assert_eq!(
        plugin
            .publish_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the plugin is never called with a corrupt payload"
    );
}

#[tokio::test]
async fn the_handler_retries_when_the_plugin_cannot_be_resolved() {
    // no types-registry client in the hub: resolution fails, nothing is lost
    let gateway = Arc::new(ModelPolicyGateway::new(
        Arc::new(toolkit::client_hub::ClientHub::new()),
        "constructorfabric",
    ));
    let handler = UsageOutboxHandler::new(gateway);
    let result = handler
        .handle(&message(serde_json::to_vec(&sample_event()).unwrap(), 0))
        .await;
    assert!(matches!(result, MessageResult::Retry), "{result:?}");
}

#[tokio::test]
async fn the_gateway_maps_publish_errors_to_delivery_outcomes() {
    let plugin = QuotaPolicy::new(&quota_models(), 1, roomy());
    let gateway = ModelPolicyGateway::from_client(plugin.clone());
    plugin.script_publish(vec![
        Ok(()),
        Err(PublishError::Transient("t".to_owned())),
        Err(PublishError::Permanent("p".to_owned())),
    ]);
    let event = sample_event();
    assert!(matches!(
        gateway.publish_usage(event.clone()).await,
        UsageDeliveryOutcome::Delivered
    ));
    assert!(matches!(
        gateway.publish_usage(event.clone()).await,
        UsageDeliveryOutcome::Retry
    ));
    assert!(matches!(
        gateway.publish_usage(event).await,
        UsageDeliveryOutcome::Reject(_)
    ));
}

#[tokio::test]
async fn the_gateway_serves_a_snapshot_by_version() {
    // S1 finding F-007: the settlement path asks for the snapshot of `policy_version_applied`
    let plugin = QuotaPolicy::new(&quota_models(), 1, roomy());
    let mut doubled = quota_models();
    for model in &mut doubled {
        model["input_tokens_credit_multiplier_micro"] = json!(99);
    }
    plugin.publish_version(2, &doubled);
    let gateway = ModelPolicyGateway::from_client(plugin);

    let old = gateway
        .snapshot_at(USER_A, 1)
        .await
        .expect("version 1 stays available");
    assert_eq!(old.policy_version, 1);
    assert_eq!(
        old.model_catalog[0].input_tokens_credit_multiplier_micro,
        PREMIUM_IN
    );
    let new = gateway.snapshot_at(USER_A, 2).await.expect("version 2");
    assert_eq!(
        new.model_catalog[0].input_tokens_credit_multiplier_micro,
        99
    );
    let missing = gateway.snapshot_at(USER_A, 3).await;
    assert!(
        matches!(
            missing,
            Err(mini_chat::domain::error::DomainError::Internal { .. })
        ),
        "{missing:?}"
    );
    assert_eq!(
        gateway
            .current_snapshot(USER_A)
            .await
            .unwrap()
            .policy_version,
        2
    );
}
