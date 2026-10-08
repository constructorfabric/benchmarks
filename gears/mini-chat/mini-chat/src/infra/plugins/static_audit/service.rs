//! Static audit plugin implementation: logs every event.

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError};
use tracing::info;

pub struct StaticAuditService {
    pub enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if !self.enabled {
            return Ok(());
        }
        match serde_json::to_string(&event) {
            Ok(json) => info!(target: "mini_chat_audit", event = %json, "mini-chat audit event"),
            Err(e) => {
                info!(target: "mini_chat_audit", error = %e, "mini-chat audit event (unserializable)");
            }
        }
        Ok(())
    }
}
