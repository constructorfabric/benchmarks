//! The mutation audit event
//! (`cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`).

use mini_chat_sdk::TurnMutationAuditEvent;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::types::MutationOp;
use crate::domain::error::DomainError;

/// What the audit event of a mutation is built from.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditInput {
    /// The operation.
    pub op: MutationOp,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The caller.
    pub actor_user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The `request_id` of the mutated turn.
    pub target_request_id: Uuid,
    /// The server-generated `request_id` of the replacement (retry and edit).
    pub new_request_id: Option<Uuid>,
    /// The mutation time.
    pub at: OffsetDateTime,
}

/// Builds the event: `turn_retry` and `turn_edit` carry
/// `original_request_id` and `new_request_id`, `turn_delete` carries
/// `request_id`. Never any content.
///
/// # Errors
///
/// [`DomainError::Internal`] for a retry or an edit without a new request id.
// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-audit-metrics:p1
pub fn mutation_audit_event(input: &AuditInput) -> Result<TurnMutationAuditEvent, DomainError> {
    let replacement = |what: &str| {
        input.new_request_id.ok_or_else(|| {
            DomainError::Internal(format!("a {what} audit event needs the new request id"))
        })
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-audit-no-content
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-match-audit-op
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-return-mutation-event
    Ok(match input.op {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-case-audit-retry
        MutationOp::Retry => TurnMutationAuditEvent::TurnRetry {
            tenant_id: input.tenant_id,
            actor_user_id: input.actor_user_id,
            chat_id: input.chat_id,
            original_request_id: input.target_request_id,
            new_request_id: replacement("retry")?,
            timestamp: input.at,
        },
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-case-audit-retry
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-case-audit-edit
        MutationOp::Edit => TurnMutationAuditEvent::TurnEdit {
            tenant_id: input.tenant_id,
            actor_user_id: input.actor_user_id,
            chat_id: input.chat_id,
            original_request_id: input.target_request_id,
            new_request_id: replacement("edit")?,
            timestamp: input.at,
        },
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-case-audit-edit
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-case-audit-delete
        MutationOp::Delete => TurnMutationAuditEvent::TurnDelete {
            tenant_id: input.tenant_id,
            actor_user_id: input.actor_user_id,
            chat_id: input.chat_id,
            request_id: input.target_request_id,
            timestamp: input.at,
        },
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-case-audit-delete
    })
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-return-mutation-event
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-match-audit-op
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-audit-no-content
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
