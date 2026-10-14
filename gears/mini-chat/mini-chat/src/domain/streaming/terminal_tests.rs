//! The terminal emission matrix: `citations` then `done` after a committed `completed` turn
//! (usage, models, quota decision and downgrade facts, warnings, no internal id), the
//! persistence-failure and finalization-failure errors, the original error on an error path, and
//! nothing for a lost CAS and a cancelled turn.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `terminal-emission`; state `sse-stream`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-sse-contract`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{QuotaDecision, UsageTokens};
use uuid::Uuid;

use super::finalizer::{CommittedTurn, StreamFinalization};
use super::provider_errors::generic_message;
use super::relay::{CitationItem, CitationSource, CitationsPayload, ErrorEvent, RelayEvent};
use super::replay::Done;
use super::terminal::{TerminalIntent, terminal_emission};
use crate::domain::error::{DomainError, StreamErrorCode};
use crate::domain::quota::{PeriodType, QuotaTier, QuotaWarning};
use crate::domain::turns::TurnState;
use crate::test_support::quota::STANDARD;
use crate::test_support::send::t;
use crate::test_support::streaming::PlanBuilder;

fn usage(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 9,
        cache_write_input_tokens: 8,
        reasoning_tokens: 7,
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the emission matrix takes the finalization result"
)]
fn committed(
    state: TurnState,
    error_code: Option<&str>,
) -> Result<StreamFinalization, DomainError> {
    Ok(StreamFinalization::Committed(CommittedTurn {
        state,
        error_code: error_code.map(ToOwned::to_owned),
        assistant_message_id: (state == TurnState::Completed).then(|| Uuid::from_u128(0xA551)),
    }))
}

fn citations() -> CitationsPayload {
    CitationsPayload {
        items: vec![CitationItem {
            source: CitationSource::Web,
            title: "T".to_owned(),
            url: Some("https://example.com".to_owned()),
            attachment_id: None,
            snippet: "s".to_owned(),
            span: None,
        }],
    }
}

fn warning() -> QuotaWarning {
    QuotaWarning {
        tier: QuotaTier::Total,
        period: PeriodType::Daily,
        remaining_percentage: 10,
        warning: true,
        exhausted: false,
        next_reset: Some(t(0)),
    }
}

fn failed_intent(code: StreamErrorCode) -> TerminalIntent {
    TerminalIntent::Failed {
        code,
        message: "sanitized text".to_owned(),
    }
}

fn done_of(events: &[RelayEvent]) -> &Done {
    match events.last() {
        Some(RelayEvent::Done(done)) => done,
        other => panic!("expected a trailing done, got {other:?}"),
    }
}

// ── completed ────────────────────────────────────────────────────────────────

#[test]
fn a_committed_completed_turn_emits_done_with_usage_and_the_models() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed {
            usage: Some(usage(500, 120)),
        },
        &committed(TurnState::Completed, None),
        None,
        None,
    );
    assert_eq!(events.len(), 1, "{events:?}");
    let done = done_of(&events);
    // cache and reasoning counts are never part of `done`
    assert_eq!((done.input_tokens, done.output_tokens), (500, 120));
    assert_eq!(done.effective_model, plan.decision.effective_model);
    assert_eq!(done.selected_model, plan.selected_model);
    assert_eq!(done.quota_decision, QuotaDecision::Allow);
    assert_eq!(done.downgrade_from, None);
    assert_eq!(done.downgrade_reason, None);
    assert_eq!(done.quota_warnings, None);
    assert_eq!(done.effective_model, STANDARD);
}

#[test]
fn missing_provider_usage_is_reported_as_zero() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed { usage: None },
        &committed(TurnState::Completed, None),
        None,
        None,
    );
    let done = done_of(&events);
    assert_eq!((done.input_tokens, done.output_tokens), (0, 0));
}

#[test]
fn a_downgrade_adds_the_origin_and_the_reason() {
    let plan = PlanBuilder::new().downgraded_from("premium-1").build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed {
            usage: Some(usage(1, 2)),
        },
        &committed(TurnState::Completed, None),
        None,
        None,
    );
    let done = done_of(&events);
    assert_eq!(done.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(done.selected_model, "premium-1");
    assert_eq!(done.downgrade_from.as_deref(), Some("premium-1"));
    assert_eq!(
        done.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
}

#[test]
fn citations_come_before_done_and_warnings_ride_on_done() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed {
            usage: Some(usage(1, 2)),
        },
        &committed(TurnState::Completed, None),
        Some(citations()),
        Some(vec![warning()]),
    );
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], RelayEvent::Citations(citations()));
    assert_eq!(done_of(&events).quota_warnings, Some(vec![warning()]));
}

