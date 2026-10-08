//! Audit gateway: delivers audit events to the audit plugin (DESIGN "Audit plugin and audit
//! outbox", ADR-0009).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use toolkit::telemetry::ThrottledLog;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Result of one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    /// The plugin accepted the event.
    Ok,
    /// No audit plugin is registered; the event is dropped.
    Dropped,
    /// Transient failure (plugin error or timeout, resolution failure); redeliver later.
    Retry,
    /// Permanent plugin failure; dead-letter with this reason.
    Reject(String),
}

/// Access to the audit plugin. The caller (the audit outbox handler) bounds the call with its own
/// timeout.
#[async_trait]
pub trait AuditGateway: Send + Sync {
    async fn deliver(&self, ev: AuditEvent) -> AuditDelivery;
}

/// Throttle interval of the "no audit plugin" / "client missing" warnings.
const WARN_THROTTLE: Duration = Duration::from_secs(60);

async fn emit_via(plugin: &dyn MiniChatAuditPluginClientV1, ev: AuditEvent) -> AuditDelivery {
    match plugin.emit(ev).await {
        Ok(()) => AuditDelivery::Ok,
        Err(AuditPluginError::Permanent(reason)) => AuditDelivery::Reject(reason),
        Err(err @ (AuditPluginError::Transient(_) | AuditPluginError::PluginTimeout)) => {
            tracing::warn!(error = %err, "audit plugin failed transiently");
            AuditDelivery::Retry
        }
    }
}

/// [`AuditGateway`] over an in-process plugin client (tests).
pub struct DirectAuditGateway(pub Arc<dyn MiniChatAuditPluginClientV1>);

#[async_trait]
impl AuditGateway for DirectAuditGateway {
    async fn deliver(&self, ev: AuditEvent) -> AuditDelivery {
        emit_via(self.0.as_ref(), ev).await
    }
}

/// Why no plugin client could be obtained.
enum Unresolved {
    /// No instance of the audit plugin type for `vendor`: deliveries are dropped.
    NoPlugin,
    /// types-registry unavailable, list failure or invalid instance: retried.
    Failed(String),
}

/// [`AuditGateway`] resolving the `MiniChatAuditPluginClientV1` instance of `vendor` lazily
/// through types-registry and the `ClientHub`.
///
/// - No plugin registered: [`AuditDelivery::Dropped`]. This is not cached; every delivery looks
///   the plugin up again, so a plugin registered later is used. The warning is throttled.
/// - A found instance id is cached.
/// - The cached instance has no scoped client in the hub: the cache is reset and the delivery is
///   [`AuditDelivery::Retry`]; a registry failure is `Retry` as well.
pub struct PluginAuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    no_plugin_log: ThrottledLog,
    missing_client_log: ThrottledLog,
}

impl PluginAuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            no_plugin_log: ThrottledLog::new(WARN_THROTTLE),
            missing_client_log: ThrottledLog::new(WARN_THROTTLE),
        }
    }

    async fn resolve_instance(&self) -> Result<String, Unresolved> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| Unresolved::Failed(format!("types-registry client: {e}")))?;
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| Unresolved::Failed(format!("list plugin instances: {e}")))?;
        choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => Unresolved::NoPlugin,
            other @ ChoosePluginError::InvalidPluginInstance { .. } => {
                Unresolved::Failed(other.to_string())
            }
        })
    }
}

