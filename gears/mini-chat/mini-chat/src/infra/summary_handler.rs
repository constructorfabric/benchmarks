//! Leased handler of the thread-summary queue (`outbox.thread_summary_queue_name`,
//! DESIGN section 3.6 "Thread Summary Update", "Asynchronous execution").
//!
//! [`ThreadSummaryHandler`] decodes one [`ThreadSummaryPayload`], runs it
//! through [`ThreadSummaryService::execute`] and maps the outcome with
//! [`disposition`]:
//!
//! | Situation | Result |
//! |-----------|--------|
//! | payload not a [`ThreadSummaryPayload`] (or a half-set base frontier) | `Reject` |
//! | committed, CAS lost, base missing, frontier deleted, empty range | `Ok` |
//! | summary model missing or disabled | `Reject` |
//! | provider failure, empty summary, other transient failure | `Retry` |
//! | ... on delivery `thread_summary_worker.max_attempts` | `Reject` (dead letter) |
//!
//! Metrics (prefix `metrics.prefix`): `{prefix}_thread_summary_execution
//! {result}`, `{prefix}_thread_summary_cas_conflicts` and
//! `{prefix}_summary_fallback` (provider failures).

use std::sync::Arc;

use async_trait::async_trait;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::{error, info, warn};

use crate::domain::ports::{MessageKey, ThreadSummaryTask};
use crate::domain::summary_service::{
    Disposition, SummaryOutcome, ThreadSummaryService, disposition,
};
use crate::infra::outbox::ThreadSummaryPayload;

/// The work item of `payload`; `Err` when only one base frontier field is
/// set.
///
/// # Errors
///
/// A diagnostic for a payload whose base frontier is half set.
pub fn task_of(payload: &ThreadSummaryPayload) -> Result<ThreadSummaryTask, String> {
    let base_frontier = match (
        payload.base_frontier_created_at,
        payload.base_frontier_message_id,
    ) {
        (Some(created_at), Some(id)) => Some(MessageKey { created_at, id }),
        (None, None) => None,
        _ => return Err("base frontier fields must be both set or both null".to_owned()),
    };
    Ok(ThreadSummaryTask {
        tenant_id: payload.tenant_id,
        chat_id: payload.chat_id,
        system_request_id: payload.system_request_id,
        base_frontier,
        frozen_target: MessageKey {
            created_at: payload.frozen_target_created_at,
            id: payload.frozen_target_message_id,
        },
    })
}

/// Counters of the thread-summary handler.
struct SummaryMetrics {
    execution: Counter<u64>,
    cas_conflicts: Counter<u64>,
    fallback: Counter<u64>,
}

impl SummaryMetrics {
    fn new(prefix: &str) -> Self {
        let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
        let meter = opentelemetry::global::meter_with_scope(scope);
        Self {
            execution: meter
                .u64_counter(format!("{prefix}_thread_summary_execution"))
                .with_description("Thread summary handler outcomes")
                .build(),
            cas_conflicts: meter
                .u64_counter(format!("{prefix}_thread_summary_cas_conflicts"))
                .with_description("Thread summary tasks that lost the frontier CAS")
                .build(),
            fallback: meter
                .u64_counter(format!("{prefix}_summary_fallback"))
                .with_description("Thread summary LLM failures (previous summary kept)")
                .build(),
        }
    }

    fn record(&self, outcome: &SummaryOutcome) {
        if let Some(result) = outcome.metric_result() {
            self.execution.add(1, &[KeyValue::new("result", result)]);
        }
        match outcome {
            SummaryOutcome::CasConflict => self.cas_conflicts.add(1, &[]),
            SummaryOutcome::ProviderError(_) => self.fallback.add(1, &[]),
            _ => {}
        }
    }
}

/// Handler of the thread-summary queue (module docs).
pub struct ThreadSummaryHandler {
    service: Arc<ThreadSummaryService>,
    metrics: SummaryMetrics,
}

impl std::fmt::Debug for ThreadSummaryHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadSummaryHandler")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl ThreadSummaryHandler {
    /// Handler running `service` (whose settings carry
    /// `thread_summary_worker.max_attempts`); outcomes are counted under
    /// `metrics_prefix`.
    #[must_use]
    pub fn new(service: Arc<ThreadSummaryService>, metrics_prefix: &str) -> Self {
        Self {
            service,
            metrics: SummaryMetrics::new(metrics_prefix),
        }
    }

    /// Processes one decoded work item on delivery `attempt` (1-based).
    pub async fn process(&self, task: &ThreadSummaryTask, attempt: u32) -> MessageResult {
        let outcome = self.service.execute(task).await;
        self.metrics.record(&outcome);
        let max_attempts = self.service.settings().max_attempts;
        let result = disposition(&outcome, attempt, max_attempts);
        log_outcome(task, &outcome, &result, attempt);
        match result {
            Disposition::Ok => MessageResult::Ok,
            Disposition::Retry => MessageResult::Retry,
            Disposition::Reject(reason) => MessageResult::Reject(reason),
        }
    }
}

fn log_outcome(
    task: &ThreadSummaryTask,
    outcome: &SummaryOutcome,
    result: &Disposition,
    attempt: u32,
) {
    match (outcome, result) {
        (SummaryOutcome::Committed { token_estimate }, _) => log_committed(task, *token_estimate),
        (_, Disposition::Ok) => log_finished(task, outcome),
        (_, Disposition::Retry) => log_retry(task, outcome, attempt),
        (_, Disposition::Reject(reason)) => log_rejected(task, reason, attempt),
    }
}

fn log_committed(task: &ThreadSummaryTask, token_estimate: i32) {
    info!(
        target: "mini_chat.thread_summary",
        chat_id = %task.chat_id,
        system_request_id = %task.system_request_id,
        token_estimate,
        "thread summary committed"
    );
}

fn log_finished(task: &ThreadSummaryTask, outcome: &SummaryOutcome) {
    info!(
        target: "mini_chat.thread_summary",
        chat_id = %task.chat_id,
        system_request_id = %task.system_request_id,
        outcome = ?outcome,
        "thread summary task finished without a commit"
    );
}

fn log_retry(task: &ThreadSummaryTask, outcome: &SummaryOutcome, attempt: u32) {
    warn!(
        target: "mini_chat.thread_summary",
        chat_id = %task.chat_id,
        system_request_id = %task.system_request_id,
        attempt,
        outcome = ?outcome,
        "thread summary attempt failed; retrying"
    );
}

fn log_rejected(task: &ThreadSummaryTask, reason: &str, attempt: u32) {
    error!(
        target: "mini_chat.thread_summary",
        chat_id = %task.chat_id,
        system_request_id = %task.system_request_id,
        attempt,
        reason = %reason,
        "thread summary task dead-lettered"
    );
}

/// Attempt number (1-based) of `msg`; the outbox counts retries from 0.
fn attempt_number(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1)
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let decoded = serde_json::from_slice::<ThreadSummaryPayload>(&msg.payload)
            .map_err(|err| err.to_string())
            .and_then(|payload| task_of(&payload));
        match decoded {
            Ok(task) => self.process(&task, attempt_number(msg)).await,
            Err(err) => {
                error!(
                    target: "mini_chat.thread_summary",
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %err,
                    "malformed thread summary payload rejected"
                );
                MessageResult::Reject(format!("malformed thread summary payload: {err}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "summary_handler_tests.rs"]
mod tests;
