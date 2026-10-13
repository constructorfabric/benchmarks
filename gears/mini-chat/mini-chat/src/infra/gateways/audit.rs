//! Audit gateway: delivers audit events to the audit plugin.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::ClientHub;
use toolkit::telemetry::ThrottledLog;
use tracing::warn;

use super::plugin_select::{PluginResolver, ResolveError};

/// Per-delivery timeout of the plugin call.
const EMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum interval between "no audit plugin" warnings.
const NO_PLUGIN_WARN_PERIOD: Duration = Duration::from_secs(60);

/// Outcome of one audit delivery attempt (drives the audit outbox handler).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEmitResult {
    /// The plugin accepted the event.
    Delivered,
    /// No plugin is registered: the event is acknowledged and dropped.
    Dropped,
    /// Not delivered; retry later.
    Retry(String),
    /// Permanently rejected; do not retry.
    Reject(String),
}

/// Port used by the audit outbox handler.
#[async_trait]
pub trait AuditGateway: Send + Sync {
    async fn emit(&self, ev: MiniChatAuditEvent) -> AuditEmitResult;
}

/// Gateway resolving the audit plugin lazily by vendor through the types-registry.
pub struct PluginAuditGateway {
    resolver: PluginResolver<MiniChatAuditPluginSpecV1, dyn MiniChatAuditPluginClientV1>,
    vendor: String,
    timeout: Duration,
    no_plugin_log: ThrottledLog,
}

impl PluginAuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self::with_timeout(hub, vendor, EMIT_TIMEOUT)
    }

    #[must_use]
    pub fn with_timeout(hub: Arc<ClientHub>, vendor: String, timeout: Duration) -> Self {
        Self {
            resolver: PluginResolver::new(
                hub,
                vendor.clone(),
                MiniChatAuditPluginSpecV1::gts_type_id(),
            ),
            vendor,
            timeout,
            no_plugin_log: ThrottledLog::new(NO_PLUGIN_WARN_PERIOD),
        }
    }
}

#[async_trait]
impl AuditGateway for PluginAuditGateway {
    async fn emit(&self, ev: MiniChatAuditEvent) -> AuditEmitResult {
        let client = match self.resolver.client().await {
            Ok(c) => c,
            Err(ResolveError::NotFound { .. }) => {
                if self.no_plugin_log.should_log() {
                    warn!(vendor = %self.vendor, "no audit plugin registered; dropping audit events");
                }
                return AuditEmitResult::Dropped;
            }
            Err(e) => return AuditEmitResult::Retry(e.to_string()),
        };

        emit_via(&*client, self.timeout, ev).await
    }
}

/// Emit through `client` with `timeout` and map the plugin result.
async fn emit_via(
    client: &dyn MiniChatAuditPluginClientV1,
    timeout: Duration,
    ev: MiniChatAuditEvent,
) -> AuditEmitResult {
    match tokio::time::timeout(timeout, client.emit(ev)).await {
        Ok(Ok(())) => AuditEmitResult::Delivered,
        Ok(Err(e @ MiniChatAuditPluginError::Permanent(_))) => {
            AuditEmitResult::Reject(e.to_string())
        }
        Ok(Err(e)) => AuditEmitResult::Retry(e.to_string()),
        Err(_) => AuditEmitResult::Retry(format!(
            "audit plugin did not respond within {}s",
            timeout.as_secs_f32()
        )),
    }
}

/// Gateway over an in-process plugin client (bypasses the types-registry);
/// used by `mini_chat::testing`.
pub struct InProcessAuditGateway {
    client: Arc<dyn MiniChatAuditPluginClientV1>,
}

impl InProcessAuditGateway {
    #[must_use]
    pub fn new(client: Arc<dyn MiniChatAuditPluginClientV1>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl AuditGateway for InProcessAuditGateway {
    async fn emit(&self, ev: MiniChatAuditEvent) -> AuditEmitResult {
        emit_via(&*self.client, EMIT_TIMEOUT, ev).await
    }
}
