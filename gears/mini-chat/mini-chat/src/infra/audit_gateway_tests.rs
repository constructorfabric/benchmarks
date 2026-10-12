#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Unit tests of the audit gateway decision logic (DESIGN section 3.2 "Audit
//! plugin and audit outbox", section 5.6).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, TurnMutationAuditEvent,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{AUDIT_PLUGIN_TIMEOUT, AuditDelivery, AuditGateway, AuditPluginLocator};
use crate::infra::plugins::static_audit::StaticAuditService;
use crate::infra::policy_gateway::{PluginLocator, PluginLookupError};

type AuditClient = dyn MiniChatAuditPluginClientV1;

const INSTANCE_ID: &str = "audit-instance-1";

/// Locator with a scripted lookup result and client; counts lookups.
struct FakeLocator {
    lookup: Mutex<Result<String, PluginLookupError>>,
    client: Mutex<Option<Arc<AuditClient>>>,
    lookups: AtomicUsize,
}

impl FakeLocator {
    fn new(
        lookup: Result<String, PluginLookupError>,
        client: Option<Arc<AuditClient>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lookup: Mutex::new(lookup),
            client: Mutex::new(client),
            lookups: AtomicUsize::new(0),
        })
    }

    fn set_lookup(&self, lookup: Result<String, PluginLookupError>) {
        *self.lookup.lock().unwrap() = lookup;
    }

    fn set_client(&self, client: Option<Arc<AuditClient>>) {
        *self.client.lock().unwrap() = client;
    }

    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PluginLocator<AuditClient> for FakeLocator {
    async fn find_instance(&self) -> Result<String, PluginLookupError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.lookup.lock().unwrap().clone()
    }

    fn client(&self, instance_id: &str) -> Option<Arc<AuditClient>> {
        assert_eq!(instance_id, INSTANCE_ID);
        self.client.lock().unwrap().clone()
    }
}

/// Plugin with a scripted `emit` behavior.
enum Behavior {
    Fail(MiniChatAuditPluginError),
    Hang,
}

struct ScriptedPlugin(Behavior);

#[async_trait]
impl MiniChatAuditPluginClientV1 for ScriptedPlugin {
    async fn emit(&self, _event: AuditEvent) -> Result<(), MiniChatAuditPluginError> {
        match &self.0 {
            Behavior::Fail(err) => Err(err.clone()),
            Behavior::Hang => futures::future::pending().await,
        }
    }
}

fn not_registered() -> PluginLookupError {
    PluginLookupError::NotRegistered {
        plugin: "audit",
        vendor: "acme".to_owned(),
    }
}

fn found() -> String {
    INSTANCE_ID.to_owned()
}

fn logging_plugin() -> Arc<AuditClient> {
    Arc::new(StaticAuditService::new(true))
}

fn gateway_with_timeout(locator: &Arc<FakeLocator>, timeout: Duration) -> AuditGateway {
    let locator: Arc<AuditPluginLocator> = Arc::clone(locator) as _;
    AuditGateway::with_locator(locator, timeout)
}

fn gateway(locator: &Arc<FakeLocator>) -> AuditGateway {
    gateway_with_timeout(locator, AUDIT_PLUGIN_TIMEOUT)
}

fn event() -> AuditEvent {
    TurnMutationAuditEvent::delete(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Uuid::from_u128(3),
        Uuid::from_u128(4),
        OffsetDateTime::UNIX_EPOCH,
    )
    .into()
}

#[test]
fn audit_plugin_timeout_is_thirty_seconds() {
    assert_eq!(AUDIT_PLUGIN_TIMEOUT, Duration::from_secs(30));
}

#[tokio::test]
async fn delivered_on_success_and_instance_id_is_cached() {
    let locator = FakeLocator::new(Ok(found()), Some(logging_plugin()));
    let gw = gateway(&locator);

    assert_eq!(gw.deliver(event()).await, AuditDelivery::Delivered);
    assert_eq!(gw.deliver(event()).await, AuditDelivery::Delivered);
    assert_eq!(locator.lookups(), 1);
}

