#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::test_fixtures::standard;

fn bad_multiplier_model() -> mini_chat_sdk::ModelCatalogEntry {
    let mut m = standard("s-bad");
    m.output_tokens_credit_multiplier_micro = u64::MAX;
    m
}

fn completed() -> Terminal {
    Terminal::Completed {
        text: "hi".into(),
        usage: None,
        response_id: None,
        incomplete_reason: None,
    }
}

fn error_codes(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            StreamEvent::Error { code, .. } => code.clone(),
            other => panic!("unexpected event {other:?}"),
        })
        .collect()
}

#[test]
fn effective_multipliers_of_valid_entry() {
    let m = standard("s1");
    let expected = (
        i64::try_from(m.input_tokens_credit_multiplier_micro).unwrap(),
        i64::try_from(m.output_tokens_credit_multiplier_micro).unwrap(),
    );
    assert_eq!(effective_multipliers(&m).unwrap(), expected);
}

#[test]
fn effective_multipliers_out_of_range_is_an_internal_error() {
    let err = effective_multipliers(&bad_multiplier_model()).unwrap_err();
    assert!(
        matches!(&err, DomainError::Internal(m) if m.contains("s-bad")),
        "{err:?}"
    );
}

#[test]
fn multiplier_error_on_completed_stream_is_finalization_failed() {
    let err = effective_multipliers(&bad_multiplier_model()).unwrap_err();
    let events = terminal_events(Uuid::nil(), &completed(), Err(err), |_, _| {
        panic!("no done after a failed finalization")
    });
    assert_eq!(error_codes(&events), vec![FINALIZATION_FAILED.to_owned()]);
}

#[test]
fn multiplier_error_on_failed_stream_keeps_the_original_code() {
    let err = effective_multipliers(&bad_multiplier_model()).unwrap_err();
    let failed = Terminal::Failed {
        code: PROVIDER_ERROR.into(),
        detail: "boom".into(),
        usage: None,
    };
    let events = terminal_events(Uuid::nil(), &failed, Err(err), |_, _| unreachable!());
    assert_eq!(error_codes(&events), vec![PROVIDER_ERROR.to_owned()]);
}

#[test]
fn multiplier_error_on_cancelled_stream_sends_nothing() {
    let err = effective_multipliers(&bad_multiplier_model()).unwrap_err();
    let cancelled = Terminal::Cancelled {
        partial_text: String::new(),
    };
    let events = terminal_events(Uuid::nil(), &cancelled, Err(err), |_, _| unreachable!());
    assert!(events.is_empty());
}
