#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: stream finalization
//! (`cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`,
//! `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`): the request and
//! the writes handed to the CAS-guarded transaction, the terminal event and
//! the second run after a failed message INSERT.

use mini_chat_sdk::{TerminalState, TurnAuditEventType};
use time::Duration as TimeDuration;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{
    FinalizeResult, FinalizeVariant, TerminalClassification,
};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::preflight::{DowngradeReason, QuotaDecision};
use crate::domain::service::quota::test_support::{Metric, design_rows};
use crate::domain::service::quota::types::QuotaPeriod;
use crate::domain::service::stream::ports::FinalizeError;
use crate::domain::service::stream::test_support::{
    ASSISTANT, CHAT, REQUEST, StreamMetric, TURN, TaskWorld, at, run_downgraded, run_premium, usage,
};
use crate::domain::service::stream::types::ErrorEvent;
use crate::test_doubles::{LogCapture, USER, USER_TENANT};

fn completed(input: u64, output: u64) -> TurnOutcome {
    TurnOutcome::Completed {
        usage: Some(usage(input, output)),
        response_id: Some("resp_internal".to_owned()),
        incomplete_reason: None,
    }
}

fn failed(code: StreamErrorCode) -> TurnOutcome {
    TurnOutcome::Failed {
        code,
        message: "sanitized message".to_owned(),
        detail: Some("provider_error: sanitized message".to_owned()),
        usage: Some(usage(10, 0)),
        response_id: Some("resp_failed".to_owned()),
    }
}

async fn run_finalize(
    world: &TaskWorld,
    run: &TurnRun,
    outcome: &TurnOutcome,
    text: &str,
    counts: ToolCounts,
) -> FinalizeReport {
    finalize_stream(
        &world.deps,
        &FinalizeInput {
            run,
            outcome,
            text,
            counts,
            ttft_ms: Some(150),
        },
    )
    .await
}

async fn simple(outcome: &TurnOutcome, text: &str) -> (TaskWorld, FinalizeReport) {
    let world = TaskWorld::new();
    let report = run_finalize(&world, &run_premium(), outcome, text, ToolCounts::default()).await;
    (world, report)
}

// ---- completed ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_turn_hands_the_cas_request_to_the_transaction() {
    let world = TaskWorld::new();
    let run = run_premium();
    let counts = ToolCounts {
        web_search: 1,
        code_interpreter: 2,
        native_file_search: 3,
        knowledge_calls: 0,
    };
    run_finalize(&world, &run, &completed(100, 40), "Hello", counts).await;
    let (request, _) = world.turns.finalizations().remove(0);
    assert_eq!(request.turn_id, TURN);
    assert_eq!(
        request.variant,
        FinalizeVariant::Stream {
            tenant_id: USER_TENANT,
            period_keys: run.decision.period_keys,
            selected_model: "p".to_owned(),
        }
    );
    assert_eq!(request.classification, TerminalClassification::Completed);
    assert_eq!(request.columns.state, TerminalState::Completed);
    assert_eq!(request.columns.error_code, None);
    assert_eq!(request.columns.error_detail, None);
    assert_eq!(request.columns.assistant_message_id, Some(ASSISTANT));
    assert_eq!(
        request.columns.provider_response_id.as_deref(),
        Some("resp_internal")
    );
    assert_eq!(request.usage, Some(usage(100, 40)));
    assert_eq!(
        (
            request.tool_calls.web_search,
            request.tool_calls.code_interpreter,
            request.tool_calls.file_search
        ),
        (1, 2, 3)
    );
    assert_eq!(request.now, at(0));
}

#[tokio::test]
async fn the_settlement_snapshot_is_the_preflight_snapshot() {
    let world = TaskWorld::new();
    let run = run_premium();
    run_finalize(&world, &run, &completed(1, 1), "x", ToolCounts::default()).await;
    let (request, _) = world.turns.finalizations().remove(0);
    assert_eq!(
        request.snapshot,
        SettlementSnapshot::Available(Box::new(run.decision.snapshot.clone()))
    );
}

