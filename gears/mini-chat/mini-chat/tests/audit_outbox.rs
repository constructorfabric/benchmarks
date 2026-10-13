//! Turn audit events: enqueued in the finalization transaction to the audit queue and delivered to
//! the audit plugin by the audit outbox handler.
//!
//! Source of truth: DESIGN 3.2 "Audit plugin and audit outbox", 3.9 "Audit Events for Turn
//! Mutations" (`turn_completed` / `turn_failed`), B.9.3 (handler outcomes `Ok` / `Retry` /
//! `Reject`, 30 s plugin timeout, retry bounded at the 120th attempt), 5.7 "FinalizeTurn
//! Invariant" (audit enqueue in the finalization transaction), ADR-0009 (audit content: identities,
//! model, token usage, latency, tool-call counts, quota decision; no prompt / response /
//! attachments), the SDK contract `AuditEvent` / `TurnAuditEvent`.

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
use std::sync::atomic::Ordering;
use std::time::Duration;

use mini_chat::infra::audit::AuditGateway;
use mini_chat::infra::db::timestamp::to_db;
use mini_chat::infra::outbox::{AUDIT_MAX_ATTEMPTS, AuditOutboxHandler};
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    TurnAuditEvent,
};
use quota_support::{
    LimitsSpec, Opts, PREMIUM, Piece, ProviderReply, QuotaHarness, RecordingAudit, STANDARD,
    TENANT_A, USER_A, audit_gateway_for, ctx_a, failed_stream,
};
use serde_json::json;
use time::OffsetDateTime;
use toolkit::client_hub::ClientHub;
use toolkit::gts::PluginV1;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

const MSG: &str = "hello";
const VENDOR: &str = "constructorfabric";

fn roomy() -> LimitsSpec {
    LimitsSpec::new(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000)
}

fn completed(event: &AuditEvent) -> &TurnAuditEvent {
    match event {
        AuditEvent::TurnCompleted(e) => e,
        other => panic!("expected turn_completed, got {other:?}"),
    }
}

fn failed(event: &AuditEvent) -> &TurnAuditEvent {
    match event {
        AuditEvent::TurnFailed(e) => e,
        other => panic!("expected turn_failed, got {other:?}"),
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// content of the turn events
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_turn_audit_event_carries_the_cache_and_reasoning_breakdown() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().usage(500, 40).details(300, 7),
    ));

    h.say(chat, MSG).await;
    let events = h.audit.wait_events(1).await;
    let e = completed(&events[0]);
    let usage = e.usage.as_ref().expect("token usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (500, 40));
    assert_eq!(
        (
            usage.cache_read_input_tokens,
            usage.cache_write_input_tokens,
            usage.reasoning_tokens
        ),
        (300, 0, 7)
    );
}

#[tokio::test]
async fn a_completed_turn_emits_turn_completed_with_identities_model_usage_and_latency() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default()
            .usage(800, 200)
            .insert_after(1, vec![Piece::Sleep(Duration::from_millis(300))]),
    ));
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(1);

    let sent = h.say(chat, MSG).await;
    let events = h.audit.wait_events(1).await;
    let after = OffsetDateTime::now_utc() + time::Duration::seconds(1);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type(), "turn_completed");
    let e = completed(&events[0]);
    assert_eq!(e.tenant_id, TENANT_A);
    assert_eq!(e.user_id, USER_A);
    assert_eq!(e.chat_id, chat);
    assert_eq!(e.request_id, sent.request_id());
    assert_eq!(e.selected_model, PREMIUM);
    assert_eq!(e.effective_model, PREMIUM);
    let usage = e.usage.as_ref().expect("token usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (800, 200));
    assert!(
        (250..10_000).contains(&e.latency_ms),
        "latency covers the provider delay: {}",
        e.latency_ms
    );
    assert_eq!((e.web_search_calls, e.file_search_calls), (0, 0));
    assert_eq!(e.quota_decision, "allow");
    assert_eq!(e.error_code, None);
    assert!(e.timestamp >= before && e.timestamp <= after);
}

