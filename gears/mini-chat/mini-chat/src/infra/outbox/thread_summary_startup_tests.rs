#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! B7B-001: the startup window of the gear, in which the outbox pipeline can
//! deliver before the runtime (S2S context) is bound. The summary handler then
//! answers a plain `Retry`: no metric, no dead letter (`inst-retry-budget` is
//! for failed deliveries), and the chat-state checks still decide first.

use toolkit_db::outbox::MessageResult;
use uuid::Uuid;

use crate::worker_test_env::{WorkerEnv, WorkerOptions, boundary};

async fn unbound() -> WorkerEnv {
    WorkerEnv::with(WorkerOptions {
        unbound_runtime: true,
        ..WorkerOptions::default()
    })
    .await
}

#[tokio::test]
async fn an_unbound_runtime_neither_counts_nor_dead_letters_a_summary_item() {
    let env = unbound().await; // thread_summary_worker.max_attempts 3
    let turns = env.conversation(3).await;
    let task = WorkerEnv::task(env.chat, None, boundary(turns[2].assistant_message, 31));

    // deliveries at and beyond the limit: a failed delivery would be a dead letter
    for attempts in [0, 1, 2, 7] {
        let result = env.deliver_summary(&task, attempts).await;
        assert!(
            matches!(result, MessageResult::Retry),
            "attempts {attempts}: {result:?}"
        );
    }

    assert!(env.metrics.events().is_empty(), "no metric is recorded");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert!(env.summary(env.chat).await.is_none());
}

#[tokio::test]
async fn an_unbound_runtime_still_ends_a_work_item_of_a_deleted_chat_ok() {
    let env = unbound().await;
    let turns = env.conversation(3).await;
    let task = WorkerEnv::task(env.chat, None, boundary(turns[2].assistant_message, 31));
    env.soft_delete_chat(env.chat).await;
    let mut gone = task;
    gone.chat_id = Uuid::from_u128(0xDEAD);

    let deleted = env.deliver_summary(&task, 0).await;
    let missing = env.deliver_summary(&gone, 0).await;

    assert!(matches!(deleted, MessageResult::Ok), "{deleted:?}");
    assert!(matches!(missing, MessageResult::Ok), "{missing:?}");
}
