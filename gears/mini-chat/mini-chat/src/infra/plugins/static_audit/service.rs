//! The static audit plugin implementation.

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditPluginClientV1, TurnAuditEvent, TurnMutationAuditEvent,
};
use tracing::info;

/// Logs audit events (when enabled).
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    /// Creates the service.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            info!(
                event_type = ?event.event_type,
                tenant_id = %event.tenant_id,
                chat_id = %event.chat_id,
                request_id = %event.request_id,
                effective_model = %event.effective_model,
                terminal_state = ?event.terminal_state,
                "mini-chat turn audit event"
            );
        }
        Ok(())
    }

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError> {
        if self.enabled {
            info!(
                event_type = ?event.event_type(),
                tenant_id = %event.tenant_id(),
                chat_id = %event.chat_id(),
                actor_user_id = %event.actor_user_id(),
                request_id = %event.mutated_request_id(),
                "mini-chat turn mutation audit event"
            );
        }
        Ok(())
    }
}
