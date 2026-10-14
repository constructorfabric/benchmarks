//! The leased handler of the thread-summary queue: it maps the outcome of the domain worker to
//! the shared outbox (`Ok`, `Retry`, `Reject`) and replaces the placeholder `UnwiredHandler` of
//! the foundation (`cpt-cf-mini-chat-algo-background-maintenance-handler-wiring`).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::maintenance::HandlerOutcome;
use crate::domain::maintenance::summary_worker::ThreadSummaryWorker;

/// The leased handler of `outbox.thread_summary_queue_name`.
// @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-turn-completes
// the work item is enqueued by the finalization transaction of the send flow through the summary
// trigger of the message streaming feature, which this feature does not change
// @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-turn-completes
// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-no-local-worker
// no gear-local polling loop, claim or reclaim table, leader-elected worker or task state machine
// exists for this path: the delivery, the partitioned ordering, the lease, the retries with
// backoff and the dead-letter handling are the shared outbox's
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-no-local-worker
pub struct ThreadSummaryHandler {
    worker: Arc<ThreadSummaryWorker>,
}

impl ThreadSummaryHandler {
    /// Create the handler over the domain worker.
    #[must_use]
    pub fn new(worker: Arc<ThreadSummaryWorker>) -> Self {
        Self { worker }
    }
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-deliver
        // the shared outbox delivers one chat's items in order on the partition of its `chat_id`,
        // under the lease `thread_summary_worker.claim_timeout_secs`
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-attempts
        // `attempts` is 0 on the first delivery, so the N-th delivery carries N minus 1
        let attempts = msg.attempts;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-attempts
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-deliver
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-handle
        let outcome = self.worker.handle(&msg.payload, attempts).await;
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-handle
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-outcomes
        // a reason never carries message content, summary text or a provider identifier
        match outcome {
            HandlerOutcome::Ok => {
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-next-turn
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-return
                // a committed summary is loaded by the context assembly of the next turn, which
                // reports `thread_summary_applied`; this handler never waits for or blocks a turn
                MessageResult::Ok
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-return
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-next-turn
            }
            HandlerOutcome::Retry => {
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-retry
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-redeliver
                // the shared outbox redelivers the same message with the same `system_request_id`
                // after its backoff (capped at 30 s); until a commit succeeds the previous summary
                // stays in effect
                MessageResult::Retry
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-redeliver
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-retry
            }
            HandlerOutcome::Reject(reason) => {
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-rejected
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-dead-letter
                // the message moves to the shared dead-letter store and the frontier stays where
                // it was; recovery is a later trigger with the same base and a wider target
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-replay
                // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-redrive
                // an operator replays the dead letter through the shared outbox tooling after the
                // cause is fixed (the replay re-runs the handler with the same `system_request_id`
                // and commits only if the stored frontier still equals the base frontier), or
                // enqueues a replacement work item for a wider range; no admin route is added
                MessageResult::Reject(reason)
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-redrive
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-replay
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-dead-letter
                // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-long-conversation:p1:inst-lc-rejected
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-outcomes
    }
}

#[cfg(test)]
#[path = "thread_summary_handler_tests.rs"]
mod thread_summary_handler_tests;
