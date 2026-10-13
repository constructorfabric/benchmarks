#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent,
    UserLimits,
};
use parking_lot::Mutex;
use toolkit_db::outbox::{MessageResult, OutboxMessage};
use uuid::Uuid;

use super::*;
use crate::domain::service::billing::{
    BillingOutcome, TerminalState, UsageEventInput, build_mutation_audit_event, build_usage_event,
};
use crate::domain::service::quota::SettlementMethod;
use crate::infra::plugin_gateways::PluginLookup;

fn msg(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "test".into(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn usage_payload() -> Vec<u8> {
    let ev = build_usage_event(&UsageEventInput {
        tenant_id: Uuid::from_u128(1),
        user_id: Some(Uuid::from_u128(2)),
        chat_id: Uuid::from_u128(3),
        turn_id: Uuid::from_u128(4),
        request_id: Uuid::from_u128(5),
        effective_model: "m".into(),
        selected_model: "m".into(),
        terminal_state: TerminalState::Completed,
        outcome: BillingOutcome::Completed,
        method: SettlementMethod::Actual,
        usage: None,
        actual_credits_micro: 10,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
    });
    serde_json::to_vec(&ev).unwrap()
}

fn audit_payload() -> Vec<u8> {
    let ev = build_mutation_audit_event(
        "turn_delete",
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Uuid::from_u128(3),
        Uuid::from_u128(4),
        None,
    );
    serde_json::to_vec(&ev).unwrap()
}

// ── fakes ──────────────────────────────────────────────────────────────────

struct FakePolicy {
    result: Result<(), PublishError>,
    published: Mutex<Vec<UsageEvent>>,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Internal("unused".into()))
    }
    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        v: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::VersionNotFound(v))
    }
    async fn get_user_limits(&self, _u: Uuid, v: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::VersionNotFound(v))
    }
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().push(payload);
        self.result.clone()
    }
}

fn policy(result: Result<(), PublishError>) -> Arc<FakePolicy> {
    Arc::new(FakePolicy {
        result,
        published: Mutex::new(Vec::new()),
    })
}

struct FakeAudit {
    result: Result<(), AuditPluginError>,
    delay: Duration,
    emitted: Mutex<Vec<AuditEvent>>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FakeAudit {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.emitted.lock().push(event);
        self.result.clone()
    }
}

fn audit(result: Result<(), AuditPluginError>) -> Arc<FakeAudit> {
    Arc::new(FakeAudit {
        result,
        delay: Duration::ZERO,
        emitted: Mutex::new(Vec::new()),
    })
}

type PolicyLookup = Result<PluginLookup<dyn MiniChatModelPolicyPluginClientV1>, String>;
type AuditLookup = Result<PluginLookup<dyn MiniChatAuditPluginClientV1>, String>;

fn found_policy(p: &Arc<FakePolicy>) -> PolicyLookup {
    Ok(PluginLookup::Found(Arc::clone(p) as Arc<dyn MiniChatModelPolicyPluginClientV1>))
}

fn found_audit(a: &Arc<FakeAudit>) -> AuditLookup {
    Ok(PluginLookup::Found(Arc::clone(a) as Arc<dyn MiniChatAuditPluginClientV1>))
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

// ── usage ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn usage_success_publishes() {
    let p = policy(Ok(()));
    let r = handle_usage(&msg(usage_payload(), 0), async { found_policy(&p) }).await;
    assert!(is_ok(&r));
    let published = p.published.lock();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].actual_credits_micro, 10);
}

#[tokio::test]
async fn usage_malformed_payload_rejected() {
    let p = policy(Ok(()));
    let r = handle_usage(&msg(b"{not json".to_vec(), 0), async { found_policy(&p) }).await;
    assert!(is_reject(&r));
    let r = handle_usage(&msg(b"{}".to_vec(), 0), async { Ok(PluginLookup::NotRegistered) }).await;
    assert!(is_reject(&r));
    assert!(p.published.lock().is_empty());
}

#[tokio::test]
async fn usage_plugin_resolution_retries() {
    let r = handle_usage(&msg(usage_payload(), 0), async { Ok(PluginLookup::NotRegistered) }).await;
    assert!(is_retry(&r));
    let r = handle_usage(&msg(usage_payload(), 0), async { Ok(PluginLookup::ClientMissing) }).await;
    assert!(is_retry(&r));
    let r = handle_usage(&msg(usage_payload(), 0), async { Err("registry down".to_owned()) }).await;
    assert!(is_retry(&r));
}

