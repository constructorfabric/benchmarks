//! The static audit plugin client: logs the received event when `enabled`.

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1};
use uuid::Uuid;

use super::config::StaticAuditConfig;

/// The audit plugin client of the static plugin.
#[derive(Debug)]
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    /// Build the service from the plugin config.
    #[must_use]
    pub fn new(config: &StaticAuditConfig) -> Self {
        Self {
            enabled: config.enabled,
        }
    }
}

/// The event type and the identifiers that correlate the event; never the prompt, the
/// response or the attachments an event may carry.
fn describe(event: &AuditEvent) -> (&'static str, Uuid, Uuid) {
    match event {
        AuditEvent::TurnCompleted(turn) => ("turn_completed", turn.chat_id, turn.request_id),
        AuditEvent::TurnFailed(turn) => ("turn_failed", turn.chat_id, turn.request_id),
        AuditEvent::TurnRetry(mutation) => {
            ("turn_retry", mutation.chat_id, mutation.new_request_id)
        }
        AuditEvent::TurnEdit(mutation) => ("turn_edit", mutation.chat_id, mutation.new_request_id),
        AuditEvent::TurnDelete(delete) => ("turn_delete", delete.chat_id, delete.request_id),
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-enabled
        if self.enabled {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-log
            let (event_type, chat_id, request_id) = describe(&event);
            tracing::info!(
                event_type,
                %chat_id,
                %request_id,
                "mini-chat audit event"
            );
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-log
        } else {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-disabled
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-silent
            // the plugin stays registered but silent: the event is accepted without a log
            tracing::trace!("static audit plugin is disabled; event accepted silently");
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-silent
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-disabled
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-enabled
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-return
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-return
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