#[tokio::test]
async fn a_completed_turn_persists_the_full_text_as_the_assistant_message() {
    let world = TaskWorld::new();
    let run = run_premium();
    run_finalize(
        &world,
        &run,
        &completed(100, 40),
        "Hello world",
        ToolCounts::default(),
    )
    .await;
    let (_, writes) = world.turns.finalizations().remove(0);
    let message = writes.assistant_message.expect("a message is persisted");
    assert_eq!(message.id, ASSISTANT);
    assert_eq!(message.tenant_id, USER_TENANT);
    assert_eq!(message.chat_id, CHAT);
    assert_eq!(message.request_id, REQUEST);
    assert_eq!(message.content, "Hello world");
    assert_eq!(
        message.provider_response_id.as_deref(),
        Some("resp_internal")
    );
    assert_eq!(message.usage, Some(usage(100, 40)));
    assert_eq!(message.model, "p", "the effective model");
}

#[tokio::test]
async fn a_completed_turn_without_text_still_persists_an_empty_message() {
    let (world, report) = simple(&completed(5, 0), "").await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.columns.assistant_message_id, Some(ASSISTANT));
    assert_eq!(writes.assistant_message.unwrap().content, "");
    assert_eq!(report.state, Some(TurnState::Completed));
}

#[tokio::test]
async fn an_incomplete_response_finalizes_as_completed_without_an_error_code() {
    let outcome = TurnOutcome::Completed {
        usage: Some(usage(10, 20)),
        response_id: None,
        incomplete_reason: Some("max_output_tokens".to_owned()),
    };
    let (world, report) = simple(&outcome, "cut off").await;
    let (request, _) = world.turns.finalizations().remove(0);
    assert_eq!(request.classification, TerminalClassification::Completed);
    assert_eq!(request.columns.state, TerminalState::Completed);
    assert_eq!(request.columns.error_code, None);
    assert_eq!(report.state, Some(TurnState::Completed));
    assert!(matches!(report.event, Some(StreamEvent::Done(_))));
}

#[tokio::test]
async fn the_assistant_message_follows_the_user_message_by_at_least_a_microsecond() {
    // The clock reads noon; the user message was written at the same instant.
    let world = TaskWorld::new();
    let mut run = run_premium();
    run.user_message_at = at(0);
    run_finalize(&world, &run, &completed(1, 1), "x", ToolCounts::default()).await;
    let created = world.turns.finalizations()[0]
        .1
        .assistant_message
        .clone()
        .unwrap()
        .created_at;
    assert_eq!(created, at(0) + TimeDuration::microseconds(1));
}

#[tokio::test]
async fn a_later_clock_reading_is_the_assistant_message_time() {
    let world = TaskWorld::new();
    let mut run = run_premium();
    run.user_message_at = at(-5);
    run_finalize(&world, &run, &completed(1, 1), "x", ToolCounts::default()).await;
    let created = world.turns.finalizations()[0]
        .1
        .assistant_message
        .clone()
        .unwrap()
        .created_at;
    assert_eq!(created, at(0));
}

#[tokio::test]
async fn a_clock_behind_the_user_message_is_overridden() {
    let world = TaskWorld::new();
    let mut run = run_premium();
    run.user_message_at = at(10);
    run_finalize(&world, &run, &completed(1, 1), "x", ToolCounts::default()).await;
    let created = world.turns.finalizations()[0]
        .1
        .assistant_message
        .clone()
        .unwrap()
        .created_at;
    assert_eq!(created, at(10) + TimeDuration::microseconds(1));
}

