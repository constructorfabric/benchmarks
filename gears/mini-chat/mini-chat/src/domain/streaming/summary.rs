//! The thread-summary trigger and the enqueued work item (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`, algorithm `summary-trigger`). The worker that
//! consumes the item belongs to `cpt-cf-mini-chat-feature-background-maintenance`.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The `system_task_type` of the work item.
pub const THREAD_SUMMARY_UPDATE: &str = "thread_summary_update";

/// A position in the message order `(created_at, id)`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frontier {
    pub created_at: OffsetDateTime,
    pub message_id: Uuid,
}

/// What the trigger decides on, all taken from the context assembly of the turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTriggerInput {
    /// Whether a summary existed at assembly.
    pub had_summary: bool,
    pub assembled_context_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    /// `thread_summary_worker.compression_threshold_pct` (1 to 99).
    pub compression_threshold_pct: u32,
}

/// Why the trigger fired.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// No summary existed and `assembled_context_tokens` reached the threshold share of the
    /// effective budget.
    Proactive,
    /// History was truncated.
    Urgent,
}

/// The work item of `outbox.thread_summary_queue_name`, partitioned by `chat_id`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryWork {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// A new UUID v4, stable across every retry of the message.
    pub system_request_id: Uuid,
    /// `None` when the frontier is empty (no summary row).
    pub base_frontier: Option<Frontier>,
    /// The latest non-deleted message that does not belong to the causing turn.
    pub frozen_target: Frontier,
}

/// Evaluate the trigger from the estimate of the context assembly: urgent when history was
/// truncated, proactive when no summary existed and the assembled tokens are at least
/// `effective_budget * compression_threshold_pct / 100`, otherwise `None`. Never from message
/// or turn counts.
#[must_use]
pub fn evaluate_trigger(input: &SummaryTriggerInput) -> Option<TriggerKind> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-estimate-source
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-conditions
    if input.messages_truncated {
        return Some(TriggerKind::Urgent);
    }
    // widened arithmetic: the budget and the threshold cannot overflow
    let reached = i128::from(input.assembled_context_tokens).saturating_mul(100)
        >= i128::from(input.effective_budget)
            .saturating_mul(i128::from(input.compression_threshold_pct));
    (!input.had_summary && reached).then_some(TriggerKind::Proactive)
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-conditions
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-estimate-source
}

/// Whether the frozen target is ahead of the base frontier: `false` when there is no target, or
/// the base frontier equals or passes the target in the order `(created_at, id)`.
#[must_use]
pub fn has_summary_work(base: Option<Frontier>, target: Option<Frontier>) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-nothing-to-do
    let Some(target) = target else {
        return false;
    };
    match base {
        None => true,
        Some(base) => (target.created_at, target.message_id) > (base.created_at, base.message_id),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-nothing-to-do
}
