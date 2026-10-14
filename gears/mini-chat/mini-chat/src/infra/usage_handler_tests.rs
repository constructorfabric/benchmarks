//! Usage outbox handler tests: `Ok`, `Retry` and `Reject` semantics against a stubbed publisher
//! and against the real gateway over a scripted plugin, the corrupt payload, the absence of an
//! attempt cap and of deduplication, and the usage delivery state machine.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler`,
//! state `cpt-cf-mini-chat-state-quota-and-billing-usage-delivery`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-usage-publication`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use super::UsageOutboxHandler;
use crate::config::OutboxConfig;
use crate::domain::quota::{UsagePublishError, UsagePublisher};
use crate::infra::outbox::{QueueKind, USAGE_PAYLOAD_TYPE, queue_specs};
use crate::test_support::quota::{PublishScript, plugin_gateway, unresolvable_gateway, usage, utc};
use crate::test_support::{TENANT_A, USER_A1};

fn sample_event() -> UsageEvent {
    UsageEvent {
        tenant_id: TENANT_A,
        user_id: Some(USER_A1),
        chat_id: Uuid::from_u128(0xC4A7),
        turn_id: Some(Uuid::from_u128(0x7001)),
        request_id: Uuid::from_u128(0x7002),
        effective_model: "standard-1".to_owned(),
        selected_model: "premium-1".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: Some(usage(900, 300)),
        actual_credits_micro: 1_200_000,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 3,
        web_search_calls: 1,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: utc(2026, 2, 28, 15, 30, 5),
        requester_type: "user".to_owned(),
        dedupe_key: Some(format!(
            "{}/{}/{}",
            TENANT_A.simple(),
            Uuid::from_u128(0x7001).simple(),
            Uuid::from_u128(0x7002).simple()
        )),
        system_task_type: None,
    }
}

fn system_event() -> UsageEvent {
    UsageEvent {
        user_id: None,
        turn_id: None,
        requester_type: "system".to_owned(),
        billing_outcome: "system_task".to_owned(),
        settlement_method: "none".to_owned(),
        actual_credits_micro: 0,
        policy_version_applied: 0,
        system_task_type: Some("thread_summary_update".to_owned()),
        dedupe_key: Some(format!(
            "{}/thread_summary_update/{}",
            TENANT_A.simple(),
            Uuid::from_u128(0x5157).simple()
        )),
        ..sample_event()
    }
}

fn message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: USAGE_PAYLOAD_TYPE.to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn message_of(event: &UsageEvent, attempts: i16) -> OutboxMessage {
    message(serde_json::to_vec(event).unwrap(), attempts)
}

fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn is_reject(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Reject(_))
}

// ── a stubbed publisher ──────────────────────────────────────────────────────

struct StubPublisher {
    result: Mutex<Result<(), UsagePublishError>>,
    published: Mutex<Vec<UsageEvent>>,
}

impl StubPublisher {
    fn new(result: Result<(), UsagePublishError>) -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(result),
            published: Mutex::new(Vec::new()),
        })
    }

    fn published(&self) -> Vec<UsageEvent> {
        self.published.lock().unwrap().clone()
    }

    fn set(&self, result: Result<(), UsagePublishError>) {
        *self.result.lock().unwrap() = result;
    }
}

#[async_trait]
impl UsagePublisher for StubPublisher {
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), UsagePublishError> {
        self.published.lock().unwrap().push(event);
        self.result.lock().unwrap().clone()
    }
}

fn handler(publisher: &Arc<StubPublisher>) -> UsageOutboxHandler {
    UsageOutboxHandler::new(publisher.clone())
}

#[test]
fn the_payload_type_is_a_valid_outbox_payload_type() {
    assert!(!USAGE_PAYLOAD_TYPE.is_empty() && USAGE_PAYLOAD_TYPE.len() <= 1024);
}

#[tokio::test]
async fn an_accepted_event_is_acknowledged_and_published_unchanged() {
    let publisher = StubPublisher::new(Ok(()));
    let event = sample_event();
    let result = handler(&publisher).handle(&message_of(&event, 0)).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(publisher.published(), vec![event]);
}

#[tokio::test]
async fn a_system_task_event_is_published_like_any_other() {
    let publisher = StubPublisher::new(Ok(()));
    let event = system_event();
    let result = handler(&publisher).handle(&message_of(&event, 0)).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(publisher.published(), vec![event]);
}

#[tokio::test]
async fn a_transient_publish_error_is_a_retry() {
    let publisher =
        StubPublisher::new(Err(UsagePublishError::Transient("backend busy".to_owned())));
    let result = handler(&publisher)
        .handle(&message_of(&sample_event(), 0))
        .await;
    assert!(is_retry(&result), "{result:?}");
    assert_eq!(publisher.published().len(), 1);
}

#[tokio::test]
async fn a_permanent_publish_error_is_a_reject_with_a_reason() {
    let publisher = StubPublisher::new(Err(UsagePublishError::Permanent("rejected".to_owned())));
    let result = handler(&publisher)
        .handle(&message_of(&sample_event(), 0))
        .await;
    match result {
        MessageResult::Reject(reason) => assert!(!reason.is_empty()),
        other => panic!("expected a reject, got {other:?}"),
    }
}

#[tokio::test]
async fn a_corrupt_payload_is_rejected_without_calling_the_plugin() {
    let publisher = StubPublisher::new(Ok(()));
    for payload in [
        b"not json".to_vec(),
        b"{}".to_vec(),
        b"[]".to_vec(),
        br#"{"tenant_id":"not-a-uuid"}"#.to_vec(),
        Vec::new(),
    ] {
        let result = handler(&publisher)
            .handle(&message(payload.clone(), 0))
            .await;
        assert!(is_reject(&result), "{payload:?}: {result:?}");
    }
    assert!(publisher.published().is_empty());
}