// ---- the audit event ----------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_turn_enqueues_a_turn_completed_audit_event_without_content() {
    let world = TaskWorld::new();
    let run = run_premium();
    let counts = ToolCounts {
        web_search: 2,
        code_interpreter: 4,
        native_file_search: 1,
        knowledge_calls: 0,
    };
    run_finalize(&world, &run, &completed(100, 40), "secret answer", counts).await;
    let audit = world.turns.finalizations()[0].1.audit.clone();
    assert_eq!(audit.event_type, TurnAuditEventType::TurnCompleted);
    assert_eq!(audit.tenant_id, USER_TENANT);
    assert_eq!(audit.requester_user_id, USER);
    assert_eq!(audit.chat_id, CHAT);
    assert_eq!(audit.turn_id, TURN);
    assert_eq!(audit.request_id, REQUEST);
    assert_eq!(audit.selected_model, "p");
    assert_eq!(audit.effective_model, "p");
    assert_eq!(
        (audit.usage.input_tokens, audit.usage.output_tokens),
        (100, 40)
    );
    assert_eq!(
        (audit.tool_calls.web_search, audit.tool_calls.file_search),
        (2, 1)
    );
    assert_eq!(audit.policy_decisions.quota.decision, "allow");
    assert_eq!(audit.timestamp, at(0));
    assert_eq!(audit.prompt, "");
    assert_eq!(audit.response, "");
    assert!(audit.attachments.is_empty());
    assert_eq!(audit.license, None);
    assert_eq!(audit.quota_scope, None);
}

#[tokio::test]
async fn failed_and_cancelled_turns_are_turn_failed_audit_events() {
    for outcome in [
        failed(StreamErrorCode::ProviderError),
        TurnOutcome::Cancelled,
    ] {
        let (world, _) = simple(&outcome, "partial").await;
        let audit = &world.turns.finalizations()[0].1.audit;
        assert_eq!(
            audit.event_type,
            TurnAuditEventType::TurnFailed,
            "{outcome:?}"
        );
    }
}

#[tokio::test]
async fn the_audit_event_of_a_downgraded_turn_says_downgrade() {
    let world = TaskWorld::new();
    run_finalize(
        &world,
        &run_downgraded(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    let audit = &world.turns.finalizations()[0].1.audit;
    assert_eq!(audit.selected_model, "p");
    assert_eq!(audit.effective_model, "s");
    assert_eq!(audit.policy_decisions.quota.decision, "downgrade");
}

#[tokio::test(start_paused = true)]
async fn the_audit_latency_runs_from_the_acceptance_to_the_finalization() {
    let world = TaskWorld::new();
    let run = run_premium();
    tokio::time::sleep(std::time::Duration::from_millis(1234)).await;
    run_finalize(&world, &run, &completed(1, 1), "x", ToolCounts::default()).await;
    assert_eq!(world.turns.finalizations()[0].1.audit.latency_ms, 1234);
}

#[tokio::test]
async fn the_file_search_count_is_the_native_count_when_knowledge_search_was_not_offered() {
    let world = TaskWorld::new();
    let counts = ToolCounts {
        native_file_search: 2,
        knowledge_calls: 1,
        ..ToolCounts::default()
    };
    run_finalize(&world, &run_premium(), &completed(1, 1), "x", counts).await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.tool_calls.file_search, 2);
    assert_eq!(writes.audit.tool_calls.file_search, 2);
}

#[tokio::test]
async fn the_file_search_count_is_the_knowledge_call_count_when_it_was_offered() {
    let world = TaskWorld::new();
    let mut run = run_premium();
    run.tools.knowledge = Some(crate::domain::service::stream::tools::KnowledgeParams {
        max_calls_per_message: 3,
        top_k: 5,
        max_chunk_chars: 2000,
    });
    let counts = ToolCounts {
        native_file_search: 2,
        knowledge_calls: 1,
        ..ToolCounts::default()
    };
    run_finalize(&world, &run, &completed(1, 1), "x", counts).await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.tool_calls.file_search, 1, "selected, never summed");
    assert_eq!(writes.audit.tool_calls.file_search, 1);
}

// ---- the terminal event of a completed turn -----------------------------------------------------

