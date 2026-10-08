#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, TurnMutationAuditEvent, UsageEvent,
    UserLimits,
};
use toolkit::client_hub::ClientHub;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use super::audit::{AUDIT_MAX_DELIVERIES, AuditHandler};
use super::usage::UsageHandler;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::gateways::audit::{AuditEmitResult, AuditGateway, PluginAuditGateway};
use crate::infra::gateways::model_policy::{
    InProcessModelPolicyGateway, ModelPolicyGateway, PublishOutcome,
};

fn message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "test".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn usage_event() -> UsageEvent {
    serde_json::from_value(serde_json::json!({
        "tenant_id": Uuid::nil(),
        "chat_id": Uuid::nil(),
        "request_id": Uuid::nil(),
        "effective_model": "m",
        "selected_model": "m",
        "terminal_state": "completed",
        "billing_outcome": "completed",
        "usage": null,
        "actual_credits_micro": 0,
        "settlement_method": "actual",
        "policy_version_applied": 1,
        "web_search_calls": 0,
        "code_interpreter_calls": 0,
        "file_search_calls": 0,
        "timestamp": "2026-10-04T00:00:00Z",
        "requester_type": "user",
        "dedupe_key": "k"
    }))
    .unwrap()
}

fn audit_event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::Delete {
        tenant_id: Uuid::nil(),
        actor_user_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        request_id: Uuid::nil(),
        timestamp: "2026-10-04T00:00:00Z".to_owned(),
    })
}

fn is_reject(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Reject(_))
}

// ---- usage ----------------------------------------------------------------

/// Gateway returning a scripted `publish_usage` outcome.
struct ScriptedPolicyGateway {
    outcome: Result<(), PublishOutcome>,
    seen: Mutex<Vec<UsageEvent>>,
}

impl ScriptedPolicyGateway {
    fn new(outcome: Result<(), PublishOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelPolicyGateway for ScriptedPolicyGateway {
    async fn current_snapshot(&self, _user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>> {
        Err(DomainError::internal("unused"))
    }
    async fn snapshot(&self, _user_id: Uuid, _version: u64) -> DomainResult<Arc<PolicySnapshot>> {
        Err(DomainError::internal("unused"))
    }
    async fn user_limits(&self, _user_id: Uuid, _version: u64) -> DomainResult<UserLimits> {
        Err(DomainError::internal("unused"))
    }
    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome> {
        self.seen.lock().unwrap().push(ev);
        self.outcome.clone()
    }
}

/// Plugin whose `publish_usage` fails with a scripted error.
struct PublishPlugin(Result<(), PublishError>);

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for PublishPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        unimplemented!()
    }
    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        unimplemented!()
    }
    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        unimplemented!()
    }
    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        self.0.clone()
    }
}

fn usage_message(attempts: i16) -> OutboxMessage {
    message(serde_json::to_vec(&usage_event()).unwrap(), attempts)
}

#[tokio::test]
async fn usage_handler_publishes_and_maps_outcomes() {
    // Success: the deserialized event reaches the gateway, the message is acked.
    let gw = ScriptedPolicyGateway::new(Ok(()));
    let handler = UsageHandler::new(gw.clone());
    assert!(matches!(
        handler.handle(&usage_message(0)).await,
        MessageResult::Ok
    ));
    assert_eq!(*gw.seen.lock().unwrap(), vec![usage_event()]);

    // Malformed payloads are rejected without calling the plugin.
    let gw = ScriptedPolicyGateway::new(Ok(()));
    let handler = UsageHandler::new(gw.clone());
    for bad in [&b"not json"[..], b"{}", b"{\"tenant_id\":\"x\"}", b""] {
        assert!(is_reject(&handler.handle(&message(bad.to_vec(), 0)).await));
    }
    assert!(gw.seen.lock().unwrap().is_empty());

    // Gateway-level outcomes.
    let handler = UsageHandler::new(ScriptedPolicyGateway::new(Err(PublishOutcome::Retry(
        "plugin down".to_owned(),
    ))));
    assert!(matches!(
        handler.handle(&usage_message(5)).await,
        MessageResult::Retry
    ));
    let handler = UsageHandler::new(ScriptedPolicyGateway::new(Err(PublishOutcome::Reject(
        "bad".to_owned(),
    ))));
    assert!(is_reject(&handler.handle(&usage_message(0)).await));

    // Plugin-level errors through the real gateway mapping.
    let transient = UsageHandler::new(Arc::new(InProcessModelPolicyGateway::new(Arc::new(
        PublishPlugin(Err(PublishError::Transient("busy".to_owned()))),
    ))));
    assert!(matches!(
        transient.handle(&usage_message(0)).await,
        MessageResult::Retry
    ));
    let permanent = UsageHandler::new(Arc::new(InProcessModelPolicyGateway::new(Arc::new(
        PublishPlugin(Err(PublishError::Permanent("nope".to_owned()))),
    ))));
    assert!(is_reject(&permanent.handle(&usage_message(0)).await));
    let ok = UsageHandler::new(Arc::new(InProcessModelPolicyGateway::new(Arc::new(
        PublishPlugin(Ok(())),
    ))));
    assert!(matches!(
        ok.handle(&usage_message(0)).await,
        MessageResult::Ok
    ));
}

