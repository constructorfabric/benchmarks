//! Tests of the usage and audit outbox handlers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, PolicySnapshot, TurnMutationAuditEvent, UsageEvent, UserLimits};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use crate::infra::outbox::handlers::audit::AuditHandler;
use crate::infra::outbox::handlers::usage::UsageHandler;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuditDelivery, AuditFailure, AuditPort, PolicyPort, PublishFailure};
use crate::domain::services::AppServices;
use crate::testing::{FakeAuthz, FakePolicy, MockProvider, MockTransport, TENANT_A, test_config, test_db};

/// Policy whose `publish_usage` returns a scripted outcome.
#[derive(Default)]
struct ScriptedPolicy {
    inner: FakePolicy,
    failure: Mutex<Option<PublishFailure>>,
    calls: Mutex<Vec<UsageEvent>>,
}

#[async_trait]
impl PolicyPort for ScriptedPolicy {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        self.inner.current_snapshot(user_id).await
    }
    async fn snapshot_by_version(&self, user_id: Uuid, version: i64) -> Result<Arc<PolicySnapshot>, DomainError> {
        self.inner.snapshot_by_version(user_id, version).await
    }
    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError> {
        self.inner.user_limits(user_id, version).await
    }
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishFailure> {
        self.calls.lock().unwrap().push(event);
        self.failure.lock().unwrap().clone().map_or(Ok(()), Err)
    }
}

/// Audit port with a scripted outcome.
struct ScriptedAudit {
    outcome: Mutex<Result<AuditDelivery, AuditFailure>>,
    calls: Mutex<usize>,
}

impl Default for ScriptedAudit {
    fn default() -> Self {
        Self { outcome: Mutex::new(Ok(AuditDelivery::Delivered)), calls: Mutex::new(0) }
    }
}

#[async_trait]
impl AuditPort for ScriptedAudit {
    async fn emit(&self, _event: MiniChatAuditEvent) -> Result<AuditDelivery, AuditFailure> {
        *self.calls.lock().unwrap() += 1;
        self.outcome.lock().unwrap().clone()
    }
}

async fn app(policy: Arc<ScriptedPolicy>, audit: Arc<ScriptedAudit>) -> Arc<AppServices> {
    Arc::new(AppServices::new(
        Arc::new(test_config()),
        test_db().await,
        Arc::new(FakeAuthz::default()),
        policy,
        audit,
        Arc::new(MockTransport(Arc::new(MockProvider::default()))),
    ))
}

fn msg(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage { partition_id: 1, seq: 1, payload, payload_type: "t".to_owned(), created_at: chrono::Utc::now(), attempts }
}

fn usage_payload() -> Vec<u8> {
    let e = UsageEvent {
        tenant_id: TENANT_A,
        user_id: None,
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "m".into(),
        selected_model: "m".into(),
        terminal_state: "completed".into(),
        billing_outcome: "completed".into(),
        usage: None,
        actual_credits_micro: 5,
        settlement_method: "actual".into(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: crate::clock::now(),
        requester_type: "user".into(),
        dedupe_key: "a/b/c".into(),
        system_task_type: None,
    };
    serde_json::to_vec(&e).unwrap()
}

fn audit_payload() -> Vec<u8> {
    let e = MiniChatAuditEvent::TurnMutation(TurnMutationAuditEvent {
        event_type: "turn_delete".into(),
        tenant_id: TENANT_A,
        actor_user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(Uuid::new_v4()),
        timestamp: crate::clock::now(),
    });
    serde_json::to_vec(&e).unwrap()
}

fn is_ok(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Ok)
}
fn is_retry(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Retry)
}
fn is_reject(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Reject(_))
}

#[tokio::test]
async fn usage_handler_outcomes() {
    let policy = Arc::new(ScriptedPolicy::default());
    let h = UsageHandler::new(app(Arc::clone(&policy), Arc::new(ScriptedAudit::default())).await);

    assert!(is_ok(&h.handle(&msg(usage_payload(), 0)).await));
    let published = policy.calls.lock().unwrap()[0].clone();
    assert_eq!(published.dedupe_key, "a/b/c");
    assert_eq!(published.actual_credits_micro, 5);

    *policy.failure.lock().unwrap() = Some(PublishFailure::Transient("down".into()));
    assert!(is_retry(&h.handle(&msg(usage_payload(), 3)).await));

    *policy.failure.lock().unwrap() = Some(PublishFailure::Permanent("bad".into()));
    assert!(is_reject(&h.handle(&msg(usage_payload(), 0)).await));

    let calls_before = policy.calls.lock().unwrap().len();
    assert!(is_reject(&h.handle(&msg(b"{not json".to_vec(), 0)).await));
    assert_eq!(policy.calls.lock().unwrap().len(), calls_before, "corrupt payload is not published");
}

#[tokio::test]
async fn audit_handler_outcomes() {
    let audit = Arc::new(ScriptedAudit::default());
    let h = AuditHandler::new(app(Arc::new(ScriptedPolicy::default()), Arc::clone(&audit)).await);

    assert!(is_ok(&h.handle(&msg(audit_payload(), 0)).await));

    *audit.outcome.lock().unwrap() = Ok(AuditDelivery::NoPlugin);
    assert!(is_ok(&h.handle(&msg(audit_payload(), 0)).await), "no plugin: acknowledged and dropped");

    *audit.outcome.lock().unwrap() = Err(AuditFailure::Transient("timeout".into()));
    assert!(is_retry(&h.handle(&msg(audit_payload(), 0)).await));
    assert!(is_retry(&h.handle(&msg(audit_payload(), 118)).await));
    assert!(is_reject(&h.handle(&msg(audit_payload(), 119)).await), "120th attempt is dead-lettered");

    *audit.outcome.lock().unwrap() = Err(AuditFailure::Permanent("schema".into()));
    assert!(is_reject(&h.handle(&msg(audit_payload(), 0)).await));

    let calls_before = *audit.calls.lock().unwrap();
    *audit.outcome.lock().unwrap() = Ok(AuditDelivery::NoPlugin);
    assert!(is_reject(&h.handle(&msg(b"garbage".to_vec(), 0)).await), "corrupt payload rejected even without plugin");
    assert_eq!(*audit.calls.lock().unwrap(), calls_before, "plugin not called for a corrupt payload");
}
