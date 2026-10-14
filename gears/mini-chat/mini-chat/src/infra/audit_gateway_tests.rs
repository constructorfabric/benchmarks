//! Audit delivery handler tests: every outcome of the algorithm, the 120th-attempt bound on
//! every retry path, the plugin lookup caching rules, the 30 s plugin timeout and the
//! `mini_chat_audit_emit` result of each delivery. Types-registry, `ClientHub` and the plugin
//! are in-process doubles; no network is used.
//!
//! Slice S2: algorithm `audit-delivery`, state machine `audit-delivery`, definition of done
//! `audit`, algorithm `observability` step `inst-obs-audit-series`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::MiniChatAuditPluginClientV1;
use tokio::time::Instant;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use tracing_test::traced_test;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use super::{AUDIT_PAYLOAD_TYPE, AuditGateway};
use crate::test_support::RecordingRegistry;
use crate::test_support::s2::{
    AuditScript, MetricsHarness, ScriptedAuditPlugin, all_audit_events, audit_instance,
    audit_message, audit_message_of, metrics_harness, turn_event,
};

/// The 120th delivery attempt: `OutboxMessage::attempts` is 0 on the first attempt.
const LAST_ATTEMPT: i16 = 119;
const VENDOR: &str = "constructorfabric";

struct Rig {
    hub: Arc<ClientHub>,
    registry: Arc<RecordingRegistry>,
    metrics: MetricsHarness,
    gateway: AuditGateway,
}

fn rig() -> Rig {
    let registry = RecordingRegistry::new(MockTypesRegistryClient::new());
    let hub = Arc::new(ClientHub::new());
    let dynamic: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(dynamic);
    let metrics = metrics_harness("mini_chat");
    let gateway = AuditGateway::new(
        Arc::clone(&hub),
        VENDOR.to_owned(),
        Arc::clone(&metrics.metrics),
    );
    Rig {
        hub,
        registry,
        metrics,
        gateway,
    }
}

impl Rig {
    /// Register an instance in types-registry only (no `ClientHub` client). Returns its id.
    fn add_instance(&self, segment: &str, vendor: &str, priority: i16) -> String {
        let (id, instance) = audit_instance(segment, vendor, priority);
        self.registry.add_instance(instance);
        id
    }

    fn register_client(&self, id: &str, plugin: &Arc<ScriptedAuditPlugin>) {
        let client: Arc<dyn MiniChatAuditPluginClientV1> = plugin.clone();
        self.hub
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(id), client);
    }

    /// A registered instance of the default vendor with a working client.
    fn add_plugin(&self, script: AuditScript) -> (Arc<ScriptedAuditPlugin>, String) {
        let id = self.add_instance("acme.core._.audit_a.v1", VENDOR, 100);
        let plugin = ScriptedAuditPlugin::new(script);
        self.register_client(&id, &plugin);
        (plugin, id)
    }

    async fn deliver(&self, event: &mini_chat_sdk::AuditEvent, attempts: i16) -> MessageResult {
        self.gateway
            .handle(&audit_message_of(event, attempts))
            .await
    }

    async fn deliver_raw(&self, payload: &[u8], attempts: i16) -> MessageResult {
        self.gateway
            .handle(&audit_message(payload.to_vec(), attempts))
            .await
    }
}

fn event() -> mini_chat_sdk::AuditEvent {
    mini_chat_sdk::AuditEvent::TurnCompleted(turn_event(Uuid::from_u128(0xC4A7)))
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

#[test]
fn the_payload_type_is_a_valid_outbox_payload_type() {
    assert!(!AUDIT_PAYLOAD_TYPE.is_empty() && AUDIT_PAYLOAD_TYPE.len() <= 1024);
    assert!(AUDIT_PAYLOAD_TYPE.chars().all(|c| c.is_ascii_graphic()));
}

// ── success ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_event_type_is_delivered_to_the_plugin_and_acknowledged() {
    let rig = rig();
    let (plugin, _) = rig.add_plugin(AuditScript::Accept);
    let events = all_audit_events(Uuid::from_u128(0xC4A7));
    for event in &events {
        let result = rig.deliver(event, 0).await;
        assert!(is_ok(&result), "{event:?}: {result:?}");
    }
    assert_eq!(
        plugin.received(),
        events,
        "delivered unchanged and in order"
    );
    assert_eq!(rig.metrics.audit_emit("ok"), 5);
    assert_eq!(rig.metrics.audit_emit_total(), 5);
}