#[tokio::test]
async fn usage_publish_errors() {
    let p = policy(Err(PublishError::Transient("busy".into())));
    let r = handle_usage(&msg(usage_payload(), 0), async { found_policy(&p) }).await;
    assert!(is_retry(&r));
    let p = policy(Err(PublishError::Permanent("bad".into())));
    let r = handle_usage(&msg(usage_payload(), 500), async { found_policy(&p) }).await;
    assert!(is_reject(&r));
}

#[tokio::test]
async fn usage_handler_uses_policy_gateway() {
    use crate::domain::service::test_support::TestEnv;
    use toolkit_db::outbox::LeasedMessageHandler;
    let env = TestEnv::default_env().await;
    // the static policy accepts usage events
    let h = UsageHandler::new(Arc::clone(&env.deps));
    assert!(is_ok(&h.handle(&msg(usage_payload(), 0)).await));
    assert!(is_reject(&h.handle(&msg(b"x".to_vec(), 0)).await));
    // the test env has no audit plugin: valid events are dropped, invalid rejected
    let a = AuditHandler::new(Arc::clone(&env.deps));
    let r = a.handle(&msg(audit_payload(), 0)).await;
    assert!(is_retry(&r), "types-registry client absent → resolution failure → Retry");
    assert!(is_reject(&a.handle(&msg(b"x".to_vec(), 0)).await));
    env.shutdown().await;
}

// ── audit ──────────────────────────────────────────────────────────────────

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn audit_success() {
    let a = audit(Ok(()));
    let r = handle_audit(&msg(audit_payload(), 0), async { found_audit(&a) }, T).await;
    assert!(is_ok(&r));
    assert_eq!(a.emitted.lock()[0].event_type(), "turn_delete");
}

#[tokio::test]
async fn audit_malformed_rejected_before_lookup() {
    for lookup in [
        Ok(PluginLookup::NotRegistered),
        Ok(PluginLookup::ClientMissing),
        Err("down".to_owned()),
    ] {
        let r = handle_audit(&msg(b"[]".to_vec(), 0), async { lookup }, T).await;
        assert!(is_reject(&r));
    }
}

#[tokio::test]
async fn audit_lookup_outcomes() {
    let r = handle_audit(&msg(audit_payload(), 0), async { Ok(PluginLookup::NotRegistered) }, T).await;
    assert!(is_ok(&r), "dropped");
    let r = handle_audit(&msg(audit_payload(), 0), async { Ok(PluginLookup::ClientMissing) }, T).await;
    assert!(is_retry(&r));
    let r = handle_audit(&msg(audit_payload(), 0), async { Err("down".to_owned()) }, T).await;
    assert!(is_retry(&r));
}

#[tokio::test]
async fn audit_plugin_errors() {
    let a = audit(Err(AuditPluginError::Transient("busy".into())));
    assert!(is_retry(&handle_audit(&msg(audit_payload(), 0), async { found_audit(&a) }, T).await));
    let a = audit(Err(AuditPluginError::PluginTimeout));
    assert!(is_retry(&handle_audit(&msg(audit_payload(), 0), async { found_audit(&a) }, T).await));
    let a = audit(Err(AuditPluginError::Permanent("no".into())));
    assert!(is_reject(&handle_audit(&msg(audit_payload(), 0), async { found_audit(&a) }, T).await));
}

#[tokio::test]
async fn audit_timeout_retries() {
    let a = Arc::new(FakeAudit {
        result: Ok(()),
        delay: Duration::from_secs(5),
        emitted: Mutex::new(Vec::new()),
    });
    let r = handle_audit(
        &msg(audit_payload(), 0),
        async { found_audit(&a) },
        Duration::from_millis(20),
    )
    .await;
    assert!(is_retry(&r));
}

#[tokio::test]
async fn audit_retry_on_last_attempt_rejects() {
    let a = audit(Err(AuditPluginError::Transient("busy".into())));
    let r = handle_audit(&msg(audit_payload(), 118), async { found_audit(&a) }, T).await;
    assert!(is_retry(&r), "119th delivery still retries");
    let r = handle_audit(&msg(audit_payload(), 119), async { found_audit(&a) }, T).await;
    assert!(is_reject(&r), "120th delivery rejects");
    let r = handle_audit(&msg(audit_payload(), 119), async { Ok(PluginLookup::ClientMissing) }, T).await;
    assert!(is_reject(&r));
    // success on the last attempt is still Ok
    let ok = audit(Ok(()));
    assert!(is_ok(&handle_audit(&msg(audit_payload(), 119), async { found_audit(&ok) }, T).await));
}
