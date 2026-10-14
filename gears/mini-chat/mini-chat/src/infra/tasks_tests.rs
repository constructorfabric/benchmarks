//! Background task set tests: spawn hook, cancellation through the parent token, bounded
//! join. Paused tokio time drives the timeout.
//!
//! Slice S2: algorithm `gear-start` (spawn hook of the workers and the reconcile task) and
//! algorithm `gear-stop` (`inst-stop-cancel-workers`, `inst-stop-join-workers`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::BackgroundTasks;

/// A task that runs until its token is cancelled and then raises `finished`.
fn watcher(tasks: &BackgroundTasks, name: &str, finished: &Arc<AtomicBool>) {
    let finished = Arc::clone(finished);
    tasks.spawn(name, move |cancel| async move {
        cancel.cancelled().await;
        finished.store(true, Ordering::SeqCst);
    });
}

#[tokio::test]
async fn a_new_set_has_no_live_task() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    assert_eq!(tasks.live_count(), 0);
}

#[tokio::test]
async fn a_spawned_task_is_live_until_it_finishes() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let finished = Arc::new(AtomicBool::new(false));
    watcher(&tasks, "worker", &finished);
    tokio::task::yield_now().await;
    assert_eq!(tasks.live_count(), 1);
    assert!(!finished.load(Ordering::SeqCst));

    tasks.cancel();
    for _ in 0..100 {
        if tasks.live_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(tasks.live_count(), 0);
    assert!(finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancelling_the_parent_token_cancels_every_task() {
    let parent = CancellationToken::new();
    let tasks = BackgroundTasks::new(&parent);
    let (first, second) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    watcher(&tasks, "first", &first);
    watcher(&tasks, "second", &second);

    parent.cancel();
    let report = tasks.shutdown(Duration::from_secs(5)).await;

    assert!(first.load(Ordering::SeqCst) && second.load(Ordering::SeqCst));
    assert!(report.timed_out.is_empty(), "{report:?}");
}

#[tokio::test]
async fn shutdown_cancels_and_joins_every_task() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let (first, second) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    watcher(&tasks, "reconcile", &first);
    watcher(&tasks, "orphan-watchdog", &second);

    let report = tasks.shutdown(Duration::from_secs(5)).await;

    assert!(first.load(Ordering::SeqCst) && second.load(Ordering::SeqCst));
    let mut joined = report.joined.clone();
    joined.sort();
    assert_eq!(joined, vec!["orphan-watchdog", "reconcile"]);
    assert!(report.timed_out.is_empty());
    assert_eq!(tasks.live_count(), 0);
}

#[tokio::test]
async fn shutting_down_an_empty_set_is_a_no_op() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let report = tasks.shutdown(Duration::from_secs(5)).await;
    assert!(report.joined.is_empty() && report.timed_out.is_empty());
}

#[tokio::test]
async fn shutdown_is_idempotent() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    watcher(&tasks, "worker", &Arc::new(AtomicBool::new(false)));
    tasks.shutdown(Duration::from_secs(5)).await;
    let again = tasks.shutdown(Duration::from_secs(5)).await;
    assert!(again.joined.is_empty() && again.timed_out.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_task_that_ignores_cancellation_is_reported_after_the_timeout() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    tasks.spawn("stuck", |_cancel| async move {
        std::future::pending::<()>().await;
    });
    let finished = Arc::new(AtomicBool::new(false));
    watcher(&tasks, "well-behaved", &finished);
    tokio::task::yield_now().await;

    let started = Instant::now();
    let report = tasks.shutdown(Duration::from_secs(5)).await;

    assert_eq!(report.timed_out, vec!["stuck"]);
    assert_eq!(report.joined, vec!["well-behaved"]);
    assert!(finished.load(Ordering::SeqCst));
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(5),
        "one bound for the whole set"
    );
}

#[tokio::test]
async fn a_task_that_finishes_on_its_own_is_joined_without_waiting() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    tasks.spawn("short", |_cancel| async move {});
    let report = tasks.shutdown(Duration::from_secs(5)).await;
    assert_eq!(report.joined, vec!["short"]);
}
