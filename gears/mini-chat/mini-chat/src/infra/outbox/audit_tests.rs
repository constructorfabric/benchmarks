#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use mini_chat_sdk::{
    PolicyDecisions, RequesterType, ToolCalls, TurnAuditEvent, TurnAuditEventType, TurnLatency,
    UsageTerminalState,
};
use parking_lot::Mutex;
use time::macros::datetime;
use toolkit::client_hub::ClientHub;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use super::AuditOutboxHandler;
use crate::domain::metrics::Metrics;
use crate::infra::audit_gateway::AuditGateway;
use crate::test_support::FakeTypesRegistry;

#[derive(Default)]
struct Recorder {
    results: Mutex<Vec<String>>,
}

impl Metrics for Recorder {
    fn stream_started(&self, _: &str, _: &str) {}
    fn stream_completed(&self, _: &str, _: &str) {}
    fn stream_failed(&self, _: &str, _: &str, _: &str) {}
    fn stream_incomplete(&self, _: &str, _: &str, _: &str) {}
    fn stream_disconnected(&self, _: &str) {}
    fn active_streams(&self, _: i64) {}
    fn cancel_requested(&self, _: &str) {}
    fn cancel_effective(&self, _: &str) {}
    fn time_to_abort_ms(&self, _: &str, _: f64) {}
    fn streams_aborted(&self, _: &str) {}
    fn quota_preflight(&self, _: &str, _: &str, _: &str) {}
    fn quota_reserve(&self, _: &str) {}
    fn quota_commit(&self, _: &str) {}
    fn quota_overshoot(&self, _: &str) {}
    fn quota_estimated_tokens(&self, _: f64) {}
    fn quota_actual_tokens(&self, _: f64) {}
    fn code_interpreter_calls(&self, _: &str, _: u64) {}
    fn finalization_latency_ms(&self, _: f64) {}
    fn ttft_provider_ms(&self, _: &str, _: &str, _: f64) {}
    fn stream_total_latency_ms(&self, _: &str, _: &str, _: f64) {}
    fn orphan_detected(&self, _: &str) {}
    fn orphan_finalized(&self, _: &str) {}
    fn orphan_scan_duration_seconds(&self, _: f64) {}
    fn audit_emit(&self, result: &str) {
        self.results.lock().push(result.to_owned());
    }
}

fn event() -> TurnAuditEvent {
    TurnAuditEvent {
        event_type: TurnAuditEventType::Completed,
        timestamp: datetime!(2026-01-01 00:00 UTC),
        tenant_id: Uuid::new_v4(),
        requester_type: RequesterType::User,
        user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        selected_model: "m".to_owned(),
        effective_model: "m".to_owned(),
        terminal_state: UsageTerminalState::Completed,
        error_code: None,
        usage: None,
        latency: TurnLatency::default(),
        tool_calls: ToolCalls::default(),
        policy_decisions: PolicyDecisions::default(),
        prompt: String::new(),
        response: String::new(),
        trace_id: None,
    }
}

fn message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

/// A hub whose types registry knows no audit plugin.
fn hub_without_plugin() -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(FakeTypesRegistry::default()));
    hub
}

fn handler(hub: Arc<ClientHub>) -> (AuditOutboxHandler, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    (
        AuditOutboxHandler::new(
            Arc::new(AuditGateway::new(hub, "constructorfabric".to_owned())),
            recorder.clone(),
        ),
        recorder,
    )
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_whether_or_not_a_plugin_exists() {
    for hub in [hub_without_plugin(), Arc::new(ClientHub::default())] {
        let (handler, recorder) = handler(hub);

        let result = handler.handle(&message(b"nope".to_vec(), 0)).await;

        assert!(matches!(result, MessageResult::Reject(_)));
        assert_eq!(*recorder.results.lock(), ["reject"]);
    }
}

#[tokio::test]
async fn without_a_registered_plugin_a_valid_event_is_dropped_and_acknowledged() {
    let (handler, recorder) = handler(hub_without_plugin());

    let result = handler
        .handle(&message(serde_json::to_vec(&event()).unwrap(), 0))
        .await;

    assert!(matches!(result, MessageResult::Ok));
    assert_eq!(*recorder.results.lock(), ["dropped"]);
}

#[tokio::test]
async fn an_unreachable_registry_is_retried_until_the_attempt_cap_then_rejected() {
    let (handler, recorder) = handler(Arc::new(ClientHub::default()));
    let payload = serde_json::to_vec(&event()).unwrap();

    let early = handler.handle(&message(payload.clone(), 5)).await;
    let last = handler.handle(&message(payload, 119)).await;

    assert!(matches!(early, MessageResult::Retry));
    assert!(matches!(last, MessageResult::Reject(reason) if reason.contains("120")));
    assert_eq!(*recorder.results.lock(), ["retry", "reject"]);
}
