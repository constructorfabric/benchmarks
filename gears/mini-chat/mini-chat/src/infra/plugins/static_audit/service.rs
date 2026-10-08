//! Static audit service.

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1};
use tracing::info;

/// Logs audit events as JSON at info level when enabled.
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        if !self.enabled {
            return Ok(());
        }
        let json = serde_json::to_string(&event)
            .map_err(|e| AuditPluginError::Permanent(format!("serialize audit event: {e}")))?;
        info!(audit_event = %json, "mini-chat audit event");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::{AuditEvent, MiniChatAuditPluginClientV1, TurnMutationAuditEvent};
    use uuid::Uuid;

    use super::*;

    fn event() -> AuditEvent {
        AuditEvent::Mutation(TurnMutationAuditEvent {
            event_type: "turn_retry".to_owned(),
            tenant_id: Uuid::from_u128(1),
            actor_user_id: Uuid::from_u128(2),
            chat_id: Uuid::from_u128(3),
            original_request_id: None,
            new_request_id: None,
            request_id: None,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
        })
    }

    #[tokio::test]
    async fn emit_succeeds_enabled_or_not() {
        StaticAuditService::new(true).emit(event()).await.unwrap();
        StaticAuditService::new(false).emit(event()).await.unwrap();
    }
}
