//! The thread-summary trigger: the proactive and urgent conditions, the threshold arithmetic,
//! the summary-exists case and the nothing-to-do check of the frozen range.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `summary-trigger`; definition of done `cpt-cf-mini-chat-dod-message-streaming-finalization`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use super::summary::{
    Frontier, SummaryTriggerInput, TriggerKind, evaluate_trigger, has_summary_work,
};
use crate::test_support::send::t;

fn input(had_summary: bool, tokens: i64, budget: i64, truncated: bool) -> SummaryTriggerInput {
    SummaryTriggerInput {
        had_summary,
        assembled_context_tokens: tokens,
        effective_budget: budget,
        messages_truncated: truncated,
        compression_threshold_pct: 80,
    }
}

#[test]
fn no_summary_and_tokens_at_the_threshold_fire_proactively() {
    assert_eq!(
        evaluate_trigger(&input(false, 800, 1000, false)),
        Some(TriggerKind::Proactive)
    );
    assert_eq!(
        evaluate_trigger(&input(false, 999, 1000, false)),
        Some(TriggerKind::Proactive)
    );
}

#[test]
fn no_summary_and_tokens_below_the_threshold_do_not_fire() {
    assert_eq!(evaluate_trigger(&input(false, 799, 1000, false)), None);
    assert_eq!(evaluate_trigger(&input(false, 0, 1000, false)), None);
}

#[test]
fn truncated_history_fires_urgently_with_or_without_a_summary() {
    assert_eq!(
        evaluate_trigger(&input(false, 10, 1000, true)),
        Some(TriggerKind::Urgent)
    );
    assert_eq!(
        evaluate_trigger(&input(true, 10, 1000, true)),
        Some(TriggerKind::Urgent)
    );
}

#[test]
fn a_summary_that_exists_and_no_truncation_never_fires() {
    assert_eq!(evaluate_trigger(&input(true, 999, 1000, false)), None);
    assert_eq!(evaluate_trigger(&input(true, 5000, 1000, false)), None);
}

#[test]
fn the_threshold_percentage_is_applied_to_the_effective_budget() {
    let mut low = input(false, 100, 1000, false);
    low.compression_threshold_pct = 10;
    assert_eq!(evaluate_trigger(&low), Some(TriggerKind::Proactive));
    let mut high = input(false, 989, 1000, false);
    high.compression_threshold_pct = 99;
    assert_eq!(evaluate_trigger(&high), None);
    high.assembled_context_tokens = 990;
    assert_eq!(evaluate_trigger(&high), Some(TriggerKind::Proactive));
}

#[test]
fn the_threshold_arithmetic_does_not_overflow() {
    let huge = input(false, i64::MAX, i64::MAX, false);
    assert_eq!(evaluate_trigger(&huge), Some(TriggerKind::Proactive));
}

fn frontier(micros: i64, id: u128) -> Frontier {
    Frontier {
        created_at: t(micros),
        message_id: Uuid::from_u128(id),
    }
}

#[test]
fn work_exists_when_the_target_is_ahead_of_the_base_frontier() {
    assert!(has_summary_work(None, Some(frontier(10, 1))));
    assert!(has_summary_work(
        Some(frontier(5, 1)),
        Some(frontier(10, 1))
    ));
}

#[test]
fn work_follows_the_message_order_created_at_then_id() {
    // the same instant: the id breaks the tie
    assert!(has_summary_work(
        Some(frontier(10, 1)),
        Some(frontier(10, 2))
    ));
    assert!(!has_summary_work(
        Some(frontier(10, 2)),
        Some(frontier(10, 1))
    ));
}

#[test]
fn nothing_to_do_without_a_target_or_when_the_base_equals_or_passes_it() {
    assert!(!has_summary_work(None, None));
    assert!(!has_summary_work(Some(frontier(5, 1)), None));
    assert!(!has_summary_work(
        Some(frontier(10, 1)),
        Some(frontier(10, 1))
    ));
    assert!(!has_summary_work(
        Some(frontier(11, 1)),
        Some(frontier(10, 1))
    ));
}