#[async_trait]
impl AuditGateway for PluginAuditGateway {
    async fn deliver(&self, ev: AuditEvent) -> AuditDelivery {
        let instance_id = match self.selector.get_or_init(|| self.resolve_instance()).await {
            Ok(id) => id,
            Err(Unresolved::NoPlugin) => {
                if self.no_plugin_log.should_log() {
                    tracing::warn!(vendor = %self.vendor, "no audit plugin registered; audit events are dropped");
                }
                return AuditDelivery::Dropped;
            }
            Err(Unresolved::Failed(reason)) => {
                tracing::warn!(vendor = %self.vendor, %reason, "audit plugin resolution failed");
                return AuditDelivery::Retry;
            }
        };
        let Some(plugin) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&instance_id))
        else {
            self.selector.reset().await;
            if self.missing_client_log.should_log() {
                tracing::warn!(plugin_gts_id = %instance_id, "audit plugin client not registered");
            }
            return AuditDelivery::Retry;
        };
        emit_via(plugin.as_ref(), ev).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mini_chat_sdk::{
        AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
        TurnMutationAuditEvent,
    };
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit::gts::PluginV1;
    use toolkit_canonical_errors::CanonicalError;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use types_registry_sdk::{GtsInstance, TypesRegistryClient};
    use uuid::Uuid;

    use super::*;
    use crate::test_support::plugins::RecordingAudit;

    const VENDOR: &str = "constructorfabric";

    fn event() -> AuditEvent {
        AuditEvent::Mutation(TurnMutationAuditEvent {
            event_type: "turn_delete".to_owned(),
            tenant_id: Uuid::from_u128(1),
            actor_user_id: Uuid::from_u128(2),
            chat_id: Uuid::from_u128(3),
            original_request_id: None,
            new_request_id: None,
            request_id: Some(Uuid::from_u128(4)),
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
        })
    }

    /// A registered audit plugin instance of `VENDOR`.
    fn plugin_instance() -> (String, GtsInstance) {
        let (id, json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.test_audit.v1",
            VENDOR,
            10,
        )
        .expect("registration");
        let id = id.to_string();
        let instance = make_test_instance(&id, json);
        (id, instance)
    }

    fn hub_with(registry: &Arc<MockTypesRegistryClient>) -> Arc<ClientHub> {
        let hub = Arc::new(ClientHub::new());
        let registry: Arc<dyn TypesRegistryClient> = registry.clone();
        hub.register::<dyn TypesRegistryClient>(registry);
        hub
    }

    #[tokio::test]
    async fn no_plugin_is_dropped_and_not_cached() {
        let empty = Arc::new(MockTypesRegistryClient::new());
        let hub = hub_with(&empty);
        let gw = PluginAuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

        assert_eq!(gw.deliver(event()).await, AuditDelivery::Dropped);
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Dropped);
        assert_eq!(empty.list_instance_calls(), 2, "no-plugin is not cached");

        // A plugin registered later is picked up by the next delivery.
        let (id, instance) = plugin_instance();
        let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        let registry_dyn: Arc<dyn TypesRegistryClient> = registry.clone();
        hub.register::<dyn TypesRegistryClient>(registry_dyn);
        let audit = Arc::new(RecordingAudit::new());
        let client: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), client);

        assert_eq!(gw.deliver(event()).await, AuditDelivery::Ok);
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Ok);
        assert_eq!(audit.events(), [event(), event()]);
        assert_eq!(registry.list_instance_calls(), 1, "found id is cached");
    }

    #[tokio::test]
    async fn missing_client_resets_cache_and_retries() {
        let (id, instance) = plugin_instance();
        let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        let hub = hub_with(&registry);
        let gw = PluginAuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

        assert_eq!(gw.deliver(event()).await, AuditDelivery::Retry);
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Retry);
        assert_eq!(registry.list_instance_calls(), 2, "cache reset each time");

        let audit = Arc::new(RecordingAudit::new());
        let client: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), client);
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Ok);
        assert_eq!(audit.events().len(), 1);
    }

    #[tokio::test]
    async fn other_vendor_is_dropped_and_resolution_failures_retry() {
        let (_, instance) = plugin_instance();
        let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        let gw = PluginAuditGateway::new(hub_with(&registry), "acme".to_owned());
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Dropped);

        let gw = PluginAuditGateway::new(Arc::new(ClientHub::new()), VENDOR.to_owned());
        assert_eq!(
            gw.deliver(event()).await,
            AuditDelivery::Retry,
            "no types-registry client"
        );

        let failing = Arc::new(
            MockTypesRegistryClient::new()
                .with_list_error(CanonicalError::internal("registry down").create()),
        );
        let gw = PluginAuditGateway::new(hub_with(&failing), VENDOR.to_owned());
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Retry);
    }

    #[tokio::test]
    async fn plugin_errors_map_to_retry_or_reject() {
        let audit = Arc::new(RecordingAudit::new());
        let gw = DirectAuditGateway(audit.clone());
        audit.fail_next(AuditPluginError::Transient("busy".into()));
        audit.fail_next(AuditPluginError::PluginTimeout);
        audit.fail_next(AuditPluginError::Permanent("bad event".into()));

        assert_eq!(gw.deliver(event()).await, AuditDelivery::Retry);
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Retry);
        assert_eq!(
            gw.deliver(event()).await,
            AuditDelivery::Reject("bad event".to_owned())
        );
        assert_eq!(gw.deliver(event()).await, AuditDelivery::Ok);
        assert_eq!(audit.events().len(), 4);
    }
}