#[tokio::test]
async fn the_quota_decision_is_allow_or_the_downgrade_reason() {
    for (switch, reason) in [
        ("force_standard_tier", "force_standard_tier"),
        ("disable_premium_tier", "disable_premium_tier"),
    ] {
        let h = QuotaHarness::with_limits(roomy()).await;
        h.policy.set_kill_switches(json!({ switch: true }));
        let chat = h.seed_chat().await;
        h.say(chat, MSG).await;
        let events = h.audit.wait_events(1).await;
        let e = completed(&events[0]);
        assert_eq!(e.quota_decision, reason);
        assert_eq!(
            (e.selected_model.as_str(), e.effective_model.as_str()),
            (PREMIUM, STANDARD)
        );
    }

    let h = QuotaHarness::with_limits(LimitsSpec::new(
        1_000_000_000,
        10_000_000_000,
        500_000,
        5_000_000,
    ))
    .await;
    h.seed_usage(quota_support::UsageSeed::premium("daily").spent(500_000))
        .await;
    let chat = h.seed_chat().await;
    h.say(chat, MSG).await;
    let events = h.audit.wait_events(1).await;
    assert_eq!(
        completed(&events[0]).quota_decision,
        "premium_quota_exhausted"
    );

    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat_on(quota_support::DISABLED).await;
    h.say(chat, MSG).await;
    let events = h.audit.wait_events(1).await;
    assert_eq!(completed(&events[0]).quota_decision, "model_disabled");
}

#[tokio::test]
async fn a_failed_turn_emits_turn_failed_with_the_error_code() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "boom",
        &["p"],
        Some((700, 40)),
    )));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("provider_error"));

    let events = h.audit.wait_events(1).await;
    assert_eq!(events[0].event_type(), "turn_failed");
    let e = failed(&events[0]);
    assert_eq!(e.error_code.as_deref(), Some("provider_error"));
    assert_eq!(e.request_id, sent.request_id());
    let usage = e.usage.as_ref().expect("the usage the provider reported");
    assert_eq!((usage.input_tokens, usage.output_tokens), (700, 40));
    assert_eq!(e.quota_decision, "allow");

    // without usage the field is empty
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    h.say(chat, "second").await;
    let events = h.audit.wait_events(2).await;
    assert_eq!(failed(&events[1]).usage, None);
}

#[tokio::test]
async fn a_cancelled_turn_is_a_turn_failed_without_an_error_code() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["a"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG, "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(rid, "cancelled").await;

    let events = h.audit.wait_events(1).await;
    assert_eq!(
        events[0].event_type(),
        "turn_failed",
        "there is no turn_cancelled type"
    );
    let e = failed(&events[0]);
    assert_eq!(e.request_id, rid);
    assert_eq!(e.error_code, None, "a cancellation is not an error");
}

