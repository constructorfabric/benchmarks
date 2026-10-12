//! Audit gateway (DESIGN section 3.2 "Audit plugin and audit outbox", section
//! 5.6 "Shared Outbox Processing Model", ADR-0009).
//!
//! [`AuditGateway::deliver`] hands one (already deserialized) audit event to
//! the audit plugin selected for the gear's `vendor` through types-registry
//! and the scoped `ClientHub`, and classifies the outcome for the audit
//! outbox handler as an [`AuditDelivery`]:
//!
//! | Situation | Outcome |
//! |-----------|---------|
//! | the plugin accepted the event | [`AuditDelivery::Delivered`] |
//! | no plugin registered for the vendor | [`AuditDelivery::Dropped`] |
//! | types-registry lookup failed or an instance is malformed | [`AuditDelivery::Retry`] |
//! | instance resolved but its client is missing from `ClientHub` | [`AuditDelivery::Retry`], cached instance id reset |
//! | transient plugin error or the call exceeded [`AUDIT_PLUGIN_TIMEOUT`] (`PluginTimeout`) | [`AuditDelivery::Retry`] |
//! | permanent plugin error | [`AuditDelivery::Reject`] |
//!
//! A found instance id is cached. "No plugin registered" is not cached: every
//! delivery looks the plugin up again, so a plugin registered later is used.
//! The "no plugin" warning is logged once per period without a plugin: the
//! first drop warns, later drops stay silent until a plugin is found again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::ClientHub;
use toolkit::plugins::GtsPluginSelector;
use tracing::warn;

use crate::infra::policy_gateway::{HubPluginLocator, PluginLocator, PluginLookupError};

/// Timeout of one audit plugin call; exceeding it is
/// [`MiniChatAuditPluginError::PluginTimeout`] (transient).
pub const AUDIT_PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

const LOG_TARGET: &str = "mini_chat.audit_gateway";

/// Locator type of the audit plugin.
pub type AuditPluginLocator = dyn PluginLocator<dyn MiniChatAuditPluginClientV1>;

/// Outcome of one audit delivery, mapped by the audit outbox handler to its
/// `Ok` / `Retry` / `Reject` result and to `mini_chat_audit_emit_total{result}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    /// The plugin accepted the event.
    Delivered,
    /// No audit plugin is registered: the event is acknowledged and dropped.
    Dropped,
    /// Delivery may succeed later; the message must not be acknowledged.
    Retry(String),
    /// The plugin rejected the event permanently.
    Reject(String),
}

/// Gateway to the audit plugin selected for the gear's vendor.
pub struct AuditGateway {
    locator: Arc<AuditPluginLocator>,
    selector: GtsPluginSelector,
    timeout: Duration,
    missing_plugin_warned: AtomicBool,
}

impl AuditGateway {
    /// Gateway resolving the audit plugin of `vendor` through types-registry
    /// and the scoped `ClientHub`, with [`AUDIT_PLUGIN_TIMEOUT`].
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        Self::with_locator(
            Arc::new(HubPluginLocator::<
                MiniChatAuditPluginSpecV1,
                dyn MiniChatAuditPluginClientV1,
            >::new(hub, vendor, "audit")),
            AUDIT_PLUGIN_TIMEOUT,
        )
    }

    /// Gateway over an arbitrary [`PluginLocator`] and plugin-call timeout.
    #[must_use]
    pub fn with_locator(locator: Arc<AuditPluginLocator>, timeout: Duration) -> Self {
        Self {
            locator,
            selector: GtsPluginSelector::new(),
            timeout,
            missing_plugin_warned: AtomicBool::new(false),
        }
    }

    /// Delivers one audit event to the selected plugin.
    pub async fn deliver(&self, event: AuditEvent) -> AuditDelivery {
        let plugin = match self.resolve_plugin().await {
            Ok(plugin) => plugin,
            Err(outcome) => return outcome,
        };
        let result = tokio::time::timeout(self.timeout, plugin.emit(event))
            .await
            .unwrap_or(Err(MiniChatAuditPluginError::PluginTimeout));
        match result {
            Ok(()) => AuditDelivery::Delivered,
            Err(err) if err.is_transient() => AuditDelivery::Retry(err.to_string()),
            Err(err) => AuditDelivery::Reject(err.to_string()),
        }
    }

    /// Resolves the plugin client, or the outcome that ends the delivery
    /// without calling a plugin (`Dropped` or `Retry`).
    async fn resolve_plugin(&self) -> Result<Arc<dyn MiniChatAuditPluginClientV1>, AuditDelivery> {
        let instance_id = self
            .selector
            .get_or_init(|| self.locator.find_instance())
            .await
            .map_err(|err| self.lookup_failure(&err))?;
        self.missing_plugin_warned.store(false, Ordering::Relaxed);

        if let Some(plugin) = self.locator.client(&instance_id) {
            return Ok(plugin);
        }
        self.selector.reset().await;
        warn!(
            target: LOG_TARGET,
            instance_id = %instance_id,
            "audit plugin client not registered; cached instance id reset"
        );
        Err(AuditDelivery::Retry(format!(
            "audit plugin client not registered for instance '{instance_id}'"
        )))
    }

    /// Outcome of a failed lookup: `Dropped` (warned once per period without
    /// a plugin) when none is registered, `Retry` otherwise.
    fn lookup_failure(&self, err: &PluginLookupError) -> AuditDelivery {
        match err {
            PluginLookupError::NotRegistered { .. } => {
                if !self.missing_plugin_warned.swap(true, Ordering::Relaxed) {
                    warn!(
                        target: LOG_TARGET,
                        reason = %err,
                        "audit events are dropped until an audit plugin is registered"
                    );
                }
                AuditDelivery::Dropped
            }
            PluginLookupError::Unavailable(_) | PluginLookupError::InvalidInstance(_) => {
                AuditDelivery::Retry(err.to_string())
            }
        }
    }
}

#[cfg(test)]
#[path = "audit_gateway_tests.rs"]
mod tests;