#[tokio::test]
async fn disabled_static_plugin_still_acknowledges() {
    let plugin: Arc<AuditClient> = Arc::new(StaticAuditService::new(false));
    let locator = FakeLocator::new(Ok(found()), Some(plugin));
    let gw = gateway(&locator);

    assert_eq!(gw.deliver(event()).await, AuditDelivery::Delivered);
}

#[tokio::test]
async fn no_plugin_is_dropped_not_cached_and_warned_once_per_period() {
    let locator = FakeLocator::new(Err(not_registered()), None);
    let gw = gateway(&locator);

    assert_eq!(gw.deliver(event()).await, AuditDelivery::Dropped);
    assert!(gw.missing_plugin_warned.load(Ordering::SeqCst));
    assert_eq!(gw.deliver(event()).await, AuditDelivery::Dropped);
    assert_eq!(locator.lookups(), 2, "no plugin is not cached");

    locator.set_lookup(Ok(found()));
    locator.set_client(Some(logging_plugin()));
    assert_eq!(gw.deliver(event()).await, AuditDelivery::Delivered);
    assert!(
        !gw.missing_plugin_warned.load(Ordering::SeqCst),
        "a found plugin ends the period without a plugin"
    );
}

#[tokio::test]
async fn lookup_failures_are_retried() {
    for err in [
        PluginLookupError::Unavailable("registry down".to_owned()),
        PluginLookupError::InvalidInstance("bad instance".to_owned()),
    ] {
        let locator = FakeLocator::new(Err(err.clone()), Some(logging_plugin()));
        let gw = gateway(&locator);

        assert_eq!(
            gw.deliver(event()).await,
            AuditDelivery::Retry(err.to_string())
        );
    }
}

#[tokio::test]
async fn missing_client_is_retried_and_resets_cached_instance() {
    let locator = FakeLocator::new(Ok(found()), Some(logging_plugin()));
    let gw = gateway(&locator);
    assert_eq!(gw.deliver(event()).await, AuditDelivery::Delivered);

    locator.set_client(None);
    let outcome = gw.deliver(event()).await;
    assert!(
        matches!(&outcome, AuditDelivery::Retry(reason) if reason.contains(INSTANCE_ID)),
        "{outcome:?}"
    );

    locator.set_client(Some(logging_plugin()));
    assert_eq!(gw.deliver(event()).await, AuditDelivery::Delivered);
    assert_eq!(locator.lookups(), 2, "the reset forces a new lookup");
}

#[tokio::test]
async fn transient_plugin_errors_are_retried() {
    for err in [
        MiniChatAuditPluginError::Transient("sink busy".to_owned()),
        MiniChatAuditPluginError::PluginTimeout,
    ] {
        let plugin: Arc<AuditClient> = Arc::new(ScriptedPlugin(Behavior::Fail(err.clone())));
        let locator = FakeLocator::new(Ok(found()), Some(plugin));
        let gw = gateway(&locator);

        assert_eq!(
            gw.deliver(event()).await,
            AuditDelivery::Retry(err.to_string())
        );
    }
}

#[tokio::test]
async fn permanent_plugin_error_is_rejected() {
    let err = MiniChatAuditPluginError::Permanent("schema mismatch".to_owned());
    let plugin: Arc<AuditClient> = Arc::new(ScriptedPlugin(Behavior::Fail(err.clone())));
    let locator = FakeLocator::new(Ok(found()), Some(plugin));
    let gw = gateway(&locator);

    assert_eq!(
        gw.deliver(event()).await,
        AuditDelivery::Reject(err.to_string())
    );
}

#[tokio::test]
async fn plugin_call_timeout_is_retried_as_plugin_timeout() {
    let plugin: Arc<AuditClient> = Arc::new(ScriptedPlugin(Behavior::Hang));
    let locator = FakeLocator::new(Ok(found()), Some(plugin));
    let gw = gateway_with_timeout(&locator, Duration::from_millis(20));

    assert_eq!(
        gw.deliver(event()).await,
        AuditDelivery::Retry(MiniChatAuditPluginError::PluginTimeout.to_string())
    );
}
