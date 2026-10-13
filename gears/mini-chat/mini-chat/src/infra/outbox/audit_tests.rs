#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`: the outcome
//! of every delivery and the `mini_chat_audit_emit{result}` it counts.
//!
//! The 120th delivery attempt is `attempts == 119` (`attempts` is 0 on the
//! first delivery).

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::MiniChatAuditPluginClientV1;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use tracing::Level;

use super::{AUDIT_MAX_DELIVERY_ATTEMPTS, AuditHandler};
use crate::infra::metrics::AuditEmitResult;
use crate::test_doubles::{
    EmitBehavior, FakeAuditPlugin, LogCapture, PluginWorld, RecordingAuditMetrics, audit_event,
    audit_payload, outbox_message,
};

struct Fixture {
    world: PluginWorld,
    plugin: Arc<FakeAuditPlugin>,
    metrics: Arc<RecordingAuditMetrics>,
}

impl Fixture {
    /// A plugin is registered with `behavior`.
    fn with_plugin(behavior: EmitBehavior) -> Self {
        let world = PluginWorld::new();
        let plugin = FakeAuditPlugin::new(behavior);
        world.register_audit(Arc::clone(&plugin) as Arc<dyn MiniChatAuditPluginClientV1>);
        Self {
            world,
            plugin,
            metrics: RecordingAuditMetrics::new(),
        }
    }

    /// Nothing is registered.
    fn without_plugin() -> Self {
        Self {
            world: PluginWorld::new(),
            plugin: FakeAuditPlugin::new(EmitBehavior::Ok),
            metrics: RecordingAuditMetrics::new(),
        }
    }

    fn handler(&self) -> AuditHandler {
        self.handler_with_emit_timeout(Duration::from_millis(50))
    }

    fn handler_with_emit_timeout(&self, timeout: Duration) -> AuditHandler {
        AuditHandler::new(
            Arc::new(self.world.audit_gateway().with_emit_timeout(timeout)),
            Arc::clone(&self.metrics) as Arc<dyn crate::infra::metrics::AuditEmitMetrics>,
        )
    }

    async fn deliver(&self, payload: Vec<u8>, attempts: i16) -> MessageResult {
        self.handler()
            .handle(&outbox_message(payload, attempts))
            .await
    }
}

#[test]
fn the_last_delivery_attempt_is_the_hundred_and_twentieth() {
    assert_eq!(AUDIT_MAX_DELIVERY_ATTEMPTS, 120);
}

// ---- inst-decode-audit .. inst-reject-malformed-audit ---------------------------------------

