//! The mutation audit payloads (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`).

use mini_chat_sdk::{AuditEvent, TurnDeleteAuditEvent, TurnMutationAuditEvent};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::mutations::MutationOp;

/// The audit event of a committed mutation: `turn_retry` or `turn_edit` with the actor, the chat,
/// the original and the new `request_id` and the commit time, or `turn_delete` with the actor,
/// the chat, the target `request_id` and the time. No content, attachment, model or provider
/// identifier.
///
/// # Errors
///
/// `Internal` for a retry or edit without a new `request_id`.
pub fn build_mutation_audit(
    op: MutationOp,
    actor_user_id: Uuid,
    chat_id: Uuid,
    target_request_id: Uuid,
    new_request_id: Option<Uuid>,
    timestamp: OffsetDateTime,
) -> Result<AuditEvent, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-no-content
    // the fields are the ids and the time only: no message text, attachment, model or provider
    // identifier
    let mutation = |new_request_id| TurnMutationAuditEvent {
        actor_user_id,
        chat_id,
        original_request_id: target_request_id,
        new_request_id,
        timestamp,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-no-content
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-match
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-return
    match (op, new_request_id) {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-retry
        (MutationOp::Retry, Some(new)) => Ok(AuditEvent::TurnRetry(mutation(new))),
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-retry
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-edit
        (MutationOp::Edit, Some(new)) => Ok(AuditEvent::TurnEdit(mutation(new))),
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-edit
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-delete
        (MutationOp::Delete, _) => Ok(AuditEvent::TurnDelete(TurnDeleteAuditEvent {
            actor_user_id,
            chat_id,
            request_id: target_request_id,
            timestamp,
        })),
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-delete
        (MutationOp::Retry | MutationOp::Edit, None) => Err(DomainError::internal(
            "a retry or edit audit event needs the new request id",
        )),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-return
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-match
}
