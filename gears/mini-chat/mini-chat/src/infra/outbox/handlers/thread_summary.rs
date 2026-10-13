//! Thread summary handler (queue `outbox.thread_summary_queue_name`).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::{decode, delivery_attempt};
use crate::domain::ports::ThreadSummaryRunner;
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// Runs summary tasks through the [`ThreadSummaryRunner`].
pub struct ThreadSummaryHandler {
    runner: Arc<dyn ThreadSummaryRunner>,
}

impl ThreadSummaryHandler {
    #[must_use]
    pub fn new(runner: Arc<dyn ThreadSummaryRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ThreadSummaryPayload = match decode("thread summary", msg) {
            Ok(p) => p,
            Err(reject) => return reject,
        };
        self.runner.run(payload, delivery_attempt(msg)).await.into()
    }
}
