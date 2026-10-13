#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! A gear whose runtime is not bound yet neither spends the delivery nor
//! decides anything: the chat-state checks (`inst-if-chat-missing`,
//! `inst-if-chat-active`, `inst-load-deleted-chat`) run first and reject; the
//! rest is a plain uncounted `Retry`.

use toolkit_db::outbox::MessageResult;
use uuid::Uuid;

use crate::worker_test_env::{WorkerEnv, WorkerOptions};

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn rejected(result: &MessageResult) -> Option<&str> {
    match result {
        MessageResult::Reject(reason) => Some(reason),
        _ => None,
    }
}

/// A deleted chat with one owed document and a bound vector store.
async fn deleted_chat(env: &WorkerEnv) -> Uuid {
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    env.owed(chat, 1).await;
    env.store_row(chat, Some("vs_chat"), "openai").await;
    env.soft_delete_chat(chat).await;
    chat
}

#[tokio::test]
async fn an_unbound_runtime_retries_without_a_provider_call_a_write_or_a_metric() {
    let env = WorkerEnv::with(WorkerOptions {
        unbound_runtime: true,
        ..WorkerOptions::default()
    })
    .await;
    env.script_happy();
    let chat = deleted_chat(&env).await;
    let id = env.att_ids_of(chat).await[0];

    // even a delivery at the attempt limit is a plain retry: it is not a store failure
    for attempts in [0, 4, 9] {
        let result = env.deliver_cleanup(chat, attempts).await;
        assert!(is_retry(&result), "attempts {attempts}: {result:?}");
    }

    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.att(id).await.cleanup_attempts, 0);
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("pending"));
    assert!(env.metrics.events().is_empty());
    assert_eq!(env.stores().await.len(), 1);
}

#[tokio::test]
async fn an_unbound_runtime_still_rejects_a_message_of_an_unknown_or_active_chat() {
    let env = WorkerEnv::with(WorkerOptions {
        unbound_runtime: true,
        ..WorkerOptions::default()
    })
    .await;
    let active = env.m.stream.seed_chat(51, "m-oa").await;

    let unknown = env.deliver_cleanup(Uuid::from_u128(0xDEAD), 0).await;
    let not_deleted = env.deliver_cleanup(active, 0).await;

    assert_eq!(rejected(&unknown), Some("chat not found"), "{unknown:?}");
    assert_eq!(
        rejected(&not_deleted),
        Some("chat is not soft-deleted"),
        "{not_deleted:?}"
    );
}
