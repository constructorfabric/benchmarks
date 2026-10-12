//! The summary trigger (DESIGN "Summary trigger based on token budget").

use mini_chat_sdk::ModelCatalogEntry;

use super::model::{Frontier, SummaryTrigger};
use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::context::{ContextPlan, effective_input_limit};

/// Evaluates the trigger for a turn from its assembled context.
///
/// Proactive: `assembled_context_tokens >= compression_threshold_pct` percent of the effective
/// budget (`min(max_input_tokens, context_window - max_output_tokens_applied)`; tool surcharges and
/// the fixed overhead are not subtracted). Urgent: the assembly dropped older messages. `None`
/// when the worker is disabled or neither condition holds.
///
/// Whether a summary already exists is decided in the finalization transaction, which reads the
/// stored frontier.
#[must_use]
pub fn evaluate(
    config: &ThreadSummaryWorkerConfig,
    entry: &ModelCatalogEntry,
    max_output_tokens_applied: u32,
    plan: &ContextPlan,
) -> Option<SummaryTrigger> {
    if !config.enabled {
        return None;
    }
    let proactive = effective_input_limit(entry, max_output_tokens_applied).is_some_and(|budget| {
        i128::from(plan.assembled_context_tokens) * 100
            >= i128::from(budget) * i128::from(config.compression_threshold_pct)
    });
    let urgent = plan.messages_truncated;
    (proactive || urgent).then_some(SummaryTrigger { proactive, urgent })
}

/// Decides in the finalization transaction whether a work item is enqueued and for which frozen
/// target.
///
/// `base` is the stored summary frontier (`None` without a summary), `latest` the latest
/// non-deleted message that does not belong to the causing turn. Urgent fires whatever the stored
/// summary; proactive only without one. No earlier message, or a frontier that is already at or
/// after the target, means nothing to summarize.
#[must_use]
pub fn frozen_target(
    trigger: SummaryTrigger,
    base: Option<Frontier>,
    latest: Option<Frontier>,
) -> Option<Frontier> {
    let fires = trigger.urgent || (trigger.proactive && base.is_none());
    if !fires {
        return None;
    }
    let target = latest?;
    match base {
        Some(base) if base >= target => None,
        _ => Some(target),
    }
}

#[cfg(test)]
#[path = "trigger_tests.rs"]
mod tests;
