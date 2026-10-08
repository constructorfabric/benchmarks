//! Audit gateway: delivers audit events to the selected audit plugin.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::ClientHub;

use crate::infra::plugin_select::{PluginResolver, Resolved};

/// Delivery outcome of one audit event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    /// Delivered.
    Delivered,
    /// No plugin registered: acknowledged and dropped.
    Dropped,
    /// Retry later.
    Retry(String),
    /// Permanent failure.
    Reject(String),
}

const AUDIT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Audit plugin access.
pub struct AuditGateway {
    resolver: PluginResolver<MiniChatAuditPluginSpecV1, dyn MiniChatAuditPluginClientV1>,
    fixed: Option<Arc<dyn MiniChatAuditPluginClientV1>>,
}

impl AuditGateway {
    /// New gateway.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { resolver: PluginResolver::new(hub, vendor), fixed: None }
    }

    /// Gateway with a fixed plugin client (tests).
    #[must_use]
    pub fn fixed(hub: Arc<ClientHub>, client: Arc<dyn MiniChatAuditPluginClientV1>) -> Self {
        Self { resolver: PluginResolver::new(hub, String::new()), fixed: Some(client) }
    }

    /// Delivers one event.
    pub async fn deliver(&self, event: MiniChatAuditEvent) -> AuditDelivery {
        let resolved = match &self.fixed {
            Some(c) => Resolved::Ready(Arc::clone(c)),
            None => self.resolver.resolve().await,
        };
        let plugin = match resolved {
            Resolved::Ready(p) => p,
            Resolved::NotRegistered => return AuditDelivery::Dropped,
            Resolved::ClientMissing(id) => {
                return AuditDelivery::Retry(format!("audit plugin client missing for '{id}'"));
            }
            Resolved::Error(e) => return AuditDelivery::Retry(e),
        };
        let fut = async move {
            match event {
                MiniChatAuditEvent::Turn(e) => plugin.emit_turn_audit(e).await,
                MiniChatAuditEvent::TurnRetry(e) => plugin.emit_turn_retry_audit(e).await,
                MiniChatAuditEvent::TurnEdit(e) => plugin.emit_turn_edit_audit(e).await,
                MiniChatAuditEvent::TurnDelete(e) => plugin.emit_turn_delete_audit(e).await,
            }
        };
        let res = tokio::time::timeout(AUDIT_CALL_TIMEOUT, fut)
            .await
            .unwrap_or(Err(MiniChatAuditPluginError::PluginTimeout));
        match res {
            Ok(()) => AuditDelivery::Delivered,
            Err(MiniChatAuditPluginError::Permanent(m)) => AuditDelivery::Reject(m),
            Err(e) => AuditDelivery::Retry(e.to_string()),
        }
    }
}