#[tokio::test]
async fn a_won_completed_turn_reports_done_with_the_usage_and_the_decision() {
    let (_, report) = simple(&completed(100, 40), "Hello").await;
    assert_eq!(report.state, Some(TurnState::Completed));
    let Some(StreamEvent::Done(done)) = report.event else {
        panic!("done expected, got {:?}", report.event);
    };
    assert_eq!(done.usage, (100, 40));
    assert_eq!(done.effective_model, "p");
    assert_eq!(done.selected_model, "p");
    assert_eq!(done.quota_decision, QuotaDecision::Allow);
    assert_eq!(done.downgrade_from, None);
    assert_eq!(done.downgrade_reason, None);
    // The warnings of the quota view are always present on a won completed
    // turn; without usage nothing warns and no entry carries a reset time.
    let warnings = done.quota_warnings.expect("present");
    assert!(
        warnings
            .iter()
            .all(|w| !w.warning && !w.exhausted && w.next_reset.is_none()),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn done_after_a_downgrade_names_the_origin_and_the_reason() {
    let world = TaskWorld::new();
    let report = run_finalize(
        &world,
        &run_downgraded(),
        &completed(1, 2),
        "x",
        ToolCounts::default(),
    )
    .await;
    let Some(StreamEvent::Done(done)) = report.event else {
        panic!("done expected");
    };
    assert_eq!(done.effective_model, "s");
    assert_eq!(done.selected_model, "p");
    assert_eq!(done.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(done.downgrade_from.as_deref(), Some("p"));
    assert_eq!(
        done.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

#[tokio::test]
async fn done_without_provider_usage_reports_zero_tokens() {
    let outcome = TurnOutcome::Completed {
        usage: None,
        response_id: None,
        incomplete_reason: None,
    };
    let (_, report) = simple(&outcome, "x").await;
    let Some(StreamEvent::Done(done)) = report.event else {
        panic!("done expected");
    };
    assert_eq!(done.usage, (0, 0));
}

#[tokio::test]
async fn quota_warnings_use_the_limits_of_the_preflight_and_the_current_buckets() {
    let world = TaskWorld::new();
    *world.quota_repo.buckets.lock().unwrap() = design_rows();
    let report = run_finalize(
        &world,
        &run_premium(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    let Some(StreamEvent::Done(done)) = report.event else {
        panic!("done expected");
    };
    let warnings = done.quota_warnings.expect("warnings are present");
    assert!(warnings.iter().any(|w| w.warning), "{warnings:?}");
    assert!(
        warnings
            .iter()
            .all(|w| !w.warning || w.next_reset.is_some())
    );
}

#[tokio::test]
async fn a_failed_warning_read_omits_the_key_logs_and_still_sends_done() {
    let logs = LogCapture::start();
    let world = TaskWorld::new();
    world.quota_repo.fail_reads();
    let report = run_finalize(
        &world,
        &run_premium(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    assert_eq!(
        report.state,
        Some(TurnState::Completed),
        "the turn is committed"
    );
    let Some(StreamEvent::Done(done)) = report.event else {
        panic!("done expected");
    };
    assert_eq!(done.quota_warnings, None);
    assert!(
        !logs.at(tracing::Level::ERROR).is_empty() || !logs.at(tracing::Level::WARN).is_empty()
    );
}

// ---- failed -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_turn_writes_the_error_columns_and_no_message() {
    let (world, report) = simple(&failed(StreamErrorCode::ProviderError), "partial text").await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(
        request.classification,
        TerminalClassification::Failed {
            error_code: "provider_error".to_owned()
        }
    );
    assert_eq!(request.columns.state, TerminalState::Failed);
    assert_eq!(
        request.columns.error_code.as_deref(),
        Some("provider_error")
    );
    assert_eq!(
        request.columns.error_detail.as_deref(),
        Some("provider_error: sanitized message")
    );
    assert_eq!(request.columns.assistant_message_id, None);
    assert_eq!(
        request.columns.provider_response_id.as_deref(),
        Some("resp_failed")
    );
    assert_eq!(request.usage, Some(usage(10, 0)));
    assert_eq!(
        writes.assistant_message, None,
        "no message for a failed turn"
    );
    assert_eq!(report.state, Some(TurnState::Failed));
}

#[tokio::test]
async fn a_failed_turn_reports_the_error_event_with_the_code_and_the_message() {
    let (_, report) = simple(&failed(StreamErrorCode::RateLimited), "").await;
    assert_eq!(
        report.event,
        Some(StreamEvent::Error(ErrorEvent {
            code: StreamErrorCode::RateLimited,
            message: "sanitized message".to_owned(),
        }))
    );
}

#[tokio::test]
async fn a_failure_without_provider_detail_has_a_null_error_detail() {
    let outcome = TurnOutcome::Failed {
        code: StreamErrorCode::WebSearchCallsExceeded,
        message: StreamErrorCode::WebSearchCallsExceeded
            .generic_message()
            .to_owned(),
        detail: None,
        usage: None,
        response_id: None,
    };
    let (world, _) = simple(&outcome, "").await;
    let (request, _) = world.turns.finalizations().remove(0);
    assert_eq!(request.columns.error_detail, None);
    assert_eq!(request.usage, None);
    assert_eq!(request.columns.provider_response_id, None);
}

// ---- cancelled ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_turn_with_text_persists_the_partial_message_and_no_usage() {
    let (world, report) = simple(&TurnOutcome::Cancelled, "partial").await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.classification, TerminalClassification::Cancelled);
    assert_eq!(request.columns.state, TerminalState::Cancelled);
    assert_eq!(request.columns.error_code, None);
    assert_eq!(request.columns.assistant_message_id, Some(ASSISTANT));
    assert_eq!(request.usage, None);
    let message = writes.assistant_message.unwrap();
    assert_eq!(message.content, "partial");
    assert_eq!(message.usage, None);
    assert_eq!(report.state, Some(TurnState::Cancelled));
    assert_eq!(report.event, None, "nothing is sent after a disconnect");
}

#[tokio::test]
async fn a_cancelled_turn_without_text_persists_no_message() {
    let (world, report) = simple(&TurnOutcome::Cancelled, "").await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.columns.assistant_message_id, None);
    assert_eq!(writes.assistant_message, None);
    assert_eq!(report.state, Some(TurnState::Cancelled));
}

#[tokio::test]
async fn a_won_cancellation_records_the_cancel_metrics() {
    let (world, _) = simple(&TurnOutcome::Cancelled, "x").await;
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelEffective),
        1
    );
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::StreamsAborted),
        1
    );
}

// ---- the lost CAS -------------------------------------------------------------------------------

#[tokio::test]
async fn a_lost_cas_settles_nothing_and_sends_no_terminal_event() {
    for outcome in [
        completed(1, 1),
        failed(StreamErrorCode::ProviderError),
        TurnOutcome::Cancelled,
    ] {
        let world = TaskWorld::new();
        world
            .turns
            .with(|t| t.finalize_results.push_back(Ok(FinalizeResult::Lost)));
        let report =
            run_finalize(&world, &run_premium(), &outcome, "x", ToolCounts::default()).await;
        assert_eq!(
            report,
            FinalizeReport {
                state: None,
                event: None
            },
            "{outcome:?}"
        );
        assert!(
            world.quota_metrics.events().is_empty(),
            "no settlement metrics"
        );
        assert_eq!(
            world.metrics.count(|m| matches!(
                m,
                StreamMetric::FinalizationLatency
                    | StreamMetric::CancelEffective
                    | StreamMetric::StreamsAborted
            )),
            0
        );
        assert_eq!(
            world.quota_repo.calls().len(),
            0,
            "no warnings are read for a lost CAS"
        );
    }
}

// ---- metrics and the cost record ----------------------------------------------------------------

#[tokio::test]
async fn a_won_finalization_records_the_settlement_and_the_finalization_latency() {
    let (world, _) = simple(&completed(100, 40), "x").await;
    let events = world.quota_metrics.events();
    assert!(
        events.contains(&Metric::Commit(QuotaPeriod::Daily)),
        "{events:?}"
    );
    assert!(events.contains(&Metric::Commit(QuotaPeriod::Monthly)));
    assert!(events.contains(&Metric::Actual(140)));
    assert_eq!(
        world
            .metrics
            .count(|m| *m == StreamMetric::FinalizationLatency),
        1
    );
}

#[tokio::test]
async fn a_won_finalization_writes_one_turn_finalized_record() {
    let logs = LogCapture::start();
    let world = TaskWorld::new();
    let counts = ToolCounts {
        web_search: 1,
        code_interpreter: 2,
        native_file_search: 3,
        knowledge_calls: 0,
    };
    run_finalize(&world, &run_premium(), &completed(100, 40), "x", counts).await;
    let records = logs.at_containing(tracing::Level::INFO, "turn finalized");
    assert_eq!(records.len(), 1, "{:?}", logs.records());
    let text = records[0].text();
    for expected in [
        format!("chat_id={CHAT}"),
        format!("turn_id={TURN}"),
        format!("request_id={REQUEST}"),
        "provider_id=openai".to_owned(),
        "effective_model=p".to_owned(),
        "state=completed".to_owned(),
        "input_tokens=100".to_owned(),
        "output_tokens=40".to_owned(),
        "web_search_calls=1".to_owned(),
        "file_search_calls=3".to_owned(),
        "code_interpreter_calls=2".to_owned(),
        "ttft_ms=150".to_owned(),
    ] {
        assert!(text.contains(&expected), "`{expected}` missing in: {text}");
    }
    assert!(text.contains("latency_ms="), "{text}");
    assert!(
        !text.contains(&USER.to_string()),
        "no user id in the cost record"
    );
    assert!(
        !text.contains(&USER_TENANT.to_string()),
        "no tenant id in the cost record"
    );
}

#[tokio::test]
async fn a_failed_turn_finalized_record_carries_the_error_code() {
    let logs = LogCapture::start();
    simple(&failed(StreamErrorCode::ProviderTimeout), "").await;
    let records = logs.at_containing(tracing::Level::INFO, "turn finalized");
    assert_eq!(records.len(), 1);
    let text = records[0].text();
    assert!(text.contains("state=failed"), "{text}");
    assert!(text.contains("error_code=provider_timeout"), "{text}");
}

#[tokio::test]
async fn a_lost_cas_writes_no_turn_finalized_record() {
    let logs = LogCapture::start();
    let world = TaskWorld::new();
    world
        .turns
        .with(|t| t.finalize_results.push_back(Ok(FinalizeResult::Lost)));
    run_finalize(
        &world,
        &run_premium(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    assert!(
        logs.at_containing(tracing::Level::INFO, "turn finalized")
            .is_empty()
    );
}

// ---- failures of the transaction ----------------------------------------------------------------

fn insert_error() -> FinalizeError {
    FinalizeError::MessageInsert(DomainError::Database("insert failed".to_owned()))
}

fn other_error() -> FinalizeError {
    FinalizeError::Other(DomainError::Database("commit failed".to_owned()))
}

#[tokio::test]
async fn a_failed_message_insert_refinalizes_as_message_persistence_failed_keeping_the_usage() {
    let world = TaskWorld::new();
    world
        .turns
        .with(|t| t.finalize_results.push_back(Err(insert_error())));
    let report = run_finalize(
        &world,
        &run_premium(),
        &completed(100, 40),
        "answer",
        ToolCounts::default(),
    )
    .await;
    let finalizations = world.turns.finalizations();
    assert_eq!(finalizations.len(), 2, "a second, message-free run");
    let (second, writes) = &finalizations[1];
    assert_eq!(
        second.classification,
        TerminalClassification::Failed {
            error_code: "message_persistence_failed".to_owned()
        }
    );
    assert_eq!(second.columns.state, TerminalState::Failed);
    assert_eq!(
        second.columns.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert_eq!(second.columns.assistant_message_id, None);
    assert_eq!(second.usage, Some(usage(100, 40)), "the usage is kept");
    assert_eq!(writes.assistant_message, None);
    assert_eq!(writes.audit.event_type, TurnAuditEventType::TurnFailed);
    assert_eq!(report.state, Some(TurnState::Failed));
    let Some(StreamEvent::Error(event)) = report.event else {
        panic!("error expected");
    };
    assert_eq!(event.code, StreamErrorCode::MessagePersistenceFailed);
}

#[tokio::test]
async fn a_second_failure_after_a_failed_message_insert_is_finalization_failed() {
    let world = TaskWorld::new();
    world.turns.with(|t| {
        t.finalize_results.push_back(Err(insert_error()));
        t.finalize_results.push_back(Err(other_error()));
    });
    let report = run_finalize(
        &world,
        &run_premium(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    assert_eq!(report.state, None, "the turn stays running");
    let Some(StreamEvent::Error(event)) = report.event else {
        panic!("error expected");
    };
    assert_eq!(event.code, StreamErrorCode::FinalizationFailed);
}

#[tokio::test]
async fn a_second_run_that_loses_the_cas_sends_nothing() {
    let world = TaskWorld::new();
    world.turns.with(|t| {
        t.finalize_results.push_back(Err(insert_error()));
        t.finalize_results.push_back(Ok(FinalizeResult::Lost));
    });
    let report = run_finalize(
        &world,
        &run_premium(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    assert_eq!(
        report,
        FinalizeReport {
            state: None,
            event: None
        }
    );
}

#[tokio::test]
async fn a_failed_transaction_of_a_completed_turn_is_finalization_failed() {
    let world = TaskWorld::new();
    world
        .turns
        .with(|t| t.finalize_results.push_back(Err(other_error())));
    let report = run_finalize(
        &world,
        &run_premium(),
        &completed(1, 1),
        "x",
        ToolCounts::default(),
    )
    .await;
    assert_eq!(world.turns.finalizations().len(), 1, "no second run");
    assert_eq!(report.state, None);
    let Some(StreamEvent::Error(event)) = &report.event else {
        panic!("error expected");
    };
    assert_eq!(event.code, StreamErrorCode::FinalizationFailed);
}

#[tokio::test]
async fn a_failed_transaction_of_a_failed_turn_reports_the_original_code() {
    let world = TaskWorld::new();
    world
        .turns
        .with(|t| t.finalize_results.push_back(Err(other_error())));
    let report = run_finalize(
        &world,
        &run_premium(),
        &failed(StreamErrorCode::RateLimited),
        "",
        ToolCounts::default(),
    )
    .await;
    assert_eq!(report.state, None);
    assert_eq!(
        report.event,
        Some(StreamEvent::Error(ErrorEvent {
            code: StreamErrorCode::RateLimited,
            message: "sanitized message".to_owned(),
        }))
    );
}

#[tokio::test]
async fn a_failed_transaction_of_a_cancelled_turn_sends_nothing_and_leaves_it_running() {
    let world = TaskWorld::new();
    world
        .turns
        .with(|t| t.finalize_results.push_back(Err(other_error())));
    let report = run_finalize(
        &world,
        &run_premium(),
        &TurnOutcome::Cancelled,
        "partial",
        ToolCounts::default(),
    )
    .await;
    assert_eq!(
        report,
        FinalizeReport {
            state: None,
            event: None
        }
    );
    assert_eq!(world.turns.finalizations().len(), 1);
}

#[tokio::test]
async fn a_failed_partial_message_insert_is_logged_and_refinalized_as_cancelled_without_it() {
    let logs = LogCapture::start();
    let world = TaskWorld::new();
    world
        .turns
        .with(|t| t.finalize_results.push_back(Err(insert_error())));
    let report = run_finalize(
        &world,
        &run_premium(),
        &TurnOutcome::Cancelled,
        "partial",
        ToolCounts::default(),
    )
    .await;
    let finalizations = world.turns.finalizations();
    assert_eq!(finalizations.len(), 2);
    let (second, writes) = &finalizations[1];
    assert_eq!(second.classification, TerminalClassification::Cancelled);
    assert_eq!(second.columns.state, TerminalState::Cancelled);
    assert_eq!(second.columns.assistant_message_id, None);
    assert_eq!(writes.assistant_message, None);
    assert_eq!(report.state, Some(TurnState::Cancelled));
    assert_eq!(report.event, None);
    assert!(
        !logs.at(tracing::Level::WARN).is_empty(),
        "a warning is logged"
    );
}
