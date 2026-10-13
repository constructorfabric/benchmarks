//! The thread-summary trigger
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`): the
//! pure decisions evaluated in the finalization transaction of a completed
//! turn. The statements (base frontier, frozen target, enqueue) are the
//! repository's; the decisions around them are here.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::service::stream::ports::MessageBoundary;

/// `system_task_type` of the work item.
pub const SYSTEM_TASK_TYPE: &str = "thread_summary_update";

/// The `ContextPlan` outputs of a turn and the parameters of the trigger, carried
/// by the provider task into the finalization input.
#[domain_model]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryTriggerPlan {
    /// `thread_summary_worker.enabled`.
    pub enabled: bool,
    /// `thread_summary_worker.compression_threshold_pct`.
    pub compression_threshold_pct: u32,
    /// `assembled_context_tokens` of the `ContextPlan`.
    pub assembled_context_tokens: i64,
    /// `messages_truncated` of the `ContextPlan`.
    pub messages_truncated: bool,
    /// The effective input budget (`inst-input-limit`).
    pub effective_input_budget: i64,
    /// A summary was loaded for the turn's context (`thread_summary_applied`).
    pub summary_applied: bool,
    /// The `system_request_id` generated before the transaction.
    pub system_request_id: Uuid,
}

impl SummaryTriggerPlan {
    /// A plan that never fires (the trigger is switched off).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            compression_threshold_pct: 80,
            assembled_context_tokens: 0,
            messages_truncated: false,
            effective_input_budget: 0,
            summary_applied: false,
            system_request_id: Uuid::nil(),
        }
    }
}

/// The trigger input of one finalization attempt.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryTrigger {
    /// The plan of the turn.
    pub plan: SummaryTriggerPlan,
    /// The committed classification is `completed`.
    pub completed: bool,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// `request_id` of the causing turn (never summarized).
    pub causing_request_id: Uuid,
}

/// The outcome of the trigger.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerResult {
    /// A work item was enqueued.
    Scheduled,
    /// Evaluated, nothing to do.
    NotNeeded,
    /// Not evaluated (off, not completed or below the threshold).
    NotEvaluated,
}

impl TriggerResult {
    /// `result` label of `mini_chat_thread_summary_trigger`; `None` for
    /// `NotEvaluated` (nothing is recorded).
    #[must_use]
    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::Scheduled => Some("scheduled"),
            Self::NotNeeded => Some("not_needed"),
            Self::NotEvaluated => None,
        }
    }
}

/// What the cheap conditions (steps 1 to 4) decide.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precheck {
    /// Decided without a database read.
    Done(TriggerResult),
    /// The frontier and the frozen target must be read.
    ReadFrontier,
}

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-summary-trigger:p1
/// Steps 1 to 4: switched off or not `completed` is `NotEvaluated`; neither
/// `urgent` (history truncated) nor `threshold_reached` (the effective budget is
/// above 0 and `assembled_context_tokens * 100` is at least `budget *
/// compression_threshold_pct`) is `NotEvaluated`; a loaded summary without
/// `urgent` is `NotNeeded`; anything else needs the frontier.
#[must_use]
pub fn precheck(trigger: &SummaryTrigger) -> Precheck {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-trigger-off
    let plan = &trigger.plan;
    if !plan.enabled || !trigger.completed {
        return Precheck::Done(TriggerResult::NotEvaluated);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-trigger-off
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-trigger-conditions
    let urgent = plan.messages_truncated;
    let threshold_reached = plan.effective_input_budget > 0
        && i128::from(plan.assembled_context_tokens) * 100
            >= i128::from(plan.effective_input_budget) * i128::from(plan.compression_threshold_pct);
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-trigger-conditions
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-below-threshold
    if !urgent && !threshold_reached {
        return Precheck::Done(TriggerResult::NotEvaluated);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-below-threshold
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-summary-effective
    if plan.summary_applied && !urgent {
        return Precheck::Done(TriggerResult::NotNeeded);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-summary-effective
    Precheck::ReadFrontier
}

/// The frozen range of a work item.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrozenRange {
    /// The base frontier (the stored one), none when no summary exists.
    pub base: Option<MessageBoundary>,
    /// The frozen target.
    pub target: MessageBoundary,
}

/// Steps 7 and 8: no target, or a base frontier at or after the target in the
/// order `(created_at ASC, id ASC)`, is `NotNeeded`.
#[must_use]
pub fn frozen_range(
    base: Option<MessageBoundary>,
    target: Option<MessageBoundary>,
) -> Option<FrozenRange> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-no-target
    let target = target?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-no-target
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-frontier-at-target
    if let Some(base) = base
        && (base.created_at, base.id) >= (target.created_at, target.id)
    {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-if-frontier-at-target
    Some(FrozenRange { base, target })
}

/// The work item enqueued on the thread-summary queue.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTask {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Stable id of the system task (the usage dedupe key).
    pub system_request_id: Uuid,
    /// The base frontier, none for a first summary.
    pub base: Option<MessageBoundary>,
    /// The frozen target frontier.
    pub target: MessageBoundary,
}

impl SummaryTask {
    /// The task of `range`.
    #[must_use]
    pub fn new(trigger: &SummaryTrigger, range: FrozenRange) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-build-summary-payload
        Self {
            tenant_id: trigger.tenant_id,
            chat_id: trigger.chat_id,
            system_request_id: trigger.plan.system_request_id,
            base: range.base,
            target: range.target,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-build-summary-payload
    }
}

/// `created_at` of a boundary as the payload carries it.
#[must_use]
pub fn boundary(created_at: OffsetDateTime, id: Uuid) -> MessageBoundary {
    MessageBoundary { created_at, id }
}

#[cfg(test)]
#[path = "summary_trigger_tests.rs"]
mod tests;