#[tokio::test]
async fn a_malformed_payload_is_rejected_and_counted_as_reject() {
    let fixture = Fixture::with_plugin(EmitBehavior::Ok);

    let result = fixture.deliver(b"{ not an event".to_vec(), 0).await;

    match result {
        MessageResult::Reject(reason) => assert!(!reason.is_empty()),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
    assert!(fixture.plugin.emitted_events().is_empty());
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_when_no_plugin_is_registered() {
    let fixture = Fixture::without_plugin();

    let result = fixture.deliver(b"[]".to_vec(), 0).await;

    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_before_the_plugin_is_resolved() {
    let fixture = Fixture::without_plugin();
    fixture.world.registry.fail_list("registry down");

    let result = fixture.deliver(b"garbage".to_vec(), 0).await;

    assert!(
        matches!(result, MessageResult::Reject(_)),
        "a registry failure must not turn a corrupt payload into a retry: {result:?}"
    );
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
}

// ---- inst-drop-event ---------------------------------------------------------------------------------

#[tokio::test]
async fn no_plugin_acknowledges_and_counts_dropped() {
    let fixture = Fixture::without_plugin();

    let result = fixture.deliver(audit_payload(), 0).await;

    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Dropped]);
}

#[tokio::test]
async fn the_absence_of_a_plugin_is_warned_once_per_period() {
    let fixture = Fixture::without_plugin();
    let handler = fixture.handler();
    let logs = LogCapture::start();

    for _ in 0..3 {
        let result = handler.handle(&outbox_message(audit_payload(), 0)).await;
        assert!(matches!(result, MessageResult::Ok), "{result:?}");
    }

    assert_eq!(logs.at(Level::WARN).len(), 1, "{:?}", logs.records());
    assert_eq!(
        fixture.metrics.recorded(),
        vec![AuditEmitResult::Dropped; 3],
        "every delivery is counted"
    );
}

#[tokio::test]
async fn a_plugin_registered_later_is_used_on_the_next_delivery() {
    let fixture = Fixture::without_plugin();
    let handler = fixture.handler();
    assert!(matches!(
        handler.handle(&outbox_message(audit_payload(), 0)).await,
        MessageResult::Ok
    ));

    fixture
        .world
        .register_audit(Arc::clone(&fixture.plugin) as Arc<dyn MiniChatAuditPluginClientV1>);
    let result = handler.handle(&outbox_message(audit_payload(), 0)).await;

    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(fixture.plugin.emitted_events(), vec![audit_event()]);
    assert_eq!(
        fixture.metrics.recorded(),
        vec![AuditEmitResult::Dropped, AuditEmitResult::Ok]
    );
}

// ---- inst-if-resolution-failed ------------------------------------------------------------------------

#[tokio::test]
async fn a_resolution_failure_is_retry_counted_as_retry() {
    let fixture = Fixture::with_plugin(EmitBehavior::Ok);
    fixture.world.registry.fail_list("registry down");

    let result = fixture.deliver(audit_payload(), 0).await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Retry]);
    assert!(fixture.plugin.emitted_events().is_empty());
}

#[tokio::test]
async fn an_instance_without_a_client_is_retry_and_recovers_once_the_client_appears() {
    let world = PluginWorld::new();
    world.add_audit_instance();
    let plugin = FakeAuditPlugin::new(EmitBehavior::Ok);
    let metrics = RecordingAuditMetrics::new();
    let handler = AuditHandler::new(
        Arc::new(world.audit_gateway()),
        Arc::clone(&metrics) as Arc<dyn crate::infra::metrics::AuditEmitMetrics>,
    );

    let first = handler.handle(&outbox_message(audit_payload(), 0)).await;
    world.register_audit(Arc::clone(&plugin) as Arc<dyn MiniChatAuditPluginClientV1>);
    let second = handler.handle(&outbox_message(audit_payload(), 1)).await;

    assert!(matches!(first, MessageResult::Retry), "{first:?}");
    assert!(matches!(second, MessageResult::Ok), "{second:?}");
    assert_eq!(
        metrics.recorded(),
        vec![AuditEmitResult::Retry, AuditEmitResult::Ok]
    );
}

// ---- inst-emit / inst-match-emit ------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_emit_is_ok_counted_as_ok_and_hands_the_event_to_the_plugin() {
    let fixture = Fixture::with_plugin(EmitBehavior::Ok);

    let result = fixture.deliver(audit_payload(), 0).await;

    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Ok]);
    assert_eq!(fixture.plugin.emitted_events(), vec![audit_event()]);
}

#[tokio::test]
async fn a_transient_error_is_retry_counted_as_retry() {
    let fixture = Fixture::with_plugin(EmitBehavior::Transient);

    let result = fixture.deliver(audit_payload(), 0).await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Retry]);
}

#[tokio::test]
async fn a_plugin_timeout_is_retry_counted_as_retry() {
    let fixture = Fixture::with_plugin(EmitBehavior::PluginTimeout);

    let result = fixture.deliver(audit_payload(), 0).await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Retry]);
}

#[tokio::test]
async fn an_emit_that_never_answers_is_retry_counted_as_retry() {
    let fixture = Fixture::with_plugin(EmitBehavior::Hang);

    let result = tokio::time::timeout(Duration::from_secs(5), fixture.deliver(audit_payload(), 0))
        .await
        .expect("the emit is bounded");

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Retry]);
}

