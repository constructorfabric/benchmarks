//! The in-request indexing wait: the add call with the single `attachment_id` attribute, the
//! 250 ms to 2 s doubling schedule, the 25 s deadline counted from the request start, the status
//! mapping, transient versus other read errors, and the read timeout that ends at the deadline.
//! The waits run on the injected virtual timer.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-indexing-lifecycle`; state
//! `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::{IndexStatus, IndexedFile, IndexingOutcome, classify_status, wait_for_indexing};
use crate::domain::attachments::provider::ProviderFailure;
use crate::domain::attachments::timing::Timer;
use crate::test_support::attach_fakes::{ProviderCall, ScriptedProvider, openai_target};
use crate::test_support::manual_time::ManualTime;
use crate::test_support::quota::default_now;

const MS: fn(u64) -> Duration = Duration::from_millis;

struct Harness {
    time: Arc<ManualTime>,
    provider: Arc<ScriptedProvider>,
    file: IndexedFile,
}

fn harness() -> Harness {
    let time = ManualTime::auto(default_now());
    let provider = ScriptedProvider::new(&time);
    Harness {
        time,
        provider,
        file: IndexedFile {
            vector_store_id: "vs_1".to_owned(),
            provider_file_id: "file-9".to_owned(),
            attachment_id: Uuid::new_v4(),
        },
    }
}

impl Harness {
    async fn wait_from(&self, started_at: Duration) -> IndexingOutcome {
        wait_for_indexing(
            &*self.provider,
            &*self.time,
            &openai_target(),
            &self.file,
            started_at,
        )
        .await
    }

    async fn wait(&self) -> IndexingOutcome {
        self.wait_from(self.time.monotonic()).await
    }

    fn read_times(&self) -> Vec<Duration> {
        self.provider
            .reads()
            .into_iter()
            .map(|(at, _)| at)
            .collect()
    }
}

// ── status mapping ───────────────────────────────────────────────────────────

#[test]
fn provider_status_texts_map_to_the_three_indexing_states() {
    // inst-iw-initial-status, inst-iw-return-failed
    assert_eq!(classify_status(None), IndexStatus::InProgress);
    assert_eq!(
        classify_status(Some("in_progress")),
        IndexStatus::InProgress
    );
    assert_eq!(classify_status(Some("completed")), IndexStatus::Completed);
    for failed in [
        "failed",
        "cancelled",
        "expired",
        "",
        "COMPLETED",
        "whatever",
    ] {
        assert_eq!(
            classify_status(Some(failed)),
            IndexStatus::Failed,
            "{failed:?}"
        );
    }
}

// ── the add call ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_file_is_added_once_with_the_attachment_id() {
    // inst-iw-add: provider file id and the single attribute attachment_id
    let h = harness();
    h.provider.queue_add_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    let calls = h.provider.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0].1,
        ProviderCall::Add {
            vector_store_id: "vs_1".to_owned(),
            file_id: "file-9".to_owned(),
            attachment_id: h.file.attachment_id,
        }
    );
}

#[tokio::test]
async fn a_failed_add_call_fails_the_indexing_without_polling_or_retry() {
    // inst-iw-add-failed, inst-iw-return-add-failed: the call is not retried
    for failure in [ProviderFailure::Other, ProviderFailure::Transient] {
        let h = harness();
        h.provider.queue_add(Err(failure));
        assert_eq!(h.wait().await, IndexingOutcome::Failed, "{failure:?}");
        assert_eq!(h.provider.calls().len(), 1, "{failure:?}");
        assert!(h.time.sleeps().is_empty(), "{failure:?}");
    }
}

#[tokio::test]
async fn a_completed_add_response_is_ready_without_any_wait() {
    let h = harness();
    h.provider.queue_add_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    assert!(h.provider.reads().is_empty());
    assert!(h.time.sleeps().is_empty());
}