#[tokio::test]
async fn audit_events_carry_no_prompt_response_or_provider_identifiers() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["SECRET-ANSWER-TEXT"],
        &Opts::default().response_id("resp_AuditProbe0001"),
    ));
    h.say(chat, "SECRET-PROMPT-TEXT").await;
    let events = h.audit.wait_events(1).await;

    let wire = serde_json::to_string(&events[0]).unwrap();
    assert!(!wire.contains("SECRET-PROMPT-TEXT"), "{wire}");
    assert!(!wire.contains("SECRET-ANSWER-TEXT"), "{wire}");
    assert!(!wire.contains("resp_AuditProbe0001"), "{wire}");
    assert!(
        !wire.contains("provider-model"),
        "no provider_model_id: {wire}"
    );
    assert!(
        !stream_support::contains_provider_identifier(&wire),
        "{wire}"
    );
    let value: serde_json::Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(value["event_type"], json!("turn_completed"));
    for forbidden in ["prompt", "response", "attachments", "content"] {
        assert!(
            value.get(forbidden).is_none(),
            "`{forbidden}` is not audited: {wire}"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// exactly one event per finalized turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn one_audit_event_per_finalized_turn_and_none_for_rejections_or_replays() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    let chat = h.seed_chat_on(STANDARD).await;
    let rid = Uuid::new_v4();
    h.say_with(chat, MSG, rid).await;
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    h.say(chat, "second").await;
    h.audit.wait_events(2).await;

    h.say_with(chat, MSG, rid).await; // replay
    h.seed_usage(quota_support::UsageSeed::total("daily").spent(1_000_000))
        .await;
    assert_eq!(
        h.reject(chat, json!({"content": "third"}))
            .await
            .status
            .as_u16(),
        429
    );

    h.audit.assert_events_stay(2).await;
    let events = h.audit.events();
    assert_eq!(
        events
            .iter()
            .map(AuditEvent::event_type)
            .collect::<Vec<_>>(),
        vec!["turn_completed", "turn_failed"]
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// delivery through the audit gateway
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_transient_plugin_failure_is_retried_and_a_permanent_one_is_dropped_from_the_queue() {
    let h = QuotaHarness::new().await;
    h.audit.script(vec![
        Err(AuditPluginError::Transient("busy".to_owned())),
        Err(AuditPluginError::PluginTimeout),
    ]);
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let events = h.audit.wait_events(1).await;
    assert_eq!(completed(&events[0]).request_id, sent.request_id());
    assert!(
        h.audit.calls.load(Ordering::SeqCst) >= 3,
        "two failures and the accepted attempt"
    );

    // permanent: rejected once, never retried, the next event still flows
    let calls_before = h.audit.calls.load(Ordering::SeqCst);
    h.audit
        .script(vec![Err(AuditPluginError::Permanent("schema".to_owned()))]);
    h.say(chat, "second").await;
    quota_support::eventually("the permanent failure was consumed", || async {
        h.audit.calls.load(Ordering::SeqCst) > calls_before
    })
    .await;
    let third = h.say(chat, "third").await;
    let events = h.audit.wait_events(2).await;
    assert_eq!(completed(&events[1]).request_id, third.request_id());
    h.audit.assert_events_stay(2).await;
    assert_eq!(
        h.audit.calls.load(Ordering::SeqCst),
        calls_before + 2,
        "one rejected attempt and one accepted one"
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

fn sample_event() -> AuditEvent {
    serde_json::from_value(json!({
        "event_type": "turn_completed",
        "tenant_id": TENANT_A, "user_id": USER_A, "chat_id": Uuid::new_v4(),
        "request_id": Uuid::new_v4(), "selected_model": "m", "effective_model": "m",
        "usage": {"input_tokens": 1, "output_tokens": 2},
        "latency_ms": 5, "web_search_calls": 0, "file_search_calls": 0,
        "quota_decision": "allow", "timestamp": "2026-03-01T10:00:00Z"
    }))
    .expect("audit event")
}

fn payload() -> Vec<u8> {
    serde_json::to_vec(&sample_event()).unwrap()
}

fn handler_with(plugin: &Arc<RecordingAudit>) -> AuditOutboxHandler {
    let dyn_plugin: Arc<dyn MiniChatAuditPluginClientV1> = plugin.clone();
    AuditOutboxHandler::new(audit_gateway_for(VENDOR, Some(dyn_plugin)))
}

#[tokio::test]
async fn the_handler_acknowledges_a_delivered_event() {
    let plugin = Arc::new(RecordingAudit::default());
    // one event: `sample_event()` draws fresh random ids on every call
    let event = sample_event();
    let result = handler_with(&plugin)
        .handle(&message(serde_json::to_vec(&event).unwrap(), 0))
        .await;
    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(plugin.events(), vec![event]);
}

#[tokio::test]
async fn the_handler_acknowledges_and_drops_when_no_plugin_is_registered() {
    let handler = AuditOutboxHandler::new(audit_gateway_for(VENDOR, None));
    let result = handler.handle(&message(payload(), 0)).await;
    assert!(
        matches!(result, MessageResult::Ok),
        "dropped events are acknowledged: {result:?}"
    );
}

#[tokio::test]
async fn the_handler_rejects_a_corrupt_payload_before_resolving_the_plugin() {
    for gateway in [
        audit_gateway_for(VENDOR, None),
        audit_gateway_for(
            VENDOR,
            Some(Arc::new(RecordingAudit::default()) as Arc<dyn MiniChatAuditPluginClientV1>),
        ),
        // a hub that cannot even resolve (no types-registry client)
        Arc::new(AuditGateway::new(Arc::new(ClientHub::new()), VENDOR)),
    ] {
        let handler = AuditOutboxHandler::new(gateway);
        for corrupt in [
            b"not json".to_vec(),
            br#"{"event_type": "turn_unknown"}"#.to_vec(),
            br#"{"tenant_id": 1}"#.to_vec(),
            Vec::new(),
        ] {
            let result = handler.handle(&message(corrupt, 0)).await;
            assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
        }
    }
}

#[tokio::test]
async fn the_handler_retries_transient_failures_and_timeouts() {
    for error in [
        AuditPluginError::Transient("busy".to_owned()),
        AuditPluginError::PluginTimeout,
    ] {
        let plugin = Arc::new(RecordingAudit::default());
        plugin.script(vec![Err(error)]);
        let result = handler_with(&plugin).handle(&message(payload(), 0)).await;
        assert!(matches!(result, MessageResult::Retry), "{result:?}");
    }
}

#[tokio::test]
async fn the_handler_rejects_a_permanent_plugin_error() {
    let plugin = Arc::new(RecordingAudit::default());
    plugin.script(vec![Err(AuditPluginError::Permanent(
        "bad schema".to_owned(),
    ))]);
    let result = handler_with(&plugin).handle(&message(payload(), 0)).await;
    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
}

#[tokio::test]
async fn the_handler_retries_when_the_plugin_cannot_be_resolved() {
    // no types-registry client at all
    let handler = AuditOutboxHandler::new(Arc::new(AuditGateway::new(
        Arc::new(ClientHub::new()),
        VENDOR,
    )));
    let result = handler.handle(&message(payload(), 0)).await;
    assert!(matches!(result, MessageResult::Retry), "{result:?}");

    // an instance registered in the types-registry whose client is missing from the ClientHub
    let hub = Arc::new(ClientHub::new());
    let (id, registration) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
        "cf.core._.no_client.v1",
        VENDOR,
        100,
    )
    .expect("registration");
    let id: &str = id.as_ref();
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(
        MockTypesRegistryClient::new().with_instances([make_test_instance(id, registration)]),
    );
    hub.register::<dyn TypesRegistryClient>(registry);
    let handler = AuditOutboxHandler::new(Arc::new(AuditGateway::new(hub, VENDOR)));
    let result = handler.handle(&message(payload(), 0)).await;
    assert!(matches!(result, MessageResult::Retry), "{result:?}");
}

#[tokio::test]
async fn a_retry_is_bounded_at_the_120th_attempt() {
    assert_eq!(AUDIT_MAX_ATTEMPTS, 120);
    let last_retry = i16::try_from(AUDIT_MAX_ATTEMPTS - 2).unwrap(); // the 119th attempt
    let last = i16::try_from(AUDIT_MAX_ATTEMPTS - 1).unwrap(); // the 120th attempt (attempts = 119)

    let plugin = Arc::new(RecordingAudit::default());
    plugin.script(vec![Err(AuditPluginError::Transient("busy".to_owned()))]);
    let result = handler_with(&plugin)
        .handle(&message(payload(), last_retry))
        .await;
    assert!(
        matches!(result, MessageResult::Retry),
        "119th attempt: {result:?}"
    );

    let plugin = Arc::new(RecordingAudit::default());
    plugin.script(vec![Err(AuditPluginError::Transient("busy".to_owned()))]);
    let result = handler_with(&plugin)
        .handle(&message(payload(), last))
        .await;
    assert!(
        matches!(result, MessageResult::Reject(_)),
        "a retry on the last attempt is dead-lettered so a broken plugin cannot block the partition: {result:?}"
    );

    // the same bound applies to a resolution failure
    let handler = AuditOutboxHandler::new(Arc::new(AuditGateway::new(
        Arc::new(ClientHub::new()),
        VENDOR,
    )));
    let result = handler.handle(&message(payload(), last)).await;
    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");

    // a successful delivery on the last attempt is still acknowledged
    let plugin = Arc::new(RecordingAudit::default());
    let result = handler_with(&plugin)
        .handle(&message(payload(), last))
        .await;
    assert!(matches!(result, MessageResult::Ok), "{result:?}");
}

#[tokio::test]
async fn the_audit_wire_form_is_tagged_by_event_type() {
    let wire = serde_json::to_value(sample_event()).unwrap();
    assert_eq!(wire["event_type"], json!("turn_completed"));
    assert!(wire.get("user_id").is_some() && wire.get("quota_decision").is_some());
}