#[tokio::test]
async fn usage_handler_retries_without_attempt_limit() {
    let handler = UsageHandler::new(ScriptedPolicyGateway::new(Err(PublishOutcome::Retry(
        "down".to_owned(),
    ))));
    assert!(matches!(
        handler.handle(&usage_message(i16::MAX)).await,
        MessageResult::Retry
    ));
}

// ---- audit ----------------------------------------------------------------

/// Audit gateway returning a scripted result and counting calls.
struct ScriptedAudit {
    result: AuditEmitResult,
    calls: AtomicUsize,
}

impl ScriptedAudit {
    fn new(result: AuditEmitResult) -> Arc<Self> {
        Arc::new(Self {
            result,
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuditGateway for ScriptedAudit {
    async fn emit(&self, _ev: MiniChatAuditEvent) -> AuditEmitResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

fn audit_message(attempts: i16) -> OutboxMessage {
    message(serde_json::to_vec(&audit_event()).unwrap(), attempts)
}

/// Gateway over a types-registry without any plugin instance.
fn gateway_without_plugin() -> Arc<PluginAuditGateway> {
    let hub = Arc::new(ClientHub::new());
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    hub.register::<dyn TypesRegistryClient>(registry);
    Arc::new(PluginAuditGateway::new(hub, "acme".to_owned()))
}

#[tokio::test]
async fn audit_handler_rejects_bad_payload_even_without_plugin() {
    let scripted = ScriptedAudit::new(AuditEmitResult::Delivered);
    let handler = AuditHandler::new(scripted.clone());
    for bad in [&b"garbage"[..], b"{}", b"[]", b""] {
        assert!(is_reject(&handler.handle(&message(bad.to_vec(), 0)).await));
    }
    assert_eq!(scripted.calls(), 0);

    // No plugin registered: a malformed payload is still rejected, not dropped.
    let handler = AuditHandler::new(gateway_without_plugin());
    assert!(is_reject(
        &handler.handle(&message(b"garbage".to_vec(), 0)).await
    ));
}

#[tokio::test]
async fn audit_handler_drops_when_no_plugin() {
    let handler = AuditHandler::new(gateway_without_plugin());
    assert!(matches!(
        handler.handle(&audit_message(0)).await,
        MessageResult::Ok
    ));
}

#[tokio::test]
async fn audit_handler_maps_gateway_results() {
    let scripted = ScriptedAudit::new(AuditEmitResult::Delivered);
    let handler = AuditHandler::new(scripted.clone());
    assert!(matches!(
        handler.handle(&audit_message(0)).await,
        MessageResult::Ok
    ));
    assert_eq!(scripted.calls(), 1);

    let handler = AuditHandler::new(ScriptedAudit::new(AuditEmitResult::Dropped));
    assert!(matches!(
        handler.handle(&audit_message(0)).await,
        MessageResult::Ok
    ));

    let handler = AuditHandler::new(ScriptedAudit::new(AuditEmitResult::Retry("x".to_owned())));
    assert!(matches!(
        handler.handle(&audit_message(0)).await,
        MessageResult::Retry
    ));

    let handler = AuditHandler::new(ScriptedAudit::new(AuditEmitResult::Reject("x".to_owned())));
    assert!(is_reject(&handler.handle(&audit_message(0)).await));
}

#[tokio::test]
async fn audit_handler_120th_retry_becomes_reject() {
    assert_eq!(AUDIT_MAX_DELIVERIES, 120);
    let handler = AuditHandler::new(ScriptedAudit::new(AuditEmitResult::Retry(
        "plugin timeout".to_owned(),
    )));
    assert!(matches!(
        handler.handle(&audit_message(0)).await,
        MessageResult::Retry
    ));
    // `attempts` is 0-based: 118 is the 119th delivery, 119 the 120th.
    assert!(matches!(
        handler.handle(&audit_message(118)).await,
        MessageResult::Retry
    ));
    for attempts in [119, 120, i16::MAX] {
        match handler.handle(&audit_message(attempts)).await {
            MessageResult::Reject(reason) => {
                assert!(reason.contains("plugin timeout"), "{reason}");
            }
            other => panic!("attempts={attempts}: expected Reject, got {other:?}"),
        }
    }

    // Delivery past the limit is still acknowledged.
    let handler = AuditHandler::new(ScriptedAudit::new(AuditEmitResult::Delivered));
    assert!(matches!(
        handler.handle(&audit_message(500)).await,
        MessageResult::Ok
    ));
}

// ---- pipeline wiring --------------------------------------------------------

#[tokio::test]
async fn pipeline_routes_usage_and_audit_queues_to_the_real_handlers() {
    use super::{OutboxEnqueuer, OutboxHandlers, QueueKind, start_outbox};
    use crate::config::MiniChatConfig;
    use crate::infra::db::test_db;

    let db = test_db().await;
    let cfg = MiniChatConfig::default();
    let policy = ScriptedPolicyGateway::new(Ok(()));
    let audit = ScriptedAudit::new(AuditEmitResult::Delivered);
    let handle = start_outbox(
        db.clone(),
        &cfg,
        OutboxHandlers::with_gateways(policy.clone(), audit.clone()),
    )
    .await
    .unwrap();
    let enq = OutboxEnqueuer::new(cfg.outbox.clone());
    enq.set_outbox(Arc::clone(handle.outbox()));

    let conn = db.conn().unwrap();
    let tenant = Uuid::new_v4();
    enq.enqueue_json(&conn, QueueKind::Usage, tenant, "usage", &usage_event())
        .await
        .unwrap()
        .fire();
    enq.enqueue_json(&conn, QueueKind::Audit, tenant, "audit", &audit_event())
        .await
        .unwrap()
        .fire();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while (policy.seen.lock().unwrap().is_empty() || audit.calls() == 0)
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    handle.stop().await;
    assert_eq!(*policy.seen.lock().unwrap(), vec![usage_event()]);
    assert_eq!(audit.calls(), 1);
}

// ---- provider gate ----------------------------------------------------------

/// Handler counting its calls; acknowledges every message.
#[derive(Default)]
struct CountingHandler(AtomicUsize);

#[async_trait]
impl LeasedMessageHandler for CountingHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        MessageResult::Ok
    }
}

fn counting_handlers() -> (Arc<CountingHandler>, super::pipeline::OutboxHandlers) {
    let counter = Arc::new(CountingHandler::default());
    let shared: Arc<dyn LeasedMessageHandler> = counter.clone();
    let handlers = super::pipeline::OutboxHandlers::placeholders().map(|_, _| Arc::clone(&shared));
    (counter, handlers)
}

#[tokio::test]
async fn provider_queues_wait_for_the_providers_ready_gate() {
    let (counter, handlers) = counting_handlers();
    let (ready, rx) = tokio::sync::watch::channel(false);
    let handlers = handlers.gate_provider_queues(&rx);

    // Usage and audit need no provider call: handled at once.
    assert!(matches!(
        handlers.usage.handle(&message(Vec::new(), 0)).await,
        MessageResult::Ok
    ));
    assert!(matches!(
        handlers.audit.handle(&message(Vec::new(), 0)).await,
        MessageResult::Ok
    ));
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);

    let gated = [
        Arc::clone(&handlers.attachment_cleanup),
        Arc::clone(&handlers.chat_cleanup),
        Arc::clone(&handlers.thread_summary),
    ];
    let waiting: Vec<_> = gated
        .into_iter()
        .map(|h| tokio::spawn(async move { h.handle(&message(Vec::new(), 0)).await }))
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        waiting.iter().all(|t| !t.is_finished()),
        "held until the gate opens"
    );
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);

    ready.send_replace(true);
    for t in waiting {
        assert!(matches!(t.await.unwrap(), MessageResult::Ok));
    }
    assert_eq!(counter.0.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn gated_queue_retries_without_handling_when_the_gate_is_dropped_closed() {
    let (counter, handlers) = counting_handlers();
    let (ready, rx) = tokio::sync::watch::channel(false);
    let handlers = handlers.gate_provider_queues(&rx);
    drop(ready);
    let r = handlers.chat_cleanup.handle(&message(Vec::new(), 0)).await;
    assert!(matches!(r, MessageResult::Retry));
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
}
