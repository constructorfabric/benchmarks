#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the thread-summary trigger decisions
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`):
//! enabled and completed, urgent (truncation), the threshold in integer
//! arithmetic against the effective budget, an effective summary, no earlier
//! message and a frontier already at the target.

use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    Precheck, SYSTEM_TASK_TYPE, SummaryTask, SummaryTrigger, SummaryTriggerPlan, TriggerResult,
    boundary, frozen_range, precheck,
};
use crate::domain::service::stream::ports::MessageBoundary;

fn plan() -> SummaryTriggerPlan {
    SummaryTriggerPlan {
        enabled: true,
        compression_threshold_pct: 80,
        assembled_context_tokens: 800,
        messages_truncated: false,
        effective_input_budget: 1000,
        summary_applied: false,
        system_request_id: Uuid::from_u128(0x5157),
    }
}

fn trigger(plan: SummaryTriggerPlan) -> SummaryTrigger {
    SummaryTrigger {
        plan,
        completed: true,
        tenant_id: Uuid::from_u128(0x7E),
        chat_id: Uuid::from_u128(0xC1),
        causing_request_id: Uuid::from_u128(0xE1),
    }
}

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000 + secs).unwrap()
}

fn b(secs: i64, id: u128) -> MessageBoundary {
    boundary(at(secs), Uuid::from_u128(id))
}

fn not_evaluated() -> Precheck {
    Precheck::Done(TriggerResult::NotEvaluated)
}

#[test]
fn the_system_task_type_is_thread_summary_update() {
    assert_eq!(SYSTEM_TASK_TYPE, "thread_summary_update");
}

#[test]
fn a_switched_off_trigger_is_not_evaluated() {
    let mut off = plan();
    off.enabled = false;
    off.messages_truncated = true;

    assert_eq!(precheck(&trigger(off)), not_evaluated());
}

#[test]
fn a_turn_that_did_not_complete_is_not_evaluated() {
    let mut failed = trigger(plan());
    failed.completed = false;

    assert_eq!(precheck(&failed), not_evaluated());
}

#[test]
fn the_disabled_plan_never_fires() {
    let mut disabled = trigger(SummaryTriggerPlan::disabled());
    disabled.plan.messages_truncated = true;

    assert_eq!(precheck(&disabled), not_evaluated());
}

#[test]
fn below_the_threshold_and_not_truncated_is_not_evaluated() {
    let mut below = plan();
    below.assembled_context_tokens = 799;

    assert_eq!(precheck(&trigger(below)), not_evaluated());
}

#[test]
fn exactly_at_the_threshold_reads_the_frontier() {
    // 800 * 100 = 80_000 = 1000 * 80
    assert_eq!(precheck(&trigger(plan())), Precheck::ReadFrontier);
}

#[test]
fn the_threshold_uses_integer_arithmetic_without_rounding() {
    let mut odd = plan();
    odd.effective_input_budget = 999; // 80% is 799.2: 799 tokens is below, 800 is above
    odd.assembled_context_tokens = 799;
    assert_eq!(precheck(&trigger(odd.clone())), not_evaluated());
    odd.assembled_context_tokens = 800;
    assert_eq!(precheck(&trigger(odd)), Precheck::ReadFrontier);
}

#[test]
fn truncation_alone_is_urgent_whatever_the_budget() {
    let mut truncated = plan();
    truncated.messages_truncated = true;
    truncated.assembled_context_tokens = 1;
    assert_eq!(
        precheck(&trigger(truncated.clone())),
        Precheck::ReadFrontier
    );
    truncated.effective_input_budget = 0;
    assert_eq!(precheck(&trigger(truncated)), Precheck::ReadFrontier);
}

#[test]
fn a_zero_budget_never_reaches_the_threshold() {
    let mut zero = plan();
    zero.effective_input_budget = 0;
    zero.assembled_context_tokens = 5_000;

    assert_eq!(precheck(&trigger(zero)), not_evaluated());
}

#[test]
fn the_compression_threshold_percentage_is_configurable() {
    let mut half = plan();
    half.compression_threshold_pct = 50;
    half.assembled_context_tokens = 500;
    assert_eq!(precheck(&trigger(half.clone())), Precheck::ReadFrontier);
    half.assembled_context_tokens = 499;
    assert_eq!(precheck(&trigger(half)), not_evaluated());
}

#[test]
fn huge_values_do_not_overflow() {
    let mut huge = plan();
    huge.assembled_context_tokens = i64::MAX;
    huge.effective_input_budget = i64::MAX;
    huge.compression_threshold_pct = 99;

    assert_eq!(precheck(&trigger(huge)), Precheck::ReadFrontier);
}

#[test]
fn an_effective_summary_without_truncation_is_not_needed() {
    let mut summarized = plan();
    summarized.summary_applied = true;

    assert_eq!(
        precheck(&trigger(summarized)),
        Precheck::Done(TriggerResult::NotNeeded)
    );
}

#[test]
fn an_effective_summary_does_not_stop_a_truncating_turn() {
    let mut summarized = plan();
    summarized.summary_applied = true;
    summarized.messages_truncated = true;

    assert_eq!(precheck(&trigger(summarized)), Precheck::ReadFrontier);
}

#[test]
fn a_summary_below_the_threshold_is_not_evaluated_not_not_needed() {
    let mut summarized = plan();
    summarized.summary_applied = true;
    summarized.assembled_context_tokens = 10;

    assert_eq!(precheck(&trigger(summarized)), not_evaluated());
}

// ---- the frozen range ---------------------------------------------------------------------------------

#[test]
fn no_target_means_not_needed() {
    assert_eq!(frozen_range(None, None), None);
    assert_eq!(frozen_range(Some(b(1, 1)), None), None);
}

#[test]
fn a_first_summary_freezes_the_target_with_no_base() {
    let range = frozen_range(None, Some(b(5, 9))).unwrap();

    assert_eq!(range.base, None);
    assert_eq!(range.target, b(5, 9));
}

#[test]
fn a_base_before_the_target_is_a_range() {
    let range = frozen_range(Some(b(1, 1)), Some(b(5, 9))).unwrap();

    assert_eq!((range.base, range.target), (Some(b(1, 1)), b(5, 9)));
}

#[test]
fn a_base_at_or_after_the_target_is_not_needed() {
    assert_eq!(
        frozen_range(Some(b(5, 9)), Some(b(5, 9))),
        None,
        "at the target"
    );
    assert_eq!(
        frozen_range(Some(b(6, 1)), Some(b(5, 9))),
        None,
        "after the target"
    );
}

#[test]
fn the_order_breaks_a_created_at_tie_by_message_id() {
    assert_eq!(
        frozen_range(Some(b(5, 9)), Some(b(5, 8))),
        None,
        "base id above target id"
    );
    assert!(
        frozen_range(Some(b(5, 8)), Some(b(5, 9))).is_some(),
        "base id below target id"
    );
}

// ---- the work item and the result label -----------------------------------------------------------------

#[test]
fn the_task_carries_the_trigger_identity_and_the_precomputed_system_request_id() {
    let trigger = trigger(plan());
    let range = frozen_range(Some(b(1, 1)), Some(b(5, 9))).unwrap();

    let task = SummaryTask::new(&trigger, range);

    assert_eq!(task.tenant_id, Uuid::from_u128(0x7E));
    assert_eq!(task.chat_id, Uuid::from_u128(0xC1));
    assert_eq!(task.system_request_id, Uuid::from_u128(0x5157));
    assert_eq!(task.base, Some(b(1, 1)));
    assert_eq!(task.target, b(5, 9));
}

#[test]
fn only_scheduled_and_not_needed_are_recorded() {
    assert_eq!(TriggerResult::Scheduled.label(), Some("scheduled"));
    assert_eq!(TriggerResult::NotNeeded.label(), Some("not_needed"));
    assert_eq!(TriggerResult::NotEvaluated.label(), None);
}