#[tokio::test]
async fn a_later_attempt_that_the_plugin_accepts_is_acknowledged() {
    let rig = rig();
    let (plugin, _) = rig.add_plugin(AuditScript::Transient);
    assert!(is_retry(&rig.deliver(&event(), 0).await));
    plugin.set_script(AuditScript::Accept);
    assert!(is_ok(&rig.deliver(&event(), 7).await));
    assert_eq!(rig.metrics.audit_emit("retry"), 1);
    assert_eq!(rig.metrics.audit_emit("ok"), 1);
}

// ── corrupt payload ──────────────────────────────────────────────────────────

fn corrupt_payloads() -> Vec<Vec<u8>> {
    vec![
        b"not json".to_vec(),
        Vec::new(),
        b"{}".to_vec(),
        b"[]".to_vec(),
        br#"{"event_type":"turn_completed"}"#.to_vec(),
        br#"{"event_type":"turn_cancelled","chat_id":"x"}"#.to_vec(),
        vec![0xff, 0xfe, 0x00],
    ]
}

#[tokio::test]
async fn a_corrupt_payload_is_rejected_before_the_plugin_is_looked_up() {
    let rig = rig();
    let (plugin, _) = rig.add_plugin(AuditScript::Accept);
    let corrupt = corrupt_payloads();
    for payload in &corrupt {
        for attempts in [0, 5, LAST_ATTEMPT] {
            let result = rig.deliver_raw(payload, attempts).await;
            assert!(
                is_reject(&result),
                "{payload:?} attempt {attempts}: {result:?}"
            );
        }
    }
    assert_eq!(plugin.call_count(), 0);
    assert_eq!(
        rig.registry.list_call_count(),
        0,
        "the payload is checked first"
    );
    assert_eq!(
        rig.metrics.audit_emit("reject"),
        u64::try_from(corrupt.len() * 3).unwrap()
    );
    assert_eq!(
        rig.metrics.audit_emit("ok") + rig.metrics.audit_emit("dropped"),
        0
    );
}

#[tokio::test]
async fn a_corrupt_payload_is_rejected_when_no_plugin_is_registered() {
    let rig = rig();
    let result = rig.deliver_raw(b"garbage", 0).await;
    assert!(
        is_reject(&result),
        "not acknowledged as dropped: {result:?}"
    );
    assert_eq!(rig.metrics.audit_emit("reject"), 1);
    assert_eq!(rig.metrics.audit_emit("dropped"), 0);
    assert_eq!(rig.registry.list_call_count(), 0);
}

#[tokio::test]
async fn a_corrupt_payload_is_rejected_even_when_the_plugin_cannot_be_resolved() {
    let rig = rig();
    rig.registry
        .fail_list
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = rig.deliver_raw(b"garbage", 0).await;
    assert!(is_reject(&result), "not retried: {result:?}");
}

// ── no plugin ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn without_a_plugin_the_event_is_acknowledged_and_counted_as_dropped() {
    let rig = rig();
    let result = rig.deliver(&event(), 0).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(rig.metrics.audit_emit("dropped"), 1);
    assert_eq!(rig.metrics.audit_emit_total(), 1);
}

#[tokio::test]
async fn no_plugin_is_never_cached_so_a_plugin_registered_later_is_used() {
    let rig = rig();
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(
        rig.registry.list_call_count(),
        2,
        "each delivery looks up again"
    );
    assert_eq!(rig.metrics.audit_emit("dropped"), 2);

    let (plugin, _) = rig.add_plugin(AuditScript::Accept);
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(plugin.call_count(), 1);
    assert_eq!(rig.metrics.audit_emit("ok"), 1);
    assert_eq!(rig.metrics.audit_emit("dropped"), 2);
}

#[tokio::test]
async fn a_retry_that_finds_no_plugin_any_more_is_acknowledged_as_dropped() {
    // RETRYING -> DROPPED
    let rig = rig();
    assert!(is_ok(&rig.deliver(&event(), 42).await));
    assert_eq!(rig.metrics.audit_emit("dropped"), 1);
}

