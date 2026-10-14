//! The background indexing task: the heartbeat that opens each 20 s round, the 250 ms to 5 s poll
//! schedule, the status mapping, the 10-minute limit, the guarded `ready` write with 4 attempts
//! (1, 2 and 4 s apart), the one-transaction failure, and the silent stop on deletion, chat
//! deletion or gear stop. It never deletes the provider file inline. The waits run on the
//! injected virtual timer.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing`;
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-indexing-lifecycle`;
//! state `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status`; metric
//! `mini_chat_attachment_background_indexing` of
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{BackgroundDeps, BackgroundTask, run_background_indexing};
use crate::domain::attachments::provider::ProviderFailure;
use crate::domain::error::DomainError;
use crate::test_support::attach_fakes::{
    ProviderCall, RecordingAttachmentMetrics, ScriptedProvider, ScriptedStore, StoreCall,
    UnusedCleanupQueue, openai_target,
};
use crate::test_support::manual_time::ManualTime;
use crate::test_support::provider::Gate;
use crate::test_support::quota::default_now;

const S: fn(u64) -> Duration = Duration::from_secs;
const MS: fn(u64) -> Duration = Duration::from_millis;

struct Harness {
    time: Arc<ManualTime>,
    provider: Arc<ScriptedProvider>,
    store: Arc<ScriptedStore>,
    metrics: Arc<RecordingAttachmentMetrics>,
    deps: BackgroundDeps,
    task: BackgroundTask,
    cancel: CancellationToken,
}

fn harness() -> Harness {
    let time = ManualTime::auto(default_now());
    let provider = ScriptedProvider::new(&time);
    let store = ScriptedStore::new(&time);
    let metrics = RecordingAttachmentMetrics::new();
    let deps = BackgroundDeps {
        provider: provider.clone(),
        store: store.clone(),
        cleanup: Arc::new(UnusedCleanupQueue),
        clock: time.clone(),
        timer: time.clone(),
        metrics: metrics.clone(),
    };
    let task = BackgroundTask {
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        attachment_id: Uuid::new_v4(),
        vector_store_id: "vs_1".to_owned(),
        provider_file_id: "file-9".to_owned(),
        target: openai_target(),
    };
    Harness {
        time,
        provider,
        store,
        metrics,
        deps,
        task,
        cancel: CancellationToken::new(),
    }
}

impl Harness {
    /// Run the task to its end (20 s of real time at most).
    async fn run(&self) {
        tokio::time::timeout(
            Duration::from_secs(20),
            run_background_indexing(&self.deps, &self.task, &self.cancel),
        )
        .await
        .expect("the background task never ended");
    }

    fn read_times(&self) -> Vec<Duration> {
        self.provider
            .reads()
            .into_iter()
            .map(|(at, _)| at)
            .collect()
    }
}

fn db_error() -> DomainError {
    DomainError::internal("database down")
}

// ── ready ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_completed_read_sets_ready_once_and_records_ready() {
    // inst-bg-outcome, inst-bg-case-ready
    let h = harness();
    h.provider.queue_read_status("in_progress");
    h.provider.queue_read_status("in_progress");
    h.provider.queue_read_status("completed");
    h.run().await;
    assert_eq!(h.store.ready_times().len(), 1);
    assert_eq!(h.store.fail_count(), 0);
    assert_eq!(h.metrics.background(), vec!["ready".to_owned()]);
}

#[tokio::test]
async fn the_first_heartbeat_comes_before_the_first_read_which_waits_250_ms() {
    // inst-bg-heartbeat, inst-bg-round
    let h = harness();
    h.provider.queue_read_status("completed");
    h.run().await;
    assert_eq!(h.store.heartbeat_times(), vec![Duration::ZERO]);
    assert_eq!(h.read_times(), vec![MS(250)]);
}

#[tokio::test]
async fn the_reads_target_the_vector_store_file_of_the_task() {
    let h = harness();
    h.provider.queue_read_status("completed");
    h.run().await;
    let calls = h.provider.calls();
    assert!(matches!(
        &calls[0].1,
        ProviderCall::Read { vector_store_id, file_id, .. }
            if vector_store_id == "vs_1" && file_id == "file-9"
    ));
}

#[tokio::test]
async fn the_poll_wait_doubles_from_250_ms_and_never_exceeds_5_s() {
    // inst-bg-round: 250 ms before the first read, doubling up to 5 s
    let h = harness();
    // never completes: stops at the 10-minute limit
    h.run().await;
    let times = h.read_times();
    assert_eq!(
        &times[..5],
        &[MS(250), MS(750), MS(1750), MS(3750), MS(7750)]
    );
    let mut previous = Duration::ZERO;
    for at in &times {
        let gap = at.checked_sub(previous).unwrap();
        assert!(gap <= S(5), "gap {gap:?} before the read at {at:?}");
        previous = *at;
    }
}

#[tokio::test]
async fn each_round_starts_with_a_heartbeat_about_20_seconds_apart() {
    // inst-bg-heartbeat: every 20 s, at most half the minimum stale threshold (60 s)
    let h = harness();
    h.run().await;
    let beats = h.store.heartbeat_times();
    assert!(beats.len() >= 20, "{} heartbeats", beats.len());
    assert_eq!(beats[0], Duration::ZERO);
    for pair in beats.windows(2) {
        let gap = pair[1].checked_sub(pair[0]).unwrap();
        assert!(gap >= S(15) && gap <= S(30), "heartbeat gap {gap:?}");
    }
}

#[tokio::test]
async fn the_heartbeat_writes_the_application_clock_so_updated_at_advances() {
    let h = harness();
    h.run().await;
    let stamps: Vec<_> = h
        .store
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            StoreCall::Heartbeat { now, .. } => Some(now),
            _ => None,
        })
        .collect();
    assert!(stamps.len() >= 2);
    assert!(stamps[1] - stamps[0] >= time::Duration::seconds(15));
}

// ── failure and timeout ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_failed_status_runs_the_failure_transaction_once_and_records_failed() {
    // inst-bg-case-failed
    let h = harness();
    h.provider.queue_read_status("in_progress");
    h.provider.queue_read_status("failed");
    h.run().await;
    assert_eq!(h.store.fail_count(), 1);
    assert!(h.store.ready_times().is_empty());
    assert_eq!(h.metrics.background(), vec!["failed".to_owned()]);
}

#[tokio::test]
async fn the_failure_transaction_carries_the_chat_of_the_task() {
    // the cleanup payload needs tenant, chat and attachment
    let h = harness();
    h.provider.queue_read_status("failed");
    h.run().await;
    let chat_ids: Vec<Uuid> = h
        .store
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            StoreCall::FailIndexing { chat_id, .. } => Some(chat_id),
            _ => None,
        })
        .collect();
    assert_eq!(chat_ids, vec![h.task.chat_id]);
}

#[tokio::test]
async fn cancelled_and_unknown_statuses_fail_the_task() {
    for text in ["cancelled", "expired", "weird"] {
        let h = harness();
        h.provider.queue_read_status(text);
        h.run().await;
        assert_eq!(h.store.fail_count(), 1, "{text}");
        assert_eq!(h.metrics.background(), vec!["failed".to_owned()], "{text}");
    }
}

#[tokio::test]
async fn a_non_transient_read_error_fails_the_task_at_once() {
    let h = harness();
    h.provider.queue_read(Err(ProviderFailure::Other));
    h.run().await;
    assert_eq!(h.store.fail_count(), 1);
    assert_eq!(h.provider.reads().len(), 1);
    assert_eq!(h.metrics.background(), vec!["failed".to_owned()]);
}

#[tokio::test]
async fn a_missing_status_and_transient_errors_keep_polling() {
    // inst-bg-round: a missing status and a transient read error keep polling
    let h = harness();
    h.provider.queue_read(Ok(None));
    h.provider.queue_read(Err(ProviderFailure::Transient));
    h.provider.queue_read(Err(ProviderFailure::Transient));
    h.provider.queue_read(Ok(None));
    h.provider.queue_read_status("completed");
    h.run().await;
    assert_eq!(h.provider.reads().len(), 5);
    assert_eq!(h.metrics.background(), vec!["ready".to_owned()]);
}

#[tokio::test]
async fn ten_minutes_without_an_outcome_is_a_timeout() {
    // inst-bg-timeout: the failure transaction with result `timeout`
    let h = harness();
    h.run().await;
    assert_eq!(h.store.fail_count(), 1);
    assert!(h.store.ready_times().is_empty());
    assert_eq!(h.metrics.background(), vec!["timeout".to_owned()]);
    let failed_at = h
        .store
        .calls()
        .into_iter()
        .find_map(|call| match call {
            StoreCall::FailIndexing { at, .. } => Some(at),
            _ => None,
        })
        .unwrap();
    assert!(failed_at >= S(600), "{failed_at:?}");
    assert!(failed_at <= S(610), "{failed_at:?}");
}

#[tokio::test]
async fn transient_errors_for_ten_minutes_are_a_timeout_not_a_failure() {
    let h = harness();
    h.provider.set_default_read(Err(ProviderFailure::Transient));
    h.run().await;
    assert_eq!(h.metrics.background(), vec!["timeout".to_owned()]);
}

#[tokio::test]
async fn the_task_never_deletes_the_provider_file_inline() {
    // inst-bg-no-inline-delete: the cleanup handler deletes it with retries
    for failing in [true, false] {
        let h = harness();
        if failing {
            h.provider.queue_read_status("failed");
        } else {
            h.provider.queue_read_status("completed");
        }
        h.run().await;
        assert_eq!(
            h.provider
                .count_where(|call| matches!(call, ProviderCall::DeleteFile(_))),
            0
        );
    }
}

// ── the guarded ready write ──────────────────────────────────────────────────

#[tokio::test]
async fn a_database_error_on_the_ready_write_is_retried_1_2_and_4_seconds_apart() {
    // inst-bg-case-ready: 4 attempts in total
    let h = harness();
    h.provider.queue_read_status("completed");
    for _ in 0..3 {
        h.store.queue_ready(Err(db_error()));
    }
    h.run().await;
    let attempts = h.store.ready_times();
    assert_eq!(attempts.len(), 4, "{attempts:?}");
    assert_eq!(attempts[1].checked_sub(attempts[0]).unwrap(), S(1));
    assert_eq!(attempts[2].checked_sub(attempts[1]).unwrap(), S(2));
    assert_eq!(attempts[3].checked_sub(attempts[2]).unwrap(), S(4));
    assert_eq!(h.metrics.background(), vec!["ready".to_owned()]);
    assert_eq!(h.store.fail_count(), 0);
}

#[tokio::test]
async fn a_ready_write_that_succeeds_on_the_second_attempt_stops_retrying() {
    let h = harness();
    h.provider.queue_read_status("completed");
    h.store.queue_ready(Err(db_error()));
    h.run().await;
    assert_eq!(h.store.ready_times().len(), 2);
    assert_eq!(h.metrics.background(), vec!["ready".to_owned()]);
}

#[tokio::test]
async fn four_failed_ready_writes_leave_the_row_uploaded_and_record_set_ready_failed() {
    let h = harness();
    h.provider.queue_read_status("completed");
    for _ in 0..6 {
        h.store.queue_ready(Err(db_error()));
    }
    h.run().await;
    assert_eq!(h.store.ready_times().len(), 4);
    assert_eq!(h.metrics.background(), vec!["set_ready_failed".to_owned()]);
    // the row stays `uploaded` for the reaper: no failure transaction
    assert_eq!(h.store.fail_count(), 0);
}

#[tokio::test]
async fn a_lost_ready_guard_stops_without_retry_and_without_a_failure() {
    // inst-bg-guard-lost: the guarded update affected no row
    let h = harness();
    h.provider.queue_read_status("completed");
    h.store.queue_ready(Ok(false));
    h.run().await;
    assert_eq!(h.store.ready_times().len(), 1);
    assert_eq!(h.store.fail_count(), 0);
}

#[tokio::test]
async fn a_lost_failure_guard_stops_without_further_action() {
    let h = harness();
    h.provider.queue_read_status("failed");
    h.store.queue_fail(Ok(false));
    h.run().await;
    assert_eq!(h.store.fail_count(), 1);
    assert!(h.store.ready_times().is_empty());
}

#[tokio::test]
async fn a_failing_failure_transaction_leaves_the_row_for_the_reaper() {
    // inst-bg-tx-failed: no retry loop, no ready write
    let h = harness();
    h.provider.queue_read_status("failed");
    h.store.queue_fail(Err(db_error()));
    h.run().await;
    assert_eq!(h.store.fail_count(), 1);
    assert!(h.store.ready_times().is_empty());
}

// ── silent stops ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_heartbeat_that_affects_no_row_stops_the_task_without_changes() {
    // inst-bg-row-gone: deleted attachment, deleted chat, or no longer `uploaded`
    let h = harness();
    h.store.queue_heartbeat(Ok(false));
    h.run().await;
    assert!(h.provider.reads().is_empty());
    assert!(h.store.ready_times().is_empty());
    assert_eq!(h.store.fail_count(), 0);
    assert!(h.metrics.background().is_empty());
}

#[tokio::test]
async fn a_row_deleted_between_rounds_stops_the_task_without_changes() {
    let h = harness();
    h.store.queue_heartbeat(Ok(true));
    h.store.queue_heartbeat(Ok(false));
    h.run().await;
    assert_eq!(h.store.heartbeat_times().len(), 2);
    assert!(h.store.ready_times().is_empty());
    assert_eq!(h.store.fail_count(), 0);
    assert!(h.metrics.background().is_empty());
    assert!(h.time.elapsed() < S(60), "{:?}", h.time.elapsed());
}

#[tokio::test]
async fn a_cancelled_gear_stops_the_task_before_any_work() {
    // inst-bg-gear-stop: the row stays uploaded for the reaper
    let h = harness();
    h.cancel.cancel();
    h.run().await;
    assert!(h.store.calls().is_empty());
    assert!(h.provider.calls().is_empty());
    assert!(h.metrics.background().is_empty());
}

#[tokio::test]
async fn a_gear_stop_during_a_provider_read_ends_the_task_without_changes() {
    let h = harness();
    let gate = Gate::new();
    h.provider.gate_reads(&gate);
    let deps = BackgroundDeps {
        provider: h.provider.clone(),
        store: h.store.clone(),
        cleanup: Arc::new(UnusedCleanupQueue),
        clock: h.time.clone(),
        timer: h.time.clone(),
        metrics: h.metrics.clone(),
    };
    let task = h.task.clone();
    let cancel = h.cancel.clone();
    let handle = tokio::spawn(async move {
        run_background_indexing(&deps, &task, &cancel).await;
    });
    // wait for the first read to be parked on the gate
    for _ in 0..500 {
        if !h.provider.reads().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(!h.provider.reads().is_empty(), "the read never arrived");
    h.cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the task ignored the gear stop")
        .unwrap();
    assert!(h.store.ready_times().is_empty());
    assert_eq!(h.store.fail_count(), 0);
    assert!(h.metrics.background().is_empty());
}