#[tokio::test]
async fn the_handler_has_no_attempt_cap_so_a_late_attempt_still_retries() {
    // unlike the audit handler, which rejects on its 120th attempt
    let publisher = StubPublisher::new(Err(UsagePublishError::Transient("busy".to_owned())));
    for attempts in [0_i16, 119, 120, 1000, i16::MAX] {
        let result = handler(&publisher)
            .handle(&message_of(&sample_event(), attempts))
            .await;
        assert!(is_retry(&result), "attempt {attempts}: {result:?}");
    }
}

#[tokio::test]
async fn the_handler_does_not_deduplicate_a_redelivered_message() {
    let publisher = StubPublisher::new(Ok(()));
    let delivery = message_of(&sample_event(), 0);
    assert!(is_ok(&handler(&publisher).handle(&delivery).await));
    assert!(is_ok(&handler(&publisher).handle(&delivery).await));
    assert_eq!(
        publisher.published().len(),
        2,
        "the consumer deduplicates by the dedupe key"
    );
    let events = publisher.published();
    assert_eq!(events[0].dedupe_key, events[1].dedupe_key);
}

// ── the real gateway over a scripted plugin ──────────────────────────────────

#[tokio::test]
async fn through_the_gateway_an_accepting_plugin_receives_the_event_and_the_message_is_acknowledged()
 {
    let wired = plugin_gateway(PublishScript::Accept);
    let handler = UsageOutboxHandler::new(wired.gateway.clone());
    let event = sample_event();
    let result = handler.handle(&message_of(&event, 0)).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(wired.plugin.published(), vec![event]);
}

#[tokio::test]
async fn through_the_gateway_a_transient_plugin_error_is_a_retry_and_a_permanent_one_a_reject() {
    let wired = plugin_gateway(PublishScript::Transient);
    let handler = UsageOutboxHandler::new(wired.gateway.clone());
    assert!(is_retry(
        &handler.handle(&message_of(&sample_event(), 0)).await
    ));
    wired.plugin.set_script(PublishScript::Permanent);
    assert!(is_reject(
        &handler.handle(&message_of(&sample_event(), 1)).await
    ));
}

#[tokio::test]
async fn a_plugin_that_cannot_be_resolved_is_a_retry_so_no_committed_usage_is_lost() {
    let handler = UsageOutboxHandler::new(unresolvable_gateway());
    let result = handler.handle(&message_of(&sample_event(), 0)).await;
    assert!(is_retry(&result), "{result:?}");
}

// ── the usage delivery state machine ─────────────────────────────────────────

#[tokio::test]
async fn usage_state_enqueued_to_delivered_when_the_plugin_accepts() {
    let publisher = StubPublisher::new(Ok(()));
    assert!(is_ok(
        &handler(&publisher)
            .handle(&message_of(&sample_event(), 0))
            .await
    ));
}

#[tokio::test]
async fn usage_state_enqueued_to_retrying_to_delivered() {
    let publisher = StubPublisher::new(Err(UsagePublishError::Transient("busy".to_owned())));
    let delivery = message_of(&sample_event(), 0);
    assert!(is_retry(&handler(&publisher).handle(&delivery).await));
    publisher.set(Ok(()));
    assert!(is_ok(
        &handler(&publisher)
            .handle(&message_of(&sample_event(), 1))
            .await
    ));
}

#[tokio::test]
async fn usage_state_enqueued_to_dead_lettered_on_a_permanent_error_or_a_corrupt_payload() {
    let permanent = StubPublisher::new(Err(UsagePublishError::Permanent("no".to_owned())));
    assert!(is_reject(
        &handler(&permanent)
            .handle(&message_of(&sample_event(), 0))
            .await
    ));
    let corrupt = StubPublisher::new(Ok(()));
    assert!(is_reject(
        &handler(&corrupt)
            .handle(&message(b"garbage".to_vec(), 0))
            .await
    ));
}

#[tokio::test]
async fn usage_state_retrying_to_dead_lettered_when_a_later_attempt_is_permanent() {
    let publisher = StubPublisher::new(Err(UsagePublishError::Transient("busy".to_owned())));
    assert!(is_retry(
        &handler(&publisher)
            .handle(&message_of(&sample_event(), 0))
            .await
    ));
    publisher.set(Err(UsagePublishError::Permanent("no".to_owned())));
    assert!(is_reject(
        &handler(&publisher)
            .handle(&message_of(&sample_event(), 1))
            .await
    ));
}

// ── the usage queue registration ─────────────────────────────────────────────

#[test]
fn the_usage_queue_name_and_partitions_come_from_the_outbox_section() {
    let defaults = OutboxConfig::default();
    assert_eq!(defaults.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(defaults.num_partitions, 4);
    let specs = queue_specs(&defaults, 300);
    let usage = specs
        .iter()
        .find(|spec| spec.kind == QueueKind::Usage)
        .unwrap();
    assert_eq!(usage.name, "mini-chat.usage_snapshot");
    assert_eq!(usage.partitions, 4);

    let custom = OutboxConfig {
        queue_name: "acme.usage".to_owned(),
        num_partitions: 8,
        ..OutboxConfig::default()
    };
    let usage = queue_specs(&custom, 300)
        .into_iter()
        .find(|spec| spec.kind == QueueKind::Usage)
        .unwrap();
    assert_eq!(usage.name, "acme.usage");
    assert_eq!(usage.partitions, 8);
}
