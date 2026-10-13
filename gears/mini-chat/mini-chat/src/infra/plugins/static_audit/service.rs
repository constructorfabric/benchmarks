//! Static audit service: logs audit events at info level.

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError};
use tracing::{info, warn};

use super::config::StaticAuditConfig;

/// Static [`MiniChatAuditPluginClientV1`] that logs events (when enabled).
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    #[must_use]
    pub fn from_config(cfg: &StaticAuditConfig) -> Self {
        Self {
            enabled: cfg.enabled,
        }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if !self.enabled {
            return Ok(());
        }
        match serde_json::to_string(&event) {
            Ok(json) => info!(
                tenant_id = %event.tenant_id(),
                event = %json,
                "mini-chat audit event"
            ),
            Err(e) => warn!(error = %e, "failed to serialize mini-chat audit event"),
        }
        Ok(())
    }
}