#[tokio::test]
async fn a_failed_or_unknown_add_response_status_fails_the_indexing() {
    for text in ["failed", "cancelled", "something_new"] {
        let h = harness();
        h.provider.queue_add_status(text);
        assert_eq!(h.wait().await, IndexingOutcome::Failed, "{text}");
        assert!(h.provider.reads().is_empty(), "{text}");
    }
}

#[tokio::test]
async fn an_add_response_without_status_counts_as_in_progress() {
    // inst-iw-initial-status
    let h = harness();
    h.provider.queue_add(Ok(None));
    h.provider.queue_read_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    assert_eq!(h.provider.reads().len(), 1);
}

// ── schedule ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn completion_after_three_reads_waits_250_500_and_1000_ms() {
    // inst-iw-initial-wait, inst-iw-backoff
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.queue_read_status("in_progress");
    h.provider.queue_read_status("in_progress");
    h.provider.queue_read_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    assert_eq!(h.time.sleeps(), vec![MS(250), MS(500), MS(1000)]);
    assert_eq!(h.read_times(), vec![MS(250), MS(750), MS(1750)]);
}

#[tokio::test]
async fn the_wait_doubles_up_to_two_seconds_and_stays_there() {
    let h = harness();
    h.provider.queue_add_status("in_progress");
    assert_eq!(h.wait().await, IndexingOutcome::StillInProgress);
    let sleeps = h.time.sleeps();
    assert_eq!(
        &sleeps[..6],
        &[MS(250), MS(500), MS(1000), MS(2000), MS(2000), MS(2000)]
    );
    assert!(sleeps.iter().all(|wait| *wait <= MS(2000)), "{sleeps:?}");
}

#[tokio::test]
async fn a_status_that_stays_in_progress_ends_at_the_deadline_with_still_in_progress() {
    // inst-iw-deadline, inst-iw-deadline-reached: 25 s after the request start
    let h = harness();
    h.provider.queue_add_status("in_progress");
    assert_eq!(h.wait().await, IndexingOutcome::StillInProgress);
    let times = h.read_times();
    assert_eq!(times[0], MS(250));
    assert_eq!(times[1], MS(750));
    assert_eq!(times[2], MS(1750));
    assert_eq!(times[3], MS(3750));
    assert_eq!(*times.last().unwrap(), MS(23_750));
    assert!(
        times.iter().all(|at| *at < Duration::from_secs(25)),
        "{times:?}"
    );
    // the wait that would reach the deadline is not taken
    assert_eq!(h.time.elapsed(), MS(23_750));
}

#[tokio::test]
async fn the_deadline_counts_from_the_request_start_not_from_the_call() {
    // the provider upload already used 10 s of the 25 s
    let h = harness();
    h.time.sleep(Duration::from_secs(10)).await;
    let started_at = Duration::ZERO;
    h.provider.queue_add_status("in_progress");
    assert_eq!(
        h.wait_from(started_at).await,
        IndexingOutcome::StillInProgress
    );
    let times = h.read_times();
    assert!(!times.is_empty());
    assert!(
        times.iter().all(|at| *at < Duration::from_secs(25)),
        "{times:?}"
    );
    assert!(h.time.elapsed() < Duration::from_secs(25));
    // the first read is 250 ms after the call, not after the request start
    assert_eq!(times[0], Duration::from_secs(10) + MS(250));
    // 15 s left: fewer polls than the 14 of a fresh 25 s window
    assert!(times.len() < 14, "{times:?}");
}

#[tokio::test]
async fn a_deadline_that_already_passed_returns_still_in_progress_without_a_read() {
    let h = harness();
    h.time.sleep(Duration::from_secs(26)).await;
    h.provider.queue_add_status("in_progress");
    assert_eq!(
        h.wait_from(Duration::ZERO).await,
        IndexingOutcome::StillInProgress
    );
    assert!(h.provider.reads().is_empty());
    assert_eq!(h.time.sleeps().len(), 1, "only the setup sleep");
}

