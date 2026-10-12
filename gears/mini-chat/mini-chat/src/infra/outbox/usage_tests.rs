#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    BillingOutcome, PolicySnapshot, RequesterType, SettlementMethod, UsageEvent,
    UsageTerminalState, UserLimits,
};
use parking_lot::Mutex;
use time::macros::datetime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use super::UsageOutboxHandler;
use crate::domain::error::DomainError;
use crate::domain::ports::{ModelPolicyGateway, UsagePublication};

struct Gateway {
    answer: UsagePublication,
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl ModelPolicyGateway for Gateway {
    async fn current_snapshot(&self, _user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        Err(DomainError::internal("not used"))
    }

    async fn snapshot_at(
        &self,
        _user_id: Uuid,
        _version: i64,
    ) -> Result<PolicySnapshot, DomainError> {
        Err(DomainError::internal("not used"))
    }

    async fn user_limits(&self, _user_id: Uuid, _version: i64) -> Result<UserLimits, DomainError> {
        Err(DomainError::internal("not used"))
    }

    async fn publish_usage(&self, event: UsageEvent) -> UsagePublication {
        self.seen.lock().push(event.dedupe_key);
        self.answer.clone()
    }
}

fn event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: UsageTerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 1,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: datetime!(2026-01-01 00:00 UTC),
        requester_type: RequesterType::User,
        dedupe_key: "key".to_owned(),
        system_task_type: None,
    }
}

fn message(payload: Vec<u8>) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts: 0,
    }
}

fn handler(answer: UsagePublication) -> (UsageOutboxHandler, Arc<Gateway>) {
    let gateway = Arc::new(Gateway {
        answer,
        seen: Mutex::new(Vec::new()),
    });
    (UsageOutboxHandler::new(gateway.clone()), gateway)
}

#[tokio::test]
async fn a_published_event_is_acknowledged() {
    let (handler, gateway) = handler(UsagePublication::Published);

    let result = handler
        .handle(&message(serde_json::to_vec(&event()).unwrap()))
        .await;

    assert!(matches!(result, MessageResult::Ok));
    assert_eq!(*gateway.seen.lock(), ["key"]);
}

#[tokio::test]
async fn a_transient_plugin_failure_is_retried() {
    let (handler, _) = handler(UsagePublication::Retry("busy".to_owned()));

    let result = handler
        .handle(&message(serde_json::to_vec(&event()).unwrap()))
        .await;

    assert!(matches!(result, MessageResult::Retry));
}

#[tokio::test]
async fn a_permanent_plugin_failure_is_rejected() {
    let (handler, _) = handler(UsagePublication::Reject("nope".to_owned()));

    let result = handler
        .handle(&message(serde_json::to_vec(&event()).unwrap()))
        .await;

    assert!(matches!(result, MessageResult::Reject(reason) if reason.contains("nope")));
}

#[tokio::test]
async fn an_undecodable_payload_is_rejected_without_calling_the_plugin() {
    let (handler, gateway) = handler(UsagePublication::Published);

    let result = handler.handle(&message(b"{not json".to_vec())).await;

    assert!(matches!(result, MessageResult::Reject(_)));
    assert!(gateway.seen.lock().is_empty());
}
