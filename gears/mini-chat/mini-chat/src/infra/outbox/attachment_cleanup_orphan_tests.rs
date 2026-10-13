#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the row-less `attachment_orphan_file` variant of the cleanup handler
//! (`inst-orphan-variant` of `cpt-cf-mini-chat-algo-attachments-handle-cleanup`):
//! the file of the message is deleted whatever the state of the row, the chat
//! and the cleanup columns, and retries are bounded by the delivery attempt
//! count.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use uuid::Uuid;

use super::{AttachmentCleanupHandler, CleanupHandlerDeps};
use crate::attachment_test_env::{ALIAS, ANTHROPIC_ALIAS, AttEnv};
use crate::chat_test_env::ts;
use crate::domain::service::attachments::payload::{CleanupEvent, SecondaryRef};
use crate::domain::service::attachments::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundRuntime, StorageClient, StorageDirectory,
};
use crate::domain::service::attachments::test_support::{
    FakeRepo, Metric, RecordingAttachmentMetrics, TestRuntime,
};
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::storage::oagw_storage::OagwStorageClient;
use crate::test_doubles::{
    FakeOagw, Fault, ProxyReply, S2S_SUBJECT, USER_TENANT, provider_json, registry_ordered,
};

fn orphan(env: &AttEnv, chat: Uuid, id: Uuid, file: &str) -> Value {
    AttEnv::payload_json(&env.cleanup_message(
        CleanupEvent::AttachmentOrphanFile,
        chat,
        id,
        Some(file),
    ))
}

async fn deliver(env: &AttEnv, payload: &Value, attempts: i16) -> MessageResult {
    env.cleanup_handler()
        .handle(&AttEnv::message(payload, attempts))
        .await
}

fn is_ok(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Ok)
}

fn is_retry(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Retry)
}

fn is_reject(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Reject(_))
}

// ================================================================================================
// the file is deleted whatever the row says
// ================================================================================================

#[tokio::test]
async fn the_file_of_an_orphan_message_is_deleted_although_the_row_is_missing() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;

    let result = deliver(&env, &orphan(&env, chat, Uuid::new_v4(), "file-lost"), 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(
        env.sequence(),
        vec![format!("DELETE /{ALIAS}/v1/files/file-lost")]
    );
    assert_eq!(env.oagw.proxied()[0].subject, S2S_SUBJECT);
    assert_eq!(env.metrics.count(&Metric::CleanupCompleted), 1);
}

#[tokio::test]
async fn the_row_state_is_irrelevant_and_no_row_is_written() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let rows = [
        // soft-deleted with cleanup done
        env.seed(chat, 1, |row| {
            row.set("deleted_at", ts(5)).set("cleanup_status", "done")
        })
        .await,
        // failed / upload_abandoned with no cleanup status
        env.seed(chat, 2, |row| {
            row.set("status", "failed")
                .set("error_code", "upload_abandoned")
        })
        .await,
        // pending, uploaded, ready with and without cleanup states
        env.seed(chat, 3, |row| {
            row.set("status", "pending").null("provider_file_id")
        })
        .await,
        env.seed(chat, 4, |row| {
            row.set("status", "uploaded")
                .set("cleanup_status", "pending")
        })
        .await,
        env.seed(chat, 5, |row| row.set("cleanup_status", "failed"))
            .await,
    ];
    let before = env.attachments_dump().await;

    for id in rows {
        let result = deliver(&env, &orphan(&env, chat, id, "file-lost"), 0).await;

        assert!(is_ok(&result), "{id}: {result:?}");
    }

    assert_eq!(env.deletes().len(), 5, "one DELETE per message");
    assert_eq!(
        env.attachments_dump().await,
        before,
        "no cleanup column is written"
    );
}

#[tokio::test]
async fn a_soft_deleted_chat_does_not_stop_the_orphan_delete() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    env.delete_chat(chat).await;

    let result = deliver(&env, &orphan(&env, chat, id, "file-lost"), 0).await;

    assert!(
        is_ok(&result),
        "chat cleanup cannot know this id: {result:?}"
    );
    assert_eq!(
        env.sequence(),
        vec![format!("DELETE /{ALIAS}/v1/files/file-lost")]
    );
}