#[test]
fn a_failed_warnings_read_sends_done_without_warnings() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed {
            usage: Some(usage(1, 2)),
        },
        &committed(TurnState::Completed, None),
        None,
        None,
    );
    assert_eq!(done_of(&events).quota_warnings, None);
}

#[test]
fn a_persistence_failure_downgrade_emits_that_error_instead_of_done_and_citations() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed {
            usage: Some(usage(1, 2)),
        },
        &committed(TurnState::Failed, Some("message_persistence_failed")),
        Some(citations()),
        None,
    );
    assert_eq!(events.len(), 1, "{events:?}");
    let RelayEvent::Error(ErrorEvent { code, message }) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(*code, StreamErrorCode::MessagePersistenceFailed);
    assert!(!message.is_empty());
}

#[test]
fn a_finalization_failure_on_a_completed_stream_emits_finalization_failed_not_done() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed {
            usage: Some(usage(1, 2)),
        },
        &Err(DomainError::internal("the enqueue failed")),
        Some(citations()),
        None,
    );
    assert_eq!(events.len(), 1, "{events:?}");
    let RelayEvent::Error(error) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(error.code, StreamErrorCode::FinalizationFailed);
    // the internal cause never reaches the client
    assert!(!error.message.contains("enqueue"), "{}", error.message);
}

// ── failed ───────────────────────────────────────────────────────────────────

#[test]
fn a_committed_failed_turn_emits_the_error_with_its_code_and_sanitized_message() {
    let plan = PlanBuilder::new().build();
    for code in [
        StreamErrorCode::ProviderError,
        StreamErrorCode::ProviderTimeout,
        StreamErrorCode::RateLimited,
        StreamErrorCode::WebSearchCallsExceeded,
        StreamErrorCode::CodeInterpreterCallsExceeded,
        StreamErrorCode::UnexpectedToolUse,
    ] {
        let events = terminal_emission(
            &plan,
            &failed_intent(code),
            &committed(TurnState::Failed, Some(code.as_str())),
            None,
            None,
        );
        assert_eq!(
            events,
            vec![RelayEvent::Error(ErrorEvent {
                code,
                message: "sanitized text".to_owned(),
            })],
            "{code:?}"
        );
    }
}

#[test]
fn a_finalization_failure_on_an_error_path_still_sends_the_original_code() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &failed_intent(StreamErrorCode::RateLimited),
        &Err(DomainError::internal("the database failed")),
        None,
        None,
    );
    assert_eq!(
        events,
        vec![RelayEvent::Error(ErrorEvent {
            code: StreamErrorCode::RateLimited,
            message: "sanitized text".to_owned(),
        })]
    );
}

// ── lost CAS and cancelled ───────────────────────────────────────────────────

#[test]
fn a_lost_cas_emits_nothing_whatever_the_intent() {
    let plan = PlanBuilder::new().build();
    for intent in [
        TerminalIntent::Completed {
            usage: Some(usage(1, 2)),
        },
        failed_intent(StreamErrorCode::ProviderError),
        TerminalIntent::Cancelled,
    ] {
        let events = terminal_emission(
            &plan,
            &intent,
            &Ok(StreamFinalization::AlreadyFinalized),
            Some(citations()),
            Some(vec![warning()]),
        );
        assert!(events.is_empty(), "{intent:?}: {events:?}");
    }
}

#[test]
fn a_cancelled_turn_emits_nothing() {
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Cancelled,
        &committed(TurnState::Cancelled, None),
        None,
        None,
    );
    assert!(events.is_empty(), "{events:?}");
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Cancelled,
        &Err(DomainError::internal("boom")),
        None,
        None,
    );
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn the_generic_text_of_the_persistence_failure_is_not_provider_text() {
    // the message of `message_persistence_failed` is the generic one of its code
    let plan = PlanBuilder::new().build();
    let events = terminal_emission(
        &plan,
        &TerminalIntent::Completed { usage: None },
        &committed(TurnState::Failed, Some("message_persistence_failed")),
        None,
        None,
    );
    let RelayEvent::Error(error) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(
        error.message,
        generic_message(StreamErrorCode::MessagePersistenceFailed)
    );
}
