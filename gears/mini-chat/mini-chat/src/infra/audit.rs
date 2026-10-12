//! Audit gateway (DESIGN 3.2 "Audit plugin and audit outbox", ADR-0009).
//!
//! Delivers one audit event to the audit plugin of the configured vendor and
//! reports an outcome the outbox handler maps to ack / retry / reject:
//!
//! | Situation | Outcome |
//! |---|---|
//! | plugin accepted | `Delivered` |
//! | no plugin instance for the vendor | `Dropped` (not cached: asked again next time) |
//! | registry client missing, listing failed, instance without a hub client, transient error, timeout | `Retry` |
//! | permanent plugin error | `Reject` |
//!
//! A found instance id is cached; it is reset when its client is missing.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use tokio::sync::Mutex;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Plugin call timeout (DESIGN B.9.3).
pub const AUDIT_PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of one delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDeliveryOutcome {
    /// The plugin accepted the event.
    Delivered,
    /// No plugin is registered; the event is acknowledged and dropped.
    Dropped,
    /// Delivery failed transiently; the event is retried.
    Retry,
    /// Delivery failed permanently; the event is rejected (dead-lettered).
    Reject,
}

/// Gateway to the audit plugin.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    instance_id: Mutex<Option<String>>,
}

impl AuditGateway {
    /// Gateway that resolves the plugin of `vendor` through the types-registry.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        Self {
            hub,
            vendor: vendor.into(),
            instance_id: Mutex::new(None),
        }
    }

    /// Looks up the plugin instance id, using the cached one when present.
    async fn instance_id(&self) -> Result<String, AuditDeliveryOutcome> {
        let mut cached = self.instance_id.lock().await;
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let registry = self.hub.get::<dyn TypesRegistryClient>().map_err(|e| {
            tracing::warn!(error = %e, "audit: types-registry client unavailable");
            AuditDeliveryOutcome::Retry
        })?;
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "audit: types-registry listing failed");
                AuditDeliveryOutcome::Retry
            })?;
        match choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        ) {
            Ok(id) => {
                *cached = Some(id.clone());
                Ok(id)
            }
            Err(ChoosePluginError::PluginNotFound { .. }) => {
                tracing::warn!(vendor = %self.vendor, "audit: no audit plugin registered, event dropped");
                Err(AuditDeliveryOutcome::Dropped)
            }
            Err(e) => {
                tracing::warn!(error = %e, "audit: plugin selection failed");
                Err(AuditDeliveryOutcome::Retry)
            }
        }
    }

    async fn reset_instance(&self) {
        *self.instance_id.lock().await = None;
    }

    /// Resolves the plugin client, resetting the cached instance when the hub
    /// does not know it.
    async fn client(&self) -> Result<Arc<dyn MiniChatAuditPluginClientV1>, AuditDeliveryOutcome> {
        let id = self.instance_id().await?;
        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        {
            return Ok(client);
        }
        tracing::warn!(instance = %id, "audit: plugin instance has no client in ClientHub");
        self.reset_instance().await;
        Err(AuditDeliveryOutcome::Retry)
    }

    /// Delivers one event.
    pub async fn deliver(&self, event: &AuditEvent) -> AuditDeliveryOutcome {
        let client = match self.client().await {
            Ok(client) => client,
            Err(outcome) => return outcome,
        };
        match tokio::time::timeout(AUDIT_PLUGIN_TIMEOUT, client.emit(event.clone())).await {
            Err(_) => {
                tracing::warn!(event_type = event.event_type(), "audit: plugin timed out");
                AuditDeliveryOutcome::Retry
            }
            Ok(result) => outcome_of(event, result),
        }
    }
}

fn outcome_of(event: &AuditEvent, result: Result<(), AuditPluginError>) -> AuditDeliveryOutcome {
    match result {
        Ok(()) => AuditDeliveryOutcome::Delivered,
        Err(AuditPluginError::Permanent(reason)) => {
            tracing::error!(
                event_type = event.event_type(),
                reason = %reason,
                "audit: plugin rejected the event"
            );
            AuditDeliveryOutcome::Reject
        }
        Err(e) => {
            tracing::warn!(error = %e, "audit: transient plugin error");
            AuditDeliveryOutcome::Retry
        }
    }
}
