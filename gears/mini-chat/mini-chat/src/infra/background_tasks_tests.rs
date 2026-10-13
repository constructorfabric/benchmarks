#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the bounded cancel-and-join of
//! `cpt-cf-mini-chat-flow-gear-foundation-gear-stop` (virtual time).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Level;

use super::{BackgroundTasks, TASK_JOIN_TIMEOUT};
use crate::test_doubles::LogCapture;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn the_join_bound_is_shorter_than_the_framework_stop_deadline() {
    // The toolkit stop timeout defaults to 30 s and the runtime shutdown deadline to 35 s.
    assert!(TASK_JOIN_TIMEOUT < secs(30), "{TASK_JOIN_TIMEOUT:?}");
    assert!(TASK_JOIN_TIMEOUT >= secs(1));
}

#[tokio::test(start_paused = true)]
async fn spawned_tasks_are_listed_while_they_run() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let token = tasks.token();
    tasks.spawn("alpha", async move { token.cancelled().await });
    let token = tasks.token();
    tasks.spawn("beta", async move { token.cancelled().await });
    tokio::task::yield_now().await;

    let mut running = tasks.running();
    running.sort_unstable();

    assert_eq!(running, vec!["alpha", "beta"]);
}

#[tokio::test(start_paused = true)]
async fn finished_tasks_are_not_listed() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    tasks.spawn("quick", async {});
    let token = tasks.token();
    tasks.spawn("long", async move { token.cancelled().await });
    tokio::time::sleep(secs(1)).await;

    assert_eq!(tasks.running(), vec!["long"]);
}

#[tokio::test(start_paused = true)]
async fn cancel_and_join_cancels_the_token_and_waits_for_the_tasks() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let finished = Arc::new(AtomicBool::new(false));
    let token = tasks.token();
    let flag = Arc::clone(&finished);
    tasks.spawn("worker", async move {
        token.cancelled().await;
        tokio::time::sleep(secs(2)).await;
        flag.store(true, Ordering::SeqCst);
    });
    tokio::task::yield_now().await;

    let still_running = tasks.cancel_and_join(secs(10)).await;

    assert!(still_running.is_empty(), "{still_running:?}");
    assert!(
        finished.load(Ordering::SeqCst),
        "the join waited for the clean-up"
    );
    assert!(tasks.running().is_empty());
    assert!(tasks.token().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn the_parent_token_cancels_the_tasks() {
    let parent = CancellationToken::new();
    let tasks = BackgroundTasks::new(&parent);
    let token = tasks.token();
    tasks.spawn("worker", async move { token.cancelled().await });

    parent.cancel();
    tokio::time::sleep(secs(1)).await;

    assert!(tasks.running().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_task_that_ignores_cancellation_is_reported_after_the_bound_with_a_warning() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    tasks.spawn("stubborn", std::future::pending());
    let token = tasks.token();
    tasks.spawn("polite", async move { token.cancelled().await });
    tokio::task::yield_now().await;
    let logs = LogCapture::start();
    let started = Instant::now();

    let still_running = tasks.cancel_and_join(secs(10)).await;

    assert_eq!(still_running, vec!["stubborn"]);
    assert_eq!(
        started.elapsed(),
        secs(10),
        "the stop gives up at the bound"
    );
    let warnings = logs.at_containing(Level::WARN, "stubborn");
    assert_eq!(warnings.len(), 1, "{:?}", logs.records());
    assert!(!logs.all_text().contains("polite"), "{}", logs.all_text());
}

#[tokio::test(start_paused = true)]
async fn the_join_returns_as_soon_as_every_task_ended() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let token = tasks.token();
    tasks.spawn("worker", async move {
        token.cancelled().await;
        tokio::time::sleep(secs(3)).await;
    });
    tokio::task::yield_now().await;
    let started = Instant::now();

    let still_running = tasks.cancel_and_join(secs(10)).await;

    assert!(still_running.is_empty());
    assert_eq!(started.elapsed(), secs(3));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_task_does_not_break_the_join() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    tasks.spawn("crashing", async { panic!("task bug") });
    let token = tasks.token();
    tasks.spawn("polite", async move { token.cancelled().await });
    tokio::task::yield_now().await;

    let still_running = tasks.cancel_and_join(secs(10)).await;

    assert!(still_running.is_empty(), "{still_running:?}");
}

#[tokio::test(start_paused = true)]
async fn joining_without_tasks_returns_immediately() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let started = Instant::now();

    let still_running = tasks.cancel_and_join(secs(10)).await;

    assert!(still_running.is_empty());
    assert_eq!(started.elapsed(), Duration::ZERO);
}