#[traced_test]
#[tokio::test]
async fn the_missing_plugin_warning_is_logged_once_per_period() {
    let rig = rig();
    let warnings = || {
        let count = std::cell::Cell::new(0);
        logs_assert(|lines: &[&str]| {
            count.set(lines.iter().filter(|line| line.contains("WARN")).count());
            Ok(())
        });
        count.get()
    };
    assert_eq!(warnings(), 0);
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(warnings(), 1, "one warning for the first drop");
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(warnings(), 1, "not one per event within the period");
}

// ── plugin selection ─────────────────────────────────────────────────────────

#[tokio::test]
async fn only_instances_of_the_configured_vendor_are_used() {
    let rig = rig();
    let other = rig.add_instance("acme.core._.audit_other.v1", "other-vendor", 1);
    let foreign = ScriptedAuditPlugin::new(AuditScript::Accept);
    rig.register_client(&other, &foreign);

    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(foreign.call_count(), 0);
    assert_eq!(rig.metrics.audit_emit("dropped"), 1);
}

#[tokio::test]
async fn the_lowest_priority_value_wins_among_the_instances_of_the_vendor() {
    let rig = rig();
    let slow = rig.add_instance("acme.core._.audit_slow.v1", VENDOR, 100);
    let fast = rig.add_instance("acme.core._.audit_fast.v1", VENDOR, 50);
    let (slow_plugin, fast_plugin) = (
        ScriptedAuditPlugin::new(AuditScript::Accept),
        ScriptedAuditPlugin::new(AuditScript::Accept),
    );
    rig.register_client(&slow, &slow_plugin);
    rig.register_client(&fast, &fast_plugin);

    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(fast_plugin.call_count(), 1);
    assert_eq!(slow_plugin.call_count(), 0);
}

#[tokio::test]
async fn a_found_instance_id_is_cached() {
    let rig = rig();
    let (plugin, _) = rig.add_plugin(AuditScript::Accept);
    for _ in 0..3 {
        assert!(is_ok(&rig.deliver(&event(), 0).await));
    }
    assert_eq!(plugin.call_count(), 3);
    assert_eq!(
        rig.registry.list_call_count(),
        1,
        "one lookup for three deliveries"
    );
}

// ── resolution error ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_resolution_error_retries_until_the_120th_attempt_and_then_rejects() {
    let rig = rig();
    rig.registry
        .fail_list
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for attempts in [0, 1, 60, LAST_ATTEMPT - 1] {
        let result = rig.deliver(&event(), attempts).await;
        assert!(is_retry(&result), "attempt {attempts}: {result:?}");
    }
    let result = rig.deliver(&event(), LAST_ATTEMPT).await;
    assert!(is_reject(&result), "the 120th attempt: {result:?}");
    assert_eq!(rig.metrics.audit_emit("retry"), 4);
    assert_eq!(rig.metrics.audit_emit("reject"), 1);
}

#[tokio::test]
async fn a_resolution_error_that_clears_lets_the_next_attempt_deliver() {
    let rig = rig();
    let (plugin, _) = rig.add_plugin(AuditScript::Accept);
    rig.registry
        .fail_list
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(is_retry(&rig.deliver(&event(), 0).await));
    rig.registry
        .fail_list
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(is_ok(&rig.deliver(&event(), 1).await));
    assert_eq!(plugin.call_count(), 1);
}

// ── client missing from the ClientHub ────────────────────────────────────────

#[tokio::test]
async fn an_instance_without_a_hub_client_retries_until_the_120th_attempt_and_then_rejects() {
    let rig = rig();
    rig.add_instance("acme.core._.audit_a.v1", VENDOR, 100);
    for attempts in [0, 1, LAST_ATTEMPT - 1] {
        let result = rig.deliver(&event(), attempts).await;
        assert!(is_retry(&result), "attempt {attempts}: {result:?}");
    }
    let result = rig.deliver(&event(), LAST_ATTEMPT).await;
    assert!(is_reject(&result), "the 120th attempt: {result:?}");
    assert_eq!(rig.metrics.audit_emit("retry"), 3);
    assert_eq!(rig.metrics.audit_emit("reject"), 1);
    assert_eq!(rig.metrics.audit_emit("dropped"), 0, "not acknowledged");
}

