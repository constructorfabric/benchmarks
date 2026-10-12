//! Audit gateway: resolves the audit plugin through the types registry and delivers audit events.
//!
//! Delivery follows ADR-0009: when no audit plugin is registered the event is acknowledged and
//! dropped (and the lookup is repeated on the next delivery); a plugin that resolves in the types
//! registry but has no client in the `ClientHub` makes the delivery retryable and resets the cached
//! instance id; plugin calls time out after 30 seconds, which is a transient failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, TurnAuditEvent,
    TurnMutationAuditEvent,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::GtsPluginSelector;
use tracing::{debug, warn};

use super::plugin_select::{PluginResolutionError, select_instance};

/// Timeout of one audit plugin call.
const AUDIT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of delivering one audit event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    /// The plugin accepted the event.
    Delivered,
    /// No audit plugin is registered: the event is acknowledged and dropped.
    Dropped,
    /// A transient failure: the event must be redelivered.
    Retry(String),
    /// A permanent failure: the event must be dead-lettered.
    Reject(String),
}

/// Gateway to the selected audit plugin.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    /// The "no audit plugin" warning was logged for the current period without a plugin.
    missing_warned: AtomicBool,
}

impl AuditGateway {
    /// Creates a gateway selecting plugin instances of `vendor`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            missing_warned: AtomicBool::new(false),
        }
    }

    /// Delivers a turn-finalization event.
    pub async fn emit_turn(&self, event: TurnAuditEvent) -> AuditDelivery {
        self.deliver(|plugin| async move { plugin.emit_turn_audit(event).await })
            .await
    }

    /// Delivers a turn-mutation event.
    pub async fn emit_mutation(&self, event: TurnMutationAuditEvent) -> AuditDelivery {
        self.deliver(|plugin| async move { plugin.emit_turn_mutation_audit(event).await })
            .await
    }

    async fn deliver<F, Fut>(&self, call: F) -> AuditDelivery
    where
        F: FnOnce(Arc<dyn MiniChatAuditPluginClientV1>) -> Fut,
        Fut: Future<Output = Result<(), AuditPluginError>>,
    {
        let instance_id = match self
            .selector
            .get_or_init(|| async {
                select_instance::<MiniChatAuditPluginSpecV1>(&self.hub, &self.vendor).await
            })
            .await
        {
            Ok(id) => {
                self.missing_warned.store(false, Ordering::Relaxed);
                id
            }
            Err(PluginResolutionError::NotFound { vendor }) => {
                // Logged once per period without a plugin; every event is still counted by the
                // handler as dropped.
                if !self.missing_warned.swap(true, Ordering::Relaxed) {
                    warn!(vendor = %vendor, "no audit plugin registered; dropping the audit events");
                }
                return AuditDelivery::Dropped;
            }
            Err(PluginResolutionError::RegistryUnavailable(cause)) => {
                return AuditDelivery::Retry(format!("audit plugin resolution failed: {cause}"));
            }
            Err(PluginResolutionError::InvalidInstance(cause)) => {
                return AuditDelivery::Retry(format!("invalid audit plugin instance: {cause}"));
            }
        };
        let Some(plugin) =
            self.hub
                .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(
                    instance_id.as_ref(),
                ))
        else {
            self.selector.reset().await;
            return AuditDelivery::Retry(format!(
                "audit plugin client `{instance_id}` is not registered"
            ));
        };
        match tokio::time::timeout(AUDIT_CALL_TIMEOUT, call(plugin)).await {
            Ok(Ok(())) => {
                debug!("audit event delivered");
                AuditDelivery::Delivered
            }
            Ok(Err(AuditPluginError::Permanent(cause))) => AuditDelivery::Reject(cause),
            Ok(Err(AuditPluginError::Transient(cause))) => AuditDelivery::Retry(cause),
            Ok(Err(AuditPluginError::PluginTimeout)) | Err(_) => {
                AuditDelivery::Retry("audit plugin timed out".to_owned())
            }
        }
    }
}