#[tokio::test]
async fn a_wait_that_ends_exactly_at_the_deadline_is_not_taken() {
    // inst-iw-deadline-reached: "reaches the deadline" includes equality
    let h = harness();
    h.time.sleep(MS(24_750)).await;
    h.provider.queue_add_status("in_progress");
    assert_eq!(
        h.wait_from(Duration::ZERO).await,
        IndexingOutcome::StillInProgress
    );
    assert!(h.provider.reads().is_empty(), "a read after the deadline");
    assert_eq!(h.time.sleeps().len(), 1, "only the setup sleep");
}

#[tokio::test]
async fn a_wait_that_ends_one_millisecond_before_the_deadline_is_taken() {
    let h = harness();
    h.time.sleep(MS(24_749)).await;
    h.provider.queue_add_status("in_progress");
    assert_eq!(
        h.wait_from(Duration::ZERO).await,
        IndexingOutcome::StillInProgress
    );
    assert_eq!(h.read_times(), vec![MS(24_999)]);
}

#[tokio::test]
async fn every_read_timeout_ends_at_the_deadline() {
    // inst-iw-read: "with a timeout that ends at the deadline"
    let h = harness();
    h.provider.queue_add_status("in_progress");
    assert_eq!(h.wait().await, IndexingOutcome::StillInProgress);
    for (at, timeout) in h.provider.reads() {
        assert_eq!(at + timeout, Duration::from_secs(25), "read at {at:?}");
    }
}

// ── read outcomes ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_completed_read_is_ready() {
    // inst-iw-completed
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.queue_read_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    assert_eq!(h.provider.reads().len(), 1);
}

#[tokio::test]
async fn failed_cancelled_and_unknown_read_statuses_fail_the_indexing() {
    // inst-iw-return-failed
    for text in ["failed", "cancelled", "expired"] {
        let h = harness();
        h.provider.queue_add_status("in_progress");
        h.provider.queue_read_status(text);
        assert_eq!(h.wait().await, IndexingOutcome::Failed, "{text}");
        assert_eq!(h.provider.reads().len(), 1, "{text}");
    }
}

#[tokio::test]
async fn a_read_without_status_keeps_polling() {
    // inst-iw-status: a missing status counts as in_progress
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.queue_read(Ok(None));
    h.provider.queue_read(Ok(None));
    h.provider.queue_read_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    assert_eq!(h.provider.reads().len(), 3);
}

#[tokio::test]
async fn a_transient_read_error_keeps_polling() {
    // inst-iw-transient: provider 5xx, gateway failure or timeout
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.queue_read(Err(ProviderFailure::Transient));
    h.provider.queue_read(Err(ProviderFailure::Transient));
    h.provider.queue_read_status("completed");
    assert_eq!(h.wait().await, IndexingOutcome::Ready);
    assert_eq!(h.provider.reads().len(), 3);
}

#[tokio::test]
async fn transient_errors_until_the_deadline_are_still_in_progress_not_a_failure() {
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.set_default_read(Err(ProviderFailure::Transient));
    assert_eq!(h.wait().await, IndexingOutcome::StillInProgress);
    assert!(h.provider.reads().len() > 5);
}

#[tokio::test]
async fn a_non_transient_read_error_fails_the_indexing_at_once() {
    // inst-iw-read-failed: for example a 4xx
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.queue_read(Err(ProviderFailure::Other));
    assert_eq!(h.wait().await, IndexingOutcome::Failed);
    assert_eq!(h.provider.reads().len(), 1);
}

#[tokio::test]
async fn a_transient_error_then_a_failed_status_is_a_failure() {
    let h = harness();
    h.provider.queue_add_status("in_progress");
    h.provider.queue_read(Err(ProviderFailure::Transient));
    h.provider.queue_read_status("failed");
    assert_eq!(h.wait().await, IndexingOutcome::Failed);
}