#[tokio::test]
async fn a_404_is_success_so_a_redelivery_is_harmless() {
    let env = AttEnv::new().await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 404, &json!({}));
    let chat = env.chat("m-oa").await;
    let message = orphan(&env, chat, Uuid::new_v4(), "file-lost");

    let first = deliver(&env, &message, 0).await;
    let second = deliver(&env, &message, 1).await;

    assert!(is_ok(&first) && is_ok(&second));
}

#[tokio::test]
async fn the_secondary_copy_of_an_orphan_message_is_deleted_best_effort() {
    let env = AttEnv::new().await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        &format!("/{ANTHROPIC_ALIAS}/v1/files/*"),
        500,
        &json!({}),
    );
    let chat = env.chat("m-an").await;
    let mut message = env.cleanup_message(
        CleanupEvent::AttachmentOrphanFile,
        chat,
        Uuid::new_v4(),
        Some("file-lost"),
    );
    message.secondary_ref = Some(SecondaryRef {
        file_id: "file_ant_9".to_owned(),
        upstream_alias: ANTHROPIC_ALIAS.to_owned(),
    });

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(
        is_ok(&result),
        "a failed secondary delete never blocks the outcome: {result:?}"
    );
    assert_eq!(
        env.sequence(),
        vec![
            format!("DELETE /{ALIAS}/v1/files/file-lost"),
            format!("DELETE /{ANTHROPIC_ALIAS}/v1/files/file_ant_9"),
        ]
    );
    assert_eq!(env.metrics.count(&Metric::SecondarySkipped), 1);
}

// ================================================================================================
// retries are bounded by the delivery attempt count
// ================================================================================================

#[tokio::test]
async fn a_failing_delete_retries_until_the_delivery_attempts_reach_the_budget() {
    let env = AttEnv::new().await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 500, &json!({}));
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "failed")
                .set("error_code", "upload_abandoned")
        })
        .await;
    let before = env.attachments_dump().await;
    let message = orphan(&env, chat, id, "file-lost");

    let first = deliver(&env, &message, 0).await;
    let second = deliver(&env, &message, 1).await;
    let third = deliver(&env, &message, 2).await;

    assert!(is_retry(&first), "{first:?}");
    assert!(is_retry(&second), "{second:?}");
    assert!(
        is_reject(&third),
        "the third delivery of max_attempts = 3 is terminal: {third:?}"
    );
    assert_eq!(
        env.attachments_dump().await,
        before,
        "no row's cleanup_attempts changes"
    );
    assert_eq!(
        (
            env.metrics.count(&Metric::CleanupRetry),
            env.metrics.count(&Metric::CleanupFailed)
        ),
        (2, 1)
    );
}

#[tokio::test]
async fn a_late_delivery_beyond_the_budget_is_rejected_at_once() {
    let env = AttEnv::new().await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 503, &json!({}));
    let chat = env.chat("m-oa").await;

    let result = deliver(&env, &orphan(&env, chat, Uuid::new_v4(), "file-lost"), 9).await;

    assert!(is_reject(&result), "{result:?}");
}

#[tokio::test]
async fn a_transport_failure_and_an_unknown_backend_follow_the_same_budget() {
    let env = AttEnv::new().await;
    env.oagw
        .push_proxy(ProxyReply::Error(Fault::DeadlineExceeded));
    let chat = env.chat("m-oa").await;
    let message = orphan(&env, chat, Uuid::new_v4(), "file-lost");
    let mut unknown = message.clone();
    unknown["storage_backend"] = json!("gcs");

    let transport = deliver(&env, &message, 0).await;
    let unconfigured = deliver(&env, &unknown, 0).await;
    let unconfigured_last = deliver(&env, &unknown, 2).await;

    assert!(is_retry(&transport));
    assert!(is_retry(&unconfigured));
    assert!(is_reject(&unconfigured_last));
    assert_eq!(
        env.oagw.proxy_call_count(),
        1,
        "an unknown backend makes no call"
    );
}