#[tokio::test(start_paused = true)]
async fn the_default_emit_bound_is_thirty_seconds() {
    let world = PluginWorld::new();
    let plugin = FakeAuditPlugin::new(EmitBehavior::Hang);
    world.register_audit(Arc::clone(&plugin) as Arc<dyn MiniChatAuditPluginClientV1>);
    let metrics = RecordingAuditMetrics::new();
    let handler = AuditHandler::new(
        Arc::new(world.audit_gateway()),
        Arc::clone(&metrics) as Arc<dyn crate::infra::metrics::AuditEmitMetrics>,
    );
    let started = tokio::time::Instant::now();

    let result = handler.handle(&outbox_message(audit_payload(), 0)).await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(30) && elapsed < Duration::from_secs(31),
        "{elapsed:?}"
    );
}

#[tokio::test]
async fn a_permanent_error_is_reject_counted_as_reject() {
    let fixture = Fixture::with_plugin(EmitBehavior::Permanent);

    let result = fixture.deliver(audit_payload(), 0).await;

    match result {
        MessageResult::Reject(reason) => assert!(!reason.is_empty()),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
}

// ---- inst-if-last-attempt ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_retry_before_the_last_attempt_stays_a_retry() {
    for behavior in [
        EmitBehavior::Transient,
        EmitBehavior::PluginTimeout,
        EmitBehavior::Hang,
    ] {
        let fixture = Fixture::with_plugin(behavior);

        let result = fixture.deliver(audit_payload(), 118).await;

        assert!(matches!(result, MessageResult::Retry), "{result:?}");
        assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Retry]);
    }
}

#[tokio::test]
async fn a_retry_on_the_hundred_and_twentieth_attempt_is_a_reject_counted_once_as_reject() {
    for behavior in [
        EmitBehavior::Transient,
        EmitBehavior::PluginTimeout,
        EmitBehavior::Hang,
    ] {
        let fixture = Fixture::with_plugin(behavior);

        let result = fixture.deliver(audit_payload(), 119).await;

        match result {
            MessageResult::Reject(reason) => assert!(!reason.is_empty()),
            other => panic!("expected Reject, got {other:?}"),
        }
        assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
    }
}

#[tokio::test]
async fn attempts_beyond_the_limit_are_rejected_too() {
    let fixture = Fixture::with_plugin(EmitBehavior::Transient);

    let result = fixture.deliver(audit_payload(), 400).await;

    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
}

#[tokio::test]
async fn a_resolution_failure_on_the_last_attempt_is_a_reject() {
    let fixture = Fixture::with_plugin(EmitBehavior::Ok);
    fixture.world.registry.fail_list("registry down");

    let result = fixture.deliver(audit_payload(), 119).await;

    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
}

#[tokio::test]
async fn success_and_drop_on_the_last_attempt_stay_acknowledged() {
    let fixture = Fixture::with_plugin(EmitBehavior::Ok);
    let ok = fixture.deliver(audit_payload(), 119).await;
    assert!(matches!(ok, MessageResult::Ok), "{ok:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Ok]);

    let dropped_fixture = Fixture::without_plugin();
    let dropped = dropped_fixture.deliver(audit_payload(), 119).await;
    assert!(matches!(dropped, MessageResult::Ok), "{dropped:?}");
    assert_eq!(
        dropped_fixture.metrics.recorded(),
        vec![AuditEmitResult::Dropped]
    );
}

#[tokio::test]
async fn a_permanent_error_on_the_last_attempt_is_counted_once() {
    let fixture = Fixture::with_plugin(EmitBehavior::Permanent);

    let result = fixture.deliver(audit_payload(), 119).await;

    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
    assert_eq!(fixture.metrics.recorded(), vec![AuditEmitResult::Reject]);
}
