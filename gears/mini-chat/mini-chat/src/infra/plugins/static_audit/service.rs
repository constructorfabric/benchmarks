//! Static audit service: logs the event type and the identities of the event,
//! never content.

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1};
use uuid::Uuid;

use super::config::StaticAuditConfig;

/// Logs audit events when enabled.
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    /// Creates the service from its configuration.
    #[must_use]
    pub fn new(cfg: &StaticAuditConfig) -> Self {
        Self {
            enabled: cfg.enabled,
        }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            log_event(&event);
        }
        Ok(())
    }
}

/// Logs the event type and the identities of the event, never content: the
/// `request_id` of a turn finalization, the chat, actor and both request ids
/// of a retry / edit, the chat, actor and request id of a delete.
fn log_event(event: &AuditEvent) {
    let event_type = event.event_type();
    match event {
        AuditEvent::TurnCompleted(turn) | AuditEvent::TurnFailed(turn) => {
            tracing::info!(event_type, request_id = %turn.request_id, "mini-chat audit event");
        }
        AuditEvent::TurnRetry {
            chat_id,
            actor_user_id,
            original_request_id,
            new_request_id,
            ..
        }
        | AuditEvent::TurnEdit {
            chat_id,
            actor_user_id,
            original_request_id,
            new_request_id,
            ..
        } => log_replacement(
            event_type,
            [
                *chat_id,
                *actor_user_id,
                *original_request_id,
                *new_request_id,
            ],
        ),
        AuditEvent::TurnDelete {
            chat_id,
            actor_user_id,
            request_id,
            ..
        } => {
            tracing::info!(
                event_type,
                chat_id = %chat_id,
                actor_user_id = %actor_user_id,
                request_id = %request_id,
                "mini-chat audit event"
            );
        }
    }
}

/// `[chat, actor, original request, new request]` of a retry / edit event.
fn log_replacement(event_type: &str, ids: [Uuid; 4]) {
    let [chat_id, actor_user_id, original_request_id, new_request_id] = ids;
    tracing::info!(
        event_type,
        chat_id = %chat_id,
        actor_user_id = %actor_user_id,
        original_request_id = %original_request_id,
        new_request_id = %new_request_id,
        "mini-chat audit event"
    );
}
