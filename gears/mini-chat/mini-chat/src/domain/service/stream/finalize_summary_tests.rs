#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the hook of the thread-summary trigger in the stream finalization
//! (`cpt-cf-mini-chat-algo-streaming-turns-finalize-stream` extended by
//! `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`): the
//! `ContextPlan` outputs carried by the provider task into the finalization input,
//! the trigger input of every attempt (the second run included) and the trigger
//! metric recorded only after a won commit.

use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::FinalizeResult;
use crate::domain::service::stream::ports::FinalizeError;
use crate::domain::service::stream::test_support::{
    CHAT, REQUEST, StreamMetric, TaskWorld, run_premium, usage, won_for,
};
use crate::domain::service::stream::types::StreamErrorCode;
use crate::domain::service::workers::summary_trigger::{
    SummaryTrigger, SummaryTriggerPlan, TriggerResult,
};
use crate::test_doubles::USER_TENANT;

fn plan() -> SummaryTriggerPlan {
    SummaryTriggerPlan {
        enabled: true,
        compression_threshold_pct: 80,
        assembled_context_tokens: 850,
        messages_truncated: true,
        effective_input_budget: 1000,
        summary_applied: true,
        system_request_id: Uuid::from_u128(0x5157_0042),
    }
}

fn run() -> TurnRun {
    TurnRun {
        summary: plan(),
        ..run_premium()
    }
}

fn completed() -> TurnOutcome {
    TurnOutcome::Completed {
        usage: Some(usage(100, 40)),
        response_id: Some("resp".to_owned()),
        incomplete_reason: None,
    }
}

fn incomplete() -> TurnOutcome {
    TurnOutcome::Completed {
        usage: Some(usage(100, 40)),
        response_id: None,
        incomplete_reason: Some("max_output_tokens".to_owned()),
    }
}

fn failed() -> TurnOutcome {
    TurnOutcome::Failed {
        code: StreamErrorCode::ProviderError,
        message: "boom".to_owned(),
        detail: None,
        usage: None,
        response_id: None,
    }
}

async fn finalize(world: &TaskWorld, run: &TurnRun, outcome: &TurnOutcome) -> FinalizeReport {
    finalize_stream(
        &world.deps,
        &FinalizeInput {
            run,
            outcome,
            text: "answer",
            counts: ToolCounts::default(),
            ttft_ms: None,
        },
    )
    .await
}

fn trigger_of(world: &TaskWorld, at: usize) -> SummaryTrigger {
    world.turns.finalizations()[at]
        .1
        .summary
        .clone()
        .expect("the trigger input is handed to the transaction")
}

#[tokio::test]
async fn the_context_plan_outputs_and_the_system_request_id_reach_the_transaction() {
    let world = TaskWorld::new();

    finalize(&world, &run(), &completed()).await;

    let trigger = trigger_of(&world, 0);
    assert_eq!(trigger.plan, plan(), "carried by value, never recomputed");
    assert!(trigger.completed);
    assert_eq!(trigger.tenant_id, USER_TENANT);
    assert_eq!(trigger.chat_id, CHAT);
    assert_eq!(trigger.causing_request_id, REQUEST);
}

#[tokio::test]
async fn an_incomplete_completion_is_a_completed_classification() {
    let world = TaskWorld::new();

    finalize(&world, &run(), &incomplete()).await;

    assert!(trigger_of(&world, 0).completed);
}

#[tokio::test]
async fn a_failed_or_cancelled_turn_is_not_a_completed_classification() {
    for outcome in [failed(), TurnOutcome::Cancelled] {
        let world = TaskWorld::new();

        finalize(&world, &run(), &outcome).await;

        assert!(!trigger_of(&world, 0).completed, "{outcome:?}");
    }
}

#[tokio::test]
async fn the_second_run_after_a_failed_message_insert_keeps_the_plan_and_is_not_completed() {
    let world = TaskWorld::new();
    world.turns.with(|t| {
        t.finalize_results
            .push_back(Err(FinalizeError::MessageInsert(DomainError::Database(
                "insert failed".to_owned(),
            ))));
    });

    finalize(&world, &run(), &completed()).await;

    assert_eq!(world.turns.finalizations().len(), 2);
    let (first, second) = (trigger_of(&world, 0), trigger_of(&world, 1));
    assert!(first.completed);
    assert!(
        !second.completed,
        "message_persistence_failed is a failed classification"
    );
    assert_eq!(first.plan, second.plan);
    assert_eq!(
        first.plan.system_request_id, second.plan.system_request_id,
        "the system_request_id is precomputed once"
    );
}

#[tokio::test]
async fn a_disabled_plan_is_handed_over_as_it_is() {
    let world = TaskWorld::new();
    let run = TurnRun {
        summary: SummaryTriggerPlan::disabled(),
        ..run_premium()
    };

    finalize(&world, &run, &completed()).await;

    assert!(!trigger_of(&world, 0).plan.enabled);
}

// ---- the metric after the commit -----------------------------------------------------------------------

async fn won_with(result: Option<TriggerResult>) -> TaskWorld {
    let world = TaskWorld::new();
    world.turns.with(|t| {
        let mut won = won_for(
            &crate::domain::service::quota::finalize::FinalizeRequest::orphan(
                Uuid::nil(),
                at_epoch(),
                crate::domain::service::quota::policy::SettlementSnapshot::Unavailable,
                at_epoch(),
            ),
        );
        won.terminal_state = mini_chat_sdk::TerminalState::Completed;
        won.summary_trigger = result;
        t.finalize_results
            .push_back(Ok(FinalizeResult::Won(Box::new(won))));
    });
    finalize(&world, &run(), &completed()).await;
    world
}

fn at_epoch() -> time::OffsetDateTime {
    time::OffsetDateTime::UNIX_EPOCH
}

fn trigger_metrics(world: &TaskWorld) -> Vec<StreamMetric> {
    world
        .metrics
        .events()
        .into_iter()
        .filter(|metric| matches!(metric, StreamMetric::SummaryTrigger(_)))
        .collect()
}

#[tokio::test]
async fn a_scheduled_trigger_is_recorded_after_the_won_commit() {
    let world = won_with(Some(TriggerResult::Scheduled)).await;

    assert_eq!(
        trigger_metrics(&world),
        vec![StreamMetric::SummaryTrigger("scheduled".to_owned())]
    );
}

#[tokio::test]
async fn a_not_needed_trigger_is_recorded() {
    let world = won_with(Some(TriggerResult::NotNeeded)).await;

    assert_eq!(
        trigger_metrics(&world),
        vec![StreamMetric::SummaryTrigger("not_needed".to_owned())]
    );
}

#[tokio::test]
async fn nothing_is_recorded_for_a_trigger_that_was_not_evaluated() {
    for result in [None, Some(TriggerResult::NotEvaluated)] {
        let world = won_with(result).await;

        assert!(trigger_metrics(&world).is_empty(), "{result:?}");
    }
}

#[tokio::test]
async fn a_lost_cas_or_a_failed_transaction_records_nothing() {
    let lost = TaskWorld::new();
    lost.turns
        .with(|t| t.finalize_results.push_back(Ok(FinalizeResult::Lost)));
    finalize(&lost, &run(), &completed()).await;
    let failed_tx = TaskWorld::new();
    failed_tx.turns.with(|t| {
        t.finalize_results
            .push_back(Err(FinalizeError::Other(DomainError::Database(
                "commit failed".to_owned(),
            ))));
    });
    finalize(&failed_tx, &run(), &completed()).await;

    assert!(trigger_metrics(&lost).is_empty());
    assert!(trigger_metrics(&failed_tx).is_empty());
}
