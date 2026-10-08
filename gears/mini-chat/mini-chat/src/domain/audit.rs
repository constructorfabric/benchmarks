//! Audit gateway + the usage and audit outbox handlers.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use mini_chat_sdk::{AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, UsageEvent};
use toolkit::client_hub::ClientHub;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::policy::{PluginResolveError, PluginResolver, PolicyGateway};
use crate::infra::metrics::Metrics;

/// Audit delivery attempts before a retried event is dead-lettered.
pub const AUDIT_MAX_ATTEMPTS: i16 = 120;
/// Audit plugin call timeout.
pub const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between "no audit plugin" warnings.
const NO_PLUGIN_WARN_EVERY_SECS: i64 = 300;

/// Delivery outcome of one audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDelivery {
    Ok,
    Dropped,
    Retry,
    Reject,
}

/// Audit gateway: resolves the audit plugin lazily; "no plugin" is not cached.
pub struct AuditGateway {
    resolver: PluginResolver,
    last_no_plugin_warn: AtomicI64,
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { resolver: PluginResolver::new(hub, vendor), last_no_plugin_warn: AtomicI64::new(0) }
    }

    /// Deliver one event.
    #[allow(clippy::cognitive_complexity, reason = "plugin resolution and delivery outcome mapping")]
    pub async fn deliver(&self, event: AuditEvent) -> AuditDelivery {
        let plugin = match self
            .resolver
            .get::<MiniChatAuditPluginSpecV1, dyn MiniChatAuditPluginClientV1>()
            .await
        {
            Ok(p) => p,
            Err(PluginResolveError::NotRegistered) => {
                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                let last = self.last_no_plugin_warn.load(Ordering::Relaxed);
                if now - last >= NO_PLUGIN_WARN_EVERY_SECS {
                    self.last_no_plugin_warn.store(now, Ordering::Relaxed);
                    tracing::warn!("no audit plugin registered; audit events are dropped");
                }
                return AuditDelivery::Dropped;
            }
            Err(e) => {
                tracing::warn!(error = %e, "audit plugin resolution failed");
                return AuditDelivery::Retry;
            }
        };
        match tokio::time::timeout(AUDIT_TIMEOUT, plugin.emit(event)).await {
            Ok(Ok(())) => AuditDelivery::Ok,
            Ok(Err(AuditPluginError::Permanent(e))) => {
                tracing::error!(error = %e, "audit plugin permanent error");
                AuditDelivery::Reject
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "audit plugin transient error");
                AuditDelivery::Retry
            }
            Err(_) => {
                tracing::warn!("audit plugin timeout");
                AuditDelivery::Retry
            }
        }
    }
}

/// Outbox handler of the audit queue.
pub struct AuditHandler {
    pub gateway: Arc<AuditGateway>,
    pub metrics: Arc<Metrics>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                self.metrics.audit_emit("reject");
                return MessageResult::Reject(format!("corrupt audit payload: {e}"));
            }
        };
        match self.gateway.deliver(event).await {
            AuditDelivery::Ok => {
                self.metrics.audit_emit("ok");
                MessageResult::Ok
            }
            AuditDelivery::Dropped => {
                self.metrics.audit_emit("dropped");
                MessageResult::Ok
            }
            AuditDelivery::Reject => {
                self.metrics.audit_emit("reject");
                MessageResult::Reject("permanent audit plugin error".to_owned())
            }
            AuditDelivery::Retry => {
                if msg.attempts + 1 >= AUDIT_MAX_ATTEMPTS {
                    self.metrics.audit_emit("reject");
                    MessageResult::Reject(format!("audit delivery gave up after {AUDIT_MAX_ATTEMPTS} attempts"))
                } else {
                    self.metrics.audit_emit("retry");
                    MessageResult::Retry
                }
            }
        }
    }
}

/// Outbox handler of the usage queue (publishes to the model policy plugin).
pub struct UsageHandler {
    pub policy: Arc<PolicyGateway>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("corrupt usage payload: {e}")),
        };
        match self.policy.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(true) => MessageResult::Retry,
            Err(false) => MessageResult::Reject("permanent usage publish error".to_owned()),
        }
    }
}
