#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The setup failure marking: the guarded mark with bounded attempts, a benign
//! zero-row update, an exhausted mark that is logged and leaves the turn to the
//! orphan watchdog, and the drop guard that marks the turn after a panic.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::Level;
use uuid::Uuid;

use super::{FailedSetup, MARK_ATTEMPTS, MARK_BACKOFF, SetupGuard, fail_setup};
use crate::domain::error::DomainError;
use crate::domain::service::mutations::ports::{MutationMetrics, MutationRepository};
use crate::domain::service::mutations::test_support::{
    FakeMutations, MutationCall, RecordingMutationMetrics,
};
use crate::domain::service::mutations::types::{MutationOp, MutationResult, SetupFailureCode};
use crate::domain::service::stream::test_support::{CHAT, at};
use crate::test_doubles::{LogCapture, USER_TENANT};

const NEW_TURN: Uuid = Uuid::from_u128(0xB200);
const NEW_REQUEST: Uuid = Uuid::from_u128(0xA200);

fn failed(code: SetupFailureCode) -> FailedSetup {
    FailedSetup {
        tenant_id: USER_TENANT,
        chat_id: CHAT,
        turn_id: NEW_TURN,
        request_id: NEW_REQUEST,
        code,
        at: at(5),
    }
}

fn database_error() -> DomainError {
    DomainError::Database("the mark failed".to_owned())
}

#[test]
fn the_mark_is_tried_three_times_with_the_20_and_100_millisecond_backoff() {
    assert_eq!(MARK_ATTEMPTS, 3);
    assert_eq!(
        MARK_BACKOFF,
        [Duration::from_millis(20), Duration::from_millis(100)]
    );
}

#[tokio::test]
async fn the_turn_is_marked_with_its_code_the_tenant_and_the_current_time() {
    for code in [
        SetupFailureCode::ContextLengthExceeded,
        SetupFailureCode::TurnSetupFailed,
        SetupFailureCode::QuotaExceeded,
    ] {
        let store = FakeMutations::new();

        fail_setup(&*store, &failed(code)).await;

        assert_eq!(
            store.calls(),
            vec![MutationCall::Mark {
                tenant: USER_TENANT,
                turn: NEW_TURN,
                code: code.as_str().to_owned(),
                at: at(5),
            }]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_failing_mark_is_retried_and_stops_at_the_first_success() {
    let store = FakeMutations::new();
    store.with(|inner| {
        inner.mark_results.push_back(Err(database_error()));
        inner.mark_results.push_back(Err(database_error()));
        inner.mark_results.push_back(Ok(1));
    });
    let logs = LogCapture::start();

    fail_setup(&*store, &failed(SetupFailureCode::TurnSetupFailed)).await;

    assert_eq!(store.marks().len(), 3);
    assert!(logs.at(Level::ERROR).is_empty(), "the third attempt worked");
}

#[tokio::test(start_paused = true)]
async fn a_mark_that_always_fails_is_tried_exactly_the_bounded_number_of_times() {
    let store = FakeMutations::new();
    store.with(|inner| {
        for _ in 0..10 {
            inner.mark_results.push_back(Err(database_error()));
        }
    });
    let started = Instant::now();

    fail_setup(&*store, &failed(SetupFailureCode::TurnSetupFailed)).await;

    assert_eq!(store.marks().len(), MARK_ATTEMPTS as usize);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(120) && waited < Duration::from_secs(1),
        "20 ms and 100 ms between the attempts, got {waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_exhausted_mark_is_logged_at_error_with_the_ids_and_never_with_content() {
    let store = FakeMutations::new();
    store.with(|inner| {
        for _ in 0..3 {
            inner.mark_results.push_back(Err(database_error()));
        }
    });
    let logs = LogCapture::start();

    fail_setup(&*store, &failed(SetupFailureCode::TurnSetupFailed)).await;

    let errors = logs.at(Level::ERROR);
    assert_eq!(errors.len(), 1, "{errors:?}");
    let text = errors[0].text();
    for id in [CHAT, NEW_TURN, NEW_REQUEST] {
        assert!(text.contains(&id.to_string()), "`{text}` names {id}");
    }
}

#[tokio::test(start_paused = true)]
async fn zero_updated_rows_is_benign_logged_at_debug_and_not_retried() {
    let store = FakeMutations::new();
    store.with(|inner| inner.mark_results.push_back(Ok(0)));
    let logs = LogCapture::start();

    fail_setup(&*store, &failed(SetupFailureCode::QuotaExceeded)).await;

    assert_eq!(store.marks().len(), 1, "no retry for 0 rows");
    assert!(logs.at(Level::ERROR).is_empty());
    assert!(logs.at(Level::WARN).is_empty());
    assert!(
        !logs.at(Level::DEBUG).is_empty(),
        "the benign outcome is logged at debug"
    );
}

#[tokio::test]
async fn fail_setup_records_no_metric_the_service_records_once_per_request() {
    // The port of fail_setup has no metrics sink: the single recording point is
    // the service (T6-ASID-001). This test pins the signature.
    let store = FakeMutations::new();

    fail_setup(&*store, &failed(SetupFailureCode::TurnSetupFailed)).await;

    assert_eq!(store.marks().len(), 1);
}

// ---- the drop guard --------------------------------------------------------------------------------

async fn settle() {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_dropped_armed_guard_marks_the_turn_failed_with_turn_setup_failed() {
    let store = FakeMutations::new();
    let metrics = RecordingMutationMetrics::new();

    let guard = SetupGuard::arm(
        Arc::clone(&store) as Arc<dyn MutationRepository>,
        Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
        MutationOp::Retry,
        failed(SetupFailureCode::QuotaExceeded),
    );
    drop(guard);
    settle().await;

    assert_eq!(
        store.marks(),
        vec![(NEW_TURN, "turn_setup_failed".to_owned())],
        "a panic is always `turn_setup_failed`, whatever code the guard was armed with"
    );
    assert_eq!(
        metrics.results(),
        vec![(MutationOp::Retry, MutationResult::SetupFailed)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_disarmed_guard_marks_nothing() {
    let store = FakeMutations::new();
    let metrics = RecordingMutationMetrics::new();

    let guard = SetupGuard::arm(
        Arc::clone(&store) as Arc<dyn MutationRepository>,
        Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
        MutationOp::Edit,
        failed(SetupFailureCode::TurnSetupFailed),
    );
    guard.disarm();
    settle().await;

    assert!(store.marks().is_empty());
    assert!(metrics.results().is_empty());
    // The same guard, left armed, does mark: the disarm is what stopped it.
    drop(SetupGuard::arm(
        Arc::clone(&store) as Arc<dyn MutationRepository>,
        Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
        MutationOp::Edit,
        failed(SetupFailureCode::TurnSetupFailed),
    ));
    settle().await;
    assert_eq!(store.marks().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_guard_of_a_panicking_task_marks_the_turn() {
    let store = FakeMutations::new();
    let metrics = RecordingMutationMetrics::new();
    let (repo, sink) = (
        Arc::clone(&store) as Arc<dyn MutationRepository>,
        Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
    );

    let task = tokio::spawn(async move {
        let _guard = SetupGuard::arm(
            repo,
            sink,
            MutationOp::Edit,
            failed(SetupFailureCode::TurnSetupFailed),
        );
        panic!("a bug after the commit");
    });
    assert!(task.await.unwrap_err().is_panic());
    settle().await;

    assert_eq!(
        store.marks(),
        vec![(NEW_TURN, "turn_setup_failed".to_owned())]
    );
}
