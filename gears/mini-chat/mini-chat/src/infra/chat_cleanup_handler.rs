//! The leased handler of the chat-cleanup queue: it maps the outcome of the domain worker to the
//! shared outbox (`Ok`, `Retry`, `Reject`) and replaces the placeholder `UnwiredHandler` of the
//! foundation (`cpt-cf-mini-chat-algo-background-maintenance-handler-wiring`).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::maintenance::HandlerOutcome;
use crate::domain::maintenance::chat_cleanup::ChatCleanupWorker;

/// The leased handler of `outbox.chat_cleanup_queue_name`.
// @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-delete
// the chat DELETE transaction of the chat management feature soft-deletes the chat, marks its
// attachments `pending` and enqueues the chat-cleanup message, and answers 204 without any
// provider call
// @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-delete
// @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-user-view
// no endpoint exposes the cleanup progress: this handler adds no route, and the user observes only
// the deletion (the chat and its sub-resources answer 404)
// @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-user-view
// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-no-local-worker
// no gear-local polling loop, claim or reclaim table, leader-elected worker or task state machine
// exists for this path: the delivery, the partitioned ordering, the lease, the retries with
// backoff and the dead-letter handling are the shared outbox's
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-no-local-worker
pub struct ChatCleanupHandler {
    worker: Arc<ChatCleanupWorker>,
}

impl ChatCleanupHandler {
    /// Create the handler over the domain worker.
    #[must_use]
    pub fn new(worker: Arc<ChatCleanupWorker>) -> Self {
        Self { worker }
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-deliver
        // the shared outbox delivers the message partitioned by `chat_id` under the shared default
        // lease of 30 s; the worker bounds every provider call by 5 s and the delivery by 20 s,
        // so a delivery ends inside its lease
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-attempts
        // `attempts` is 0 on the first delivery, so the N-th delivery carries N minus 1
        let attempts = msg.attempts;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-attempts
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-deliver
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-handle
        let outcome = self.worker.handle(&msg.payload, attempts).await;
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-handle
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-outcomes
        // a reason never carries message content or a provider identifier
        match outcome {
            HandlerOutcome::Ok => {
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-return
                // the chat is provider-purged, or purged with recorded provider-file debt
                MessageResult::Ok
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-return
            }
            HandlerOutcome::Retry => {
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-retry
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-redeliver
                // the shared outbox redelivers the same message after its backoff; the next
                // delivery re-reads the attachment rows and the `chat_vector_stores` row and
                // continues from the persisted state
                MessageResult::Retry
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-redeliver
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-retry
            }
            HandlerOutcome::Reject(reason) => {
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-rejected
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-dead-letter
                // the message moves to the shared dead-letter store; a kept `chat_vector_stores`
                // row marks the outstanding store delete for an operator replay
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-store-replay
                // the replayed message finds no `pending` row and retries the store delete once
                // per delivery
                MessageResult::Reject(reason)
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-store-replay
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-dead-letter
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup:p1:inst-cd-rejected
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-outcomes
    }
}

#[cfg(test)]
#[path = "chat_cleanup_handler_tests.rs"]
mod chat_cleanup_handler_tests;
