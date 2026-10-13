//! Static audit plugin: logs audit events.

use async_trait::async_trait;
use mini_chat_sdk::{AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1};

use super::config::StaticAuditPluginConfig;

pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    #[must_use]
    pub fn from_config(cfg: &StaticAuditPluginConfig) -> Self {
        Self {
            enabled: cfg.enabled,
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        if !self.enabled {
            return Ok(());
        }
        match &event {
            MiniChatAuditEvent::Turn(t) => tracing::info!(
                target: "mini_chat::audit",
                kind = "turn",
                event_type = %t.event_type,
                tenant_id = %t.tenant_id,
                chat_id = %t.chat_id,
                turn_id = %t.turn_id,
                request_id = %t.request_id,
                terminal_state = %t.terminal_state,
                effective_model = %t.effective_model,
                "mini-chat audit event"
            ),
            MiniChatAuditEvent::Mutation(m) => tracing::info!(
                target: "mini_chat::audit",
                kind = "mutation",
                event_type = %m.event_type,
                tenant_id = %m.tenant_id,
                chat_id = %m.chat_id,
                actor_user_id = %m.actor_user_id,
                "mini-chat audit event"
            ),
        }
        Ok(())
    }
}
