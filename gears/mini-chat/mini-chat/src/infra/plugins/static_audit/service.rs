//! Client of the static audit plugin.

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, TurnAuditEventType,
    TurnMutationAuditEvent,
};
use uuid::Uuid;

use super::config::StaticAuditConfig;

/// Audit plugin that logs events as structured records.
pub struct StaticAuditService {
    config: StaticAuditConfig,
}

impl StaticAuditService {
    /// Creates the service.
    #[must_use]
    pub fn new(config: StaticAuditConfig) -> Self {
        Self { config }
    }
}

/// Metadata-only projection of an audit event: the only fields that are logged.
///
/// Prompt, response, attachments, license and quota scope never enter this
/// record, so they can never reach the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuditLogRecord {
    event_type: &'static str,
    tenant_id: Uuid,
    chat_id: Uuid,
    turn_id: Option<Uuid>,
    request_id: Uuid,
    user_id: Uuid,
    selected_model: Option<String>,
    effective_model: Option<String>,
    latency_ms: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl AuditLogRecord {
    fn from_event(event: &MiniChatAuditEvent) -> Self {
        match event {
            MiniChatAuditEvent::Turn(turn) => Self {
                event_type: match turn.event_type {
                    TurnAuditEventType::TurnCompleted => "turn_completed",
                    TurnAuditEventType::TurnFailed => "turn_failed",
                },
                tenant_id: turn.tenant_id,
                chat_id: turn.chat_id,
                turn_id: Some(turn.turn_id),
                request_id: turn.request_id,
                user_id: turn.requester_user_id,
                selected_model: Some(turn.selected_model.clone()),
                effective_model: Some(turn.effective_model.clone()),
                latency_ms: Some(turn.latency_ms),
                input_tokens: Some(turn.usage.input_tokens),
                output_tokens: Some(turn.usage.output_tokens),
            },
            MiniChatAuditEvent::Mutation(mutation) => {
                let (event_type, tenant_id, actor_user_id, chat_id, request_id) = match mutation {
                    TurnMutationAuditEvent::TurnRetry {
                        tenant_id,
                        actor_user_id,
                        chat_id,
                        new_request_id,
                        ..
                    } => (
                        "turn_retry",
                        *tenant_id,
                        *actor_user_id,
                        *chat_id,
                        *new_request_id,
                    ),
                    TurnMutationAuditEvent::TurnEdit {
                        tenant_id,
                        actor_user_id,
                        chat_id,
                        new_request_id,
                        ..
                    } => (
                        "turn_edit",
                        *tenant_id,
                        *actor_user_id,
                        *chat_id,
                        *new_request_id,
                    ),
                    TurnMutationAuditEvent::TurnDelete {
                        tenant_id,
                        actor_user_id,
                        chat_id,
                        request_id,
                        ..
                    } => (
                        "turn_delete",
                        *tenant_id,
                        *actor_user_id,
                        *chat_id,
                        *request_id,
                    ),
                };
                Self {
                    event_type,
                    tenant_id,
                    chat_id,
                    turn_id: None,
                    request_id,
                    user_id: actor_user_id,
                    selected_model: None,
                    effective_model: None,
                    latency_ms: None,
                    input_tokens: None,
                    output_tokens: None,
                }
            }
        }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-if-audit-enabled
        if self.config.enabled {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-log-audit-event
            let record = AuditLogRecord::from_event(&event);
            tracing::info!(
                target: "mini_chat::audit",
                event_type = record.event_type,
                tenant_id = %record.tenant_id,
                chat_id = %record.chat_id,
                turn_id = ?record.turn_id,
                request_id = %record.request_id,
                user_id = %record.user_id,
                selected_model = ?record.selected_model,
                effective_model = ?record.effective_model,
                latency_ms = ?record.latency_ms,
                input_tokens = ?record.input_tokens,
                output_tokens = ?record.output_tokens,
                "mini-chat audit event"
            );
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-log-audit-event
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-if-audit-enabled

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-return-audit-ok
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-return-audit-ok
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