#[tokio::test]
async fn an_orphan_delete_that_does_not_answer_is_cut_by_its_timeout() {
    let env = AttEnv::new().await;
    env.oagw.push_proxy(ProxyReply::Pending);
    let chat = env.chat("m-oa").await;
    let handler = env.cleanup_handler_timeout(Duration::from_millis(100));

    let result = handler
        .handle(&AttEnv::message(
            &orphan(&env, chat, Uuid::new_v4(), "file-lost"),
            0,
        ))
        .await;

    assert!(is_retry(&result), "{result:?}");
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 200, &json!({}));
    let answered = deliver(&env, &orphan(&env, chat, Uuid::new_v4(), "file-lost"), 0).await;
    assert!(
        is_ok(&answered),
        "an answering provider is fine: {answered:?}"
    );
}

#[tokio::test]
async fn without_the_s2s_context_an_orphan_message_is_retried() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let handler = env.cleanup_handler_with_runtime(TestRuntime::unbound());

    let result = handler
        .handle(&AttEnv::message(
            &orphan(&env, chat, Uuid::new_v4(), "file-lost"),
            0,
        ))
        .await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    env.script_happy();
    let started = deliver(&env, &orphan(&env, chat, Uuid::new_v4(), "file-lost"), 0).await;
    assert!(is_ok(&started), "{started:?}");
}

// ================================================================================================
// the variant never touches the store
// ================================================================================================

fn handler_over(
    repo: &Arc<FakeRepo>,
    oagw: &Arc<FakeOagw>,
) -> (AttachmentCleanupHandler, Arc<RecordingAttachmentMetrics>) {
    let directory = Arc::new(ProviderResolver::new(&registry_ordered(&[(
        "openai",
        provider_json(ALIAS, "openai"),
    )])));
    let metrics = RecordingAttachmentMetrics::new();
    let handler = AttachmentCleanupHandler::new(CleanupHandlerDeps {
        repo: Arc::clone(repo) as Arc<dyn AttachmentRepository>,
        storage: Arc::new(OagwStorageClient::new(oagw.clone())) as Arc<dyn StorageClient>,
        directory: directory as Arc<dyn StorageDirectory>,
        metrics: Arc::clone(&metrics) as Arc<dyn AttachmentMetrics>,
        runtime: TestRuntime::bound() as Arc<dyn BackgroundRuntime>,
        max_attempts: 3,
        clock: Arc::new(crate::domain::clock::now_micros),
        delete_timeout: Duration::from_secs(10),
    });
    (handler, metrics)
}

fn orphan_json(file: &str) -> Value {
    json!({
        "event_type": "attachment_orphan_file",
        "tenant_id": USER_TENANT.to_string(),
        "chat_id": Uuid::from_u128(0xC1).to_string(),
        "attachment_id": Uuid::from_u128(0xA1).to_string(),
        "provider_file_id": file,
        "vector_store_id": null,
        "storage_backend": "openai",
        "attachment_kind": "document",
        "deleted_at": "2026-03-10T12:00:00Z",
        "secondary_ref": null,
    })
}

#[tokio::test]
async fn an_orphan_message_reads_and_writes_no_chat_or_attachment_row() {
    let repo = FakeRepo::new();
    let oagw = FakeOagw::new();
    oagw.on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 200, &json!({}));
    let (handler, _) = handler_over(&repo, &oagw);

    for (payload, attempts) in [(orphan_json("file-1"), 0), (orphan_json("file-2"), 2)] {
        let result = handler.handle(&AttEnv::message(&payload, attempts)).await;
        assert!(is_ok(&result));
    }
    oagw.on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 500, &json!({}));
    let failed = handler
        .handle(&AttEnv::message(&orphan_json("file-3"), 2))
        .await;
    assert!(is_reject(&failed));

    let state = repo.state.lock().unwrap();
    assert!(
        state.calls.is_empty(),
        "no store call at all, success, retry or reject: {:?}",
        state.calls
    );
}

#[tokio::test]
async fn a_row_bearing_message_reads_the_chat_and_the_row() {
    let repo = FakeRepo::new();
    let oagw = FakeOagw::new();
    let (handler, _) = handler_over(&repo, &oagw);
    let mut message = orphan_json("file-1");
    message["event_type"] = json!("attachment_deleted");

    let result = handler.handle(&AttEnv::message(&message, 0)).await;

    assert!(is_reject(&result), "no such row: {result:?}");
    assert!(
        repo.state.lock().unwrap().calls.contains(&"cleanup_row")
            || repo.state.lock().unwrap().calls.contains(&"chat_deleted"),
        "the contrast with the orphan variant"
    );
}
