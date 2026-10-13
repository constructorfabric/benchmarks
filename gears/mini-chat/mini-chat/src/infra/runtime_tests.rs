#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the late-bound runtime handles of the gear
//! (`cpt-cf-mini-chat-flow-gear-foundation-gear-start`, `inst-spawn-workers`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::GearRuntime;
use crate::domain::service::attachments::ports::BackgroundRuntime;
use crate::infra::background_tasks::BackgroundTasks;
use crate::test_doubles::{S2S_SUBJECT, s2s_ctx};

fn flag_task(flag: &Arc<AtomicBool>) -> futures::future::BoxFuture<'static, ()> {
    let flag = Arc::clone(flag);
    Box::pin(async move {
        flag.store(true, Ordering::SeqCst);
    })
}

#[tokio::test]
async fn before_start_nothing_spawns_and_there_is_no_s2s_context() {
    let runtime = GearRuntime::new();
    let ran = Arc::new(AtomicBool::new(false));

    assert!(!runtime.spawn("task", flag_task(&ran)));
    tokio::task::yield_now().await;

    assert!(!ran.load(Ordering::SeqCst));
    assert!(runtime.s2s_context().is_none());
    let tasks = Arc::new(BackgroundTasks::new(&CancellationToken::new()));
    assert!(
        runtime.bind(&tasks, s2s_ctx()),
        "the same runtime works once bound"
    );
    assert!(runtime.spawn("task", flag_task(&ran)));
}

#[tokio::test]
async fn after_the_bind_tasks_run_on_the_registry_and_the_s2s_context_is_served() {
    let parent = CancellationToken::new();
    let tasks = Arc::new(BackgroundTasks::new(&parent));
    let runtime = GearRuntime::new();
    assert!(runtime.bind(&tasks, s2s_ctx()));
    let ran = Arc::new(AtomicBool::new(false));

    assert!(runtime.spawn("indexing", flag_task(&ran)));
    for _ in 0..100 {
        if ran.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(ran.load(Ordering::SeqCst));
    assert_eq!(runtime.s2s_context().unwrap().subject_id(), S2S_SUBJECT);
}

#[tokio::test]
async fn a_task_spawned_through_the_runtime_is_cancelled_with_the_registry() {
    let parent = CancellationToken::new();
    let tasks = Arc::new(BackgroundTasks::new(&parent));
    let runtime = GearRuntime::new();
    assert!(runtime.bind(&tasks, s2s_ctx()));
    let stop = runtime.stop_token();
    let ended = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ended);
    assert!(runtime.spawn(
        "waiter",
        Box::pin(async move {
            stop.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        }),
    ));

    let stuck = tasks.cancel_and_join(Duration::from_secs(2)).await;

    assert!(stuck.is_empty());
    assert!(ended.load(Ordering::SeqCst));
    assert!(tasks.running().is_empty());
}

#[tokio::test]
async fn the_runtime_binds_once() {
    let parent = CancellationToken::new();
    let tasks = Arc::new(BackgroundTasks::new(&parent));
    let runtime = GearRuntime::new();

    assert!(runtime.bind(&tasks, s2s_ctx()));
    assert!(!runtime.bind(&tasks, s2s_ctx()), "a second bind is refused");
}

#[tokio::test]
async fn the_stop_token_is_cancelled_when_the_gear_stops() {
    let parent = CancellationToken::new();
    let tasks = Arc::new(BackgroundTasks::new(&parent));
    let runtime = GearRuntime::new();
    assert!(runtime.bind(&tasks, s2s_ctx()));
    let stop = runtime.stop_token();
    assert!(!stop.is_cancelled());

    parent.cancel();

    assert!(
        stop.is_cancelled(),
        "the token is the registry's, a child of the gear's"
    );
}