#[tokio::test]
async fn a_missing_client_resets_the_cached_instance_id() {
    let rig = rig();
    let (plugin, id) = rig.add_plugin(AuditScript::Accept);
    assert!(is_ok(&rig.deliver(&event(), 0).await));
    assert_eq!(rig.registry.list_call_count(), 1, "the id is now cached");

    // the client disappears: the cached id still points at it, the delivery retries
    let removed = rig
        .hub
        .remove_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id));
    assert!(removed.is_some());
    assert!(is_retry(&rig.deliver(&event(), 1).await));
    assert_eq!(rig.registry.list_call_count(), 1, "the cache was used");

    // the client is back: the cache was reset, so the instance is looked up again
    rig.register_client(&id, &plugin);
    assert!(is_ok(&rig.deliver(&event(), 2).await));
    assert_eq!(
        rig.registry.list_call_count(),
        2,
        "the cached instance id was reset by the missing client"
    );
}

// ── plugin errors ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_transient_error_and_a_plugin_timeout_retry_until_the_120th_attempt() {
    for script in [AuditScript::Transient, AuditScript::Timeout] {
        let rig = rig();
        let (plugin, _) = rig.add_plugin(script);
        for attempts in [0, 1, LAST_ATTEMPT - 1] {
            let result = rig.deliver(&event(), attempts).await;
            assert!(
                is_retry(&result),
                "{script:?} attempt {attempts}: {result:?}"
            );
        }
        let result = rig.deliver(&event(), LAST_ATTEMPT).await;
        assert!(
            is_reject(&result),
            "{script:?} the 120th attempt: {result:?}"
        );
        assert_eq!(
            plugin.call_count(),
            4,
            "{script:?}: the plugin is called each time"
        );
        assert_eq!(rig.metrics.audit_emit("retry"), 3, "{script:?}");
        assert_eq!(rig.metrics.audit_emit("reject"), 1, "{script:?}");
    }
}

#[tokio::test]
async fn a_permanent_error_is_rejected_at_once_on_any_attempt() {
    for attempts in [0, 3, LAST_ATTEMPT] {
        let rig = rig();
        let (plugin, _) = rig.add_plugin(AuditScript::Permanent);
        let result = rig.deliver(&event(), attempts).await;
        assert!(is_reject(&result), "attempt {attempts}: {result:?}");
        assert_eq!(plugin.call_count(), 1);
        assert_eq!(rig.metrics.audit_emit("reject"), 1);
        assert_eq!(rig.metrics.audit_emit("retry"), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn the_plugin_call_times_out_after_30_seconds_and_counts_as_transient() {
    let rig = rig();
    let (plugin, _) = rig.add_plugin(AuditScript::Hang);

    let started = Instant::now();
    let result = rig.deliver(&event(), 0).await;
    assert!(is_retry(&result), "{result:?}");
    assert_eq!(started.elapsed(), Duration::from_secs(30));

    let started = Instant::now();
    let result = rig.deliver(&event(), LAST_ATTEMPT).await;
    assert!(is_reject(&result), "{result:?}");
    assert_eq!(started.elapsed(), Duration::from_secs(30));

    assert_eq!(plugin.call_count(), 2);
    assert_eq!(rig.metrics.audit_emit("retry"), 1);
    assert_eq!(rig.metrics.audit_emit("reject"), 1);
}

// ── mini_chat_audit_emit ─────────────────────────────────────────────────────

#[tokio::test]
async fn each_delivery_records_exactly_one_result_and_nothing_else() {
    let rig = rig();
    // dropped
    rig.deliver(&event(), 0).await;
    // corrupt -> reject
    rig.deliver_raw(b"bad", 0).await;
    // ok
    let (plugin, _) = rig.add_plugin(AuditScript::Accept);
    rig.deliver(&event(), 0).await;
    // retry
    plugin.set_script(AuditScript::Transient);
    rig.deliver(&event(), 0).await;
    // permanent -> reject
    plugin.set_script(AuditScript::Permanent);
    rig.deliver(&event(), 0).await;

    assert_eq!(rig.metrics.audit_emit("dropped"), 1);
    assert_eq!(rig.metrics.audit_emit("ok"), 1);
    assert_eq!(rig.metrics.audit_emit("retry"), 1);
    assert_eq!(rig.metrics.audit_emit("reject"), 2);
    assert_eq!(
        rig.metrics.audit_emit_total(),
        5,
        "one counter per delivery"
    );
}
