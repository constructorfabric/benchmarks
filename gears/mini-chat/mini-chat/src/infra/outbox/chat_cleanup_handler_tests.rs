#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the chat cleanup handler that replaces the foundation placeholder
//! (`cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`,
//! `-clean-chat-attachment`, `-clean-chat-vector-store`,
//! `cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup`,
//! `cpt-cf-mini-chat-dod-background-workers-chat-cleanup-handler`,
//! `cpt-cf-mini-chat-dod-background-workers-vector-store-cleanup`): the exact
//! provider calls, the attachment state machine and its attempts, the vector
//! store ordering rule, the Reject cases and the best-effort secondary copy.
//! Real Secure ORM stores on `SQLite`, the real OAGW storage client over the fake
//! gateway.

use std::sync::Arc;

use serde_json::{Value, json};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use tracing::Level;
use uuid::Uuid;

use super::{ChatCleanupPayload, decode_chat_cleanup};
use crate::attachment_test_env::ANTHROPIC_ALIAS;
use crate::domain::service::quota::test_support::PolicyStep;
use crate::domain::service::workers::metrics::CleanupResource::{File, VectorStore};
use crate::domain::service::workers::test_support::Metric;
use crate::test_doubles::{
    OTHER_TENANT, OTHER_USER, ProxyReply, S2S_SUBJECT, USER_TENANT, outbox_message,
};
use crate::worker_test_env::{WorkerEnv, WorkerOptions, cleanup_message};

const FILES: &str = "/api.openai.test/v1/files";

fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn rejected(result: &MessageResult) -> Option<&str> {
    match result {
        MessageResult::Reject(reason) => Some(reason),
        _ => None,
    }
}

/// A deleted chat on `model` with `n` owed documents `1..=n` and a bound
/// vector store `vs_chat`.
async fn deleted_chat(env: &WorkerEnv, model: &str, n: u128) -> Uuid {
    let chat = env.m.stream.seed_chat(50, model).await;
    for k in 1..=n {
        env.owed(chat, k).await;
    }
    env.store_row(chat, Some("vs_chat"), "openai").await;
    env.soft_delete_chat(chat).await;
    chat
}

async fn deliver(env: &WorkerEnv, chat: Uuid, attempts: i16) -> MessageResult {
    env.deliver_cleanup(chat, attempts).await
}

fn file_delete(n: u128) -> String {
    format!("DELETE {FILES}/file-prov{n}")
}

fn store_delete() -> String {
    "DELETE /api.openai.test/v1/vector_stores/vs_chat".to_owned()
}

fn sequence(env: &WorkerEnv) -> Vec<String> {
    env.oagw
        .proxied()
        .iter()
        .map(|call| {
            format!(
                "{} {}",
                call.method,
                call.uri.split('?').next().unwrap_or_default()
            )
        })
        .collect()
}

// ================================================================================================
// the happy path through the API
// ================================================================================================

#[tokio::test]
async fn a_deleted_chat_releases_its_files_its_secondary_copy_and_its_vector_store_exactly_once() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-an").await;
    // two ready documents (one in the vector store), one XLSX and one image with
    // an uploaded Anthropic secondary copy
    env.m
        .seed_attachment(chat, 1, |row| row.set("storage_backend", "openai"))
        .await;
    env.m
        .seed_attachment(chat, 2, |row| row.set("storage_backend", "openai"))
        .await;
    env.m
        .seed_attachment(chat, 3, |row| {
            row.set("storage_backend", "openai")
                .set("filename", "data.xlsx")
                .set("for_file_search", false)
                .set("for_code_interpreter", true)
        })
        .await;
    env.seed_image_with_secondary(chat, 4, "file_ant_7").await;
    env.store_row(chat, Some("vs_chat"), "openai").await;

    env.delete_chat_via_api(chat).await;

    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "the 204 is answered before any provider call"
    );
    let messages = env.cleanup_queue().await;
    assert_eq!(messages.len(), 1);
    let before = env.untouched_snapshot().await;
    let handler = env.chat_cleanup_handler();
    let message = outbox_message(env.cleanup_payload(0).await, 0);

    let result = handler.handle(&message).await;

    assert!(is_ok(&result), "{result:?}");
    // one delete per provider file, one secondary delete, the store last
    let deletes = env.delete_paths();
    assert_eq!(deletes.len(), 6, "{deletes:?}");
    for n in 1..=4 {
        let wanted = format!("{FILES}/file-prov{n}");
        assert_eq!(
            deletes.iter().filter(|path| **path == wanted).count(),
            1,
            "{deletes:?}"
        );
    }
    let secondary = format!("/{ANTHROPIC_ALIAS}/v1/files/file_ant_7");
    assert_eq!(deletes.iter().filter(|path| **path == secondary).count(), 1);
    assert_eq!(
        deletes.last().map(String::as_str),
        Some("/api.openai.test/v1/vector_stores/vs_chat"),
        "the store is deleted after the last file delete: {deletes:?}"
    );
    let secondary_call = env
        .oagw
        .proxied()
        .into_iter()
        .find(|call| call.uri.ends_with("/file_ant_7"))
        .unwrap();
    assert_eq!(
        secondary_call.header("anthropic-version"),
        Some("2023-06-01")
    );
    assert_eq!(
        secondary_call.header("anthropic-beta"),
        Some("files-api-2025-04-14")
    );
    for call in env.oagw.proxied() {
        assert_eq!(
            call.subject, S2S_SUBJECT,
            "the S2S context is used outside a request"
        );
    }
    for id in env.att_ids_of(chat).await {
        let snap = env.att(id).await;
        assert_eq!(snap.cleanup_status.as_deref(), Some("done"), "{id}");
        assert!(
            snap.deleted_at.is_some() || snap.status == "ready",
            "status and ids stay"
        );
        assert!(snap.provider_file_id.is_some(), "the provider ids stay");
    }
    assert!(
        env.stores().await.is_empty(),
        "the chat_vector_stores row is gone"
    );
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(File)), 4);
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(VectorStore)), 1);
    assert_eq!(env.metrics.count(Metric::CleanupRetry(File)), 0);
    assert_eq!(
        env.untouched_snapshot().await,
        before,
        "no messages, turns or chats row changed"
    );
    assert!(env.audits().await.is_empty(), "no audit event");
    assert_eq!(env.quota.outbox_rows("mini-chat.audit").await.len(), 0);

    // a redelivery of the acknowledged message makes no provider call
    let calls = env.oagw.proxy_call_count();
    let again = handler.handle(&message).await;
    assert!(is_ok(&again), "{again:?}");
    assert_eq!(env.oagw.proxy_call_count(), calls);
}

#[tokio::test]
async fn a_404_for_the_files_the_secondary_copy_and_the_store_gives_the_same_end_state() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw
        .on_path_json("DELETE", &format!("{FILES}/*"), 404, &json!({}));
    env.oagw.on_path_json(
        "DELETE",
        &format!("/{ANTHROPIC_ALIAS}/v1/files/*"),
        404,
        &json!({}),
    );
    env.oagw.on_path_json(
        "DELETE",
        "/api.openai.test/v1/vector_stores/*",
        404,
        &json!({}),
    );
    let chat = env.m.stream.seed_chat(50, "m-an").await;
    env.owed(chat, 1).await;
    env.owed_with(chat, 2, |row| {
        row.set("secondary_status", "uploaded")
            .set("secondary_file_id", "file_ant_1")
    })
    .await;
    env.store_row(chat, Some("vs_chat"), "openai").await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    for id in env.att_ids_of(chat).await {
        assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    }
    assert!(env.stores().await.is_empty());
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(File)), 2);
    assert_eq!(
        env.metrics.count(Metric::SecondaryCleanupSkipped),
        0,
        "a 404 is a success"
    );
}

#[tokio::test]
async fn an_attachment_row_stays_as_it_was_and_only_its_cleanup_columns_change() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let id = env.att_ids_of(chat).await[0];
    let before = env.att(id).await;

    deliver(&env, chat, 0).await;

    let after = env.att(id).await;
    assert_eq!(after.status, before.status);
    assert_eq!(after.deleted_at, before.deleted_at);
    assert_eq!(after.provider_file_id, before.provider_file_id);
    assert_eq!(after.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        after.cleanup_updated_at.as_deref(),
        Some(crate::attachment_test_env::AttEnv::stamp(env.now()).as_str())
    );
    assert_eq!(
        after.cleanup_attempts, 0,
        "no attempt is counted for a success"
    );
}

#[tokio::test]
async fn a_chat_with_nothing_to_clean_is_released_on_the_first_delivery() {
    let env = WorkerEnv::new().await;
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert!(env.metrics.events().is_empty());
}

#[tokio::test]
async fn the_real_pipeline_consumes_the_message_after_the_204_and_releases_the_provider() {
    let env = WorkerEnv::with(WorkerOptions {
        real_pipeline: true,
        ..WorkerOptions::default()
    })
    .await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    let id = env
        .m
        .seed_attachment(chat, 1, |row| row.set("storage_backend", "openai"))
        .await;
    env.store_row(chat, Some("vs_chat"), "openai").await;

    env.delete_chat_via_api(chat).await;

    env.wait_for("the handler releases the chat", || async {
        env.stores().await.is_empty() && env.att(id).await.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(env.delete_paths().len(), 2, "one file and one store");
}

// ================================================================================================
// failures
// ================================================================================================

#[tokio::test]
async fn a_file_delete_failing_twice_keeps_the_attachment_pending_and_the_store_untouched_until_it_succeeds()
 {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw.on_path_sequence(
        "DELETE",
        &format!("{FILES}/file-prov2"),
        vec![
            (500, json!({})),
            (500, json!({})),
            (200, json!({ "deleted": true })),
        ],
    );
    let chat = deleted_chat(&env, "m-oa", 3).await;
    let failing = env.att_ids_of(chat).await[1];

    let first = deliver(&env, chat, 0).await;
    let second = deliver(&env, chat, 1).await;

    assert!(
        is_retry(&first) && is_retry(&second),
        "{first:?} {second:?}"
    );
    let snap = env.att(failing).await;
    assert_eq!(snap.cleanup_attempts, 2);
    assert_eq!(snap.last_cleanup_error.as_deref(), Some("http 500"));
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        snap.cleanup_updated_at.as_deref(),
        Some(crate::attachment_test_env::AttEnv::stamp(env.now()).as_str()),
        "a failed attempt stamps cleanup_updated_at"
    );
    assert_eq!(env.metrics.count(Metric::CleanupRetry(File)), 2);
    assert!(
        !sequence(&env).contains(&store_delete()),
        "the store is not deleted while an attachment is pending: {:?}",
        sequence(&env)
    );
    // the attachments that succeeded are not deleted again
    assert_eq!(
        sequence(&env)
            .iter()
            .filter(|c| **c == file_delete(1))
            .count(),
        1
    );

    let third = deliver(&env, chat, 2).await;

    assert!(is_ok(&third), "{third:?}");
    assert_eq!(
        env.att(failing).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert!(sequence(&env).contains(&store_delete()));
    assert!(env.stores().await.is_empty());
}

#[tokio::test]
async fn a_file_that_never_deletes_ends_failed_at_the_attempt_limit_while_the_rest_and_the_store_are_released()
 {
    let env = WorkerEnv::with(WorkerOptions {
        max_cleanup_attempts: 3,
        ..WorkerOptions::default()
    })
    .await;
    env.script_happy();
    env.oagw
        .on_path_json("DELETE", &format!("{FILES}/file-prov2"), 500, &json!({}));
    let chat = deleted_chat(&env, "m-oa", 3).await;
    let ids = env.att_ids_of(chat).await;

    let first = deliver(&env, chat, 0).await;
    let second = deliver(&env, chat, 1).await;
    let third = deliver(&env, chat, 2).await;

    assert!(is_retry(&first) && is_retry(&second));
    assert!(
        is_ok(&third),
        "the store is released in the same delivery: {third:?}"
    );
    let failed = env.att(ids[1]).await;
    assert_eq!(failed.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(failed.cleanup_attempts, 3);
    assert_eq!(failed.last_cleanup_error.as_deref(), Some("http 500"));
    assert_eq!(
        failed.provider_file_id.as_deref(),
        Some("file-prov2"),
        "the debt keeps its provider id"
    );
    for other in [ids[0], ids[2]] {
        assert_eq!(env.att(other).await.cleanup_status.as_deref(), Some("done"));
    }
    assert_eq!(env.metrics.count(Metric::CleanupFailed(File)), 1);
    assert_eq!(env.metrics.count(Metric::CleanupRetry(File)), 2);
    assert_eq!(
        env.metrics.count(Metric::CleanupStoreWithFailedAttachments),
        1
    );
    assert!(env.stores().await.is_empty(), "the store is still deleted");
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(VectorStore)), 1);
}

#[tokio::test]
async fn the_debt_metric_is_recorded_before_each_store_delete_and_a_retried_delete_counts_again() {
    let env = WorkerEnv::with(WorkerOptions {
        max_cleanup_attempts: 5,
        ..WorkerOptions::default()
    })
    .await;
    env.script_happy();
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let id = env.att_ids_of(chat).await[0];
    env.exec(&format!(
        "UPDATE attachments SET cleanup_status = 'failed' WHERE id = {}",
        crate::chat_test_env::blob(id)
    ))
    .await;
    env.oagw.on_path_sequence(
        "DELETE",
        "/api.openai.test/v1/vector_stores/vs_chat",
        vec![(500, json!({})), (200, json!({}))],
    );

    let first = deliver(&env, chat, 0).await;
    let second = deliver(&env, chat, 1).await;

    assert!(is_retry(&first) && is_ok(&second));
    assert_eq!(
        env.metrics.count(Metric::CleanupStoreWithFailedAttachments),
        2
    );
}

#[tokio::test]
async fn a_store_delete_that_always_fails_retries_until_the_limit_then_rejects_and_keeps_the_row() {
    let env = WorkerEnv::with(WorkerOptions {
        max_cleanup_attempts: 3,
        ..WorkerOptions::default()
    })
    .await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        "/api.openai.test/v1/vector_stores/vs_chat",
        500,
        &json!({}),
    );
    let chat = deleted_chat(&env, "m-oa", 1).await;

    let first = deliver(&env, chat, 0).await;
    let second = deliver(&env, chat, 1).await;
    let third = deliver(&env, chat, 2).await;

    assert!(
        is_retry(&first) && is_retry(&second),
        "{first:?} {second:?}"
    );
    assert_eq!(
        rejected(&third),
        Some("vector store delete: max attempts (3) reached"),
        "{third:?}"
    );
    assert_eq!(env.metrics.count(Metric::CleanupRetry(VectorStore)), 3);
    assert_eq!(env.metrics.count(Metric::CleanupFailed(VectorStore)), 1);
    assert_eq!(env.stores().await.len(), 1, "the row is kept for a replay");
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(VectorStore)), 0);

    // the dead letter is replayed after the cause is fixed
    env.oagw.on_path_json(
        "DELETE",
        "/api.openai.test/v1/vector_stores/vs_chat",
        200,
        &json!({}),
    );
    let replay = deliver(&env, chat, 3).await;

    assert!(is_ok(&replay), "{replay:?}");
    assert!(
        env.stores().await.is_empty(),
        "the replay deletes the store and its row"
    );
}

#[tokio::test]
async fn a_failure_of_the_store_call_before_the_limit_is_retry_and_the_delivery_number_is_attempts_plus_one()
 {
    let env = WorkerEnv::with(WorkerOptions {
        max_cleanup_attempts: 5,
        ..WorkerOptions::default()
    })
    .await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        "/api.openai.test/v1/vector_stores/vs_chat",
        503,
        &json!({}),
    );
    let chat = deleted_chat(&env, "m-oa", 0).await;

    for attempts in 0..4 {
        let result = deliver(&env, chat, attempts).await;
        assert!(is_retry(&result), "attempts {attempts}: {result:?}");
    }
    let last = deliver(&env, chat, 4).await;

    assert_eq!(
        rejected(&last),
        Some("vector store delete: max attempts (5) reached"),
        "the fifth delivery reaches the limit"
    );
}

#[tokio::test]
async fn a_database_failure_after_the_provider_delete_is_retry_without_counting_an_attempt() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let id = env.att_ids_of(chat).await[0];
    env.exec("CREATE TRIGGER fail_att BEFORE UPDATE ON attachments BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result), "{result:?}");
    let snap = env.att(id).await;
    assert_eq!(
        snap.cleanup_attempts, 0,
        "a database failure is not an attempt"
    );
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    assert!(
        env.metrics
            .events()
            .iter()
            .all(|m| !matches!(m, Metric::CleanupRetry(_) | Metric::CleanupFailed(_)))
    );

    env.exec("DROP TRIGGER fail_att").await;
    let healed = deliver(&env, chat, 1).await;
    assert!(is_ok(&healed), "{healed:?}");
}

#[tokio::test]
async fn a_database_failure_while_recording_a_failed_attempt_is_retry_and_changes_nothing() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw
        .on_path_json("DELETE", &format!("{FILES}/file-prov1"), 500, &json!({}));
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let id = env.att_ids_of(chat).await[0];
    env.exec("CREATE TRIGGER fail_att BEFORE UPDATE ON attachments BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(env.att(id).await.cleanup_attempts, 0);
    assert_eq!(env.metrics.count(Metric::CleanupRetry(File)), 0);
}

#[tokio::test]
async fn a_failing_read_of_the_chat_is_retry_with_no_limit_and_no_provider_call() {
    let env = WorkerEnv::with(WorkerOptions {
        max_cleanup_attempts: 2,
        ..WorkerOptions::default()
    })
    .await;
    let chat = deleted_chat(&env, "m-oa", 1).await;
    env.exec("ALTER TABLE chats RENAME TO chats_hidden").await;

    for attempts in [0, 1, 7, 500] {
        let result = deliver(&env, chat, attempts).await;
        assert!(is_retry(&result), "attempts {attempts}: {result:?}");
    }

    assert_eq!(env.oagw.proxy_call_count(), 0);
    env.exec("ALTER TABLE chats_hidden RENAME TO chats").await;
}

#[tokio::test]
async fn a_failing_read_of_the_pending_attachments_is_retry() {
    let env = WorkerEnv::new().await;
    let chat = deleted_chat(&env, "m-oa", 1).await;
    env.exec("ALTER TABLE attachments RENAME TO attachments_hidden")
        .await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    env.exec("ALTER TABLE attachments_hidden RENAME TO attachments")
        .await;
}

// ================================================================================================
// reject
// ================================================================================================

#[tokio::test]
async fn a_message_for_an_active_chat_is_rejected_and_nothing_is_touched() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    env.owed(chat, 1).await;
    env.store_row(chat, Some("vs_chat"), "openai").await;
    let before = (env.attachments_dump().await, env.stores().await);

    let result = deliver(&env, chat, 0).await;

    assert_eq!(rejected(&result), Some("chat is not soft-deleted"));
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!((env.attachments_dump().await, env.stores().await), before);
}

#[tokio::test]
async fn a_message_for_an_unknown_chat_is_rejected() {
    let env = WorkerEnv::new().await;

    let result = deliver(&env, Uuid::from_u128(0xDEAD), 0).await;

    assert_eq!(rejected(&result), Some("chat not found"));
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_message_naming_the_wrong_tenant_finds_no_chat_and_touches_no_row() {
    let env = WorkerEnv::new().await;
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let before = env.attachments_dump().await;

    let result = env
        .chat_cleanup_handler()
        .handle(&cleanup_message(OTHER_TENANT, chat, env.now(), 0))
        .await;

    assert_eq!(rejected(&result), Some("chat not found"));
    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

fn valid_payload(chat: Uuid) -> Value {
    json!({
        "tenant_id": USER_TENANT.to_string(),
        "chat_id": chat.to_string(),
        "system_request_id": Uuid::from_u128(0x5157).to_string(),
        "reason": "chat_soft_delete",
        "chat_deleted_at": "2026-03-10T12:00:00Z",
    })
}

fn without(mut payload: Value, key: &str) -> Value {
    payload.as_object_mut().unwrap().remove(key);
    payload
}

fn with(mut payload: Value, key: &str, value: Value) -> Value {
    payload[key] = value;
    payload
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_without_touching_anything() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let before = env.attachments_dump().await;
    let ok = valid_payload(chat);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not json", b"not json".to_vec()),
        ("an array", b"[]".to_vec()),
        ("an empty object", b"{}".to_vec()),
        (
            "missing tenant",
            serde_json::to_vec(&without(ok.clone(), "tenant_id")).unwrap(),
        ),
        (
            "missing chat",
            serde_json::to_vec(&without(ok.clone(), "chat_id")).unwrap(),
        ),
        (
            "missing request id",
            serde_json::to_vec(&without(ok.clone(), "system_request_id")).unwrap(),
        ),
        (
            "missing reason",
            serde_json::to_vec(&without(ok.clone(), "reason")).unwrap(),
        ),
        (
            "missing time",
            serde_json::to_vec(&without(ok.clone(), "chat_deleted_at")).unwrap(),
        ),
        (
            "unknown key",
            serde_json::to_vec(&with(ok.clone(), "extra", json!(1))).unwrap(),
        ),
        (
            "tenant not a uuid",
            serde_json::to_vec(&with(ok.clone(), "tenant_id", json!("x"))).unwrap(),
        ),
        (
            "chat not a uuid",
            serde_json::to_vec(&with(ok.clone(), "chat_id", json!(7))).unwrap(),
        ),
        (
            "time not rfc3339",
            serde_json::to_vec(&with(ok.clone(), "chat_deleted_at", json!("yesterday"))).unwrap(),
        ),
        (
            "wrong reason",
            serde_json::to_vec(&with(ok.clone(), "reason", json!("temporary_chat"))).unwrap(),
        ),
    ];

    for (name, bytes) in cases {
        let result = env
            .chat_cleanup_handler()
            .handle(&outbox_message(bytes, 0))
            .await;
        assert!(rejected(&result).is_some(), "{name}: {result:?}");
    }

    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.attachments_dump().await, before);
}

#[test]
fn the_payload_of_the_chat_soft_delete_decodes() {
    let payload = ChatCleanupPayload::new(
        USER_TENANT,
        Uuid::from_u128(0xC1),
        crate::domain::clock::now_micros(),
    );

    let decoded = decode_chat_cleanup(&payload.encode().unwrap()).unwrap();

    assert_eq!(decoded, payload);
}

#[test]
fn a_decode_error_names_the_problem_without_the_payload_content() {
    let error = decode_chat_cleanup(b"{\"tenant_id\":\"secret-value\"}").unwrap_err();

    assert!(!error.to_string().contains("secret-value"), "{error}");
}

#[tokio::test]
async fn rows_of_another_chat_or_tenant_are_never_read_or_changed() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = deleted_chat(&env, "m-oa", 1).await;
    // another deleted chat of the same tenant and a chat of another tenant, each with a pending file
    let other_chat = env.m.stream.seed_chat(51, "m-oa").await;
    env.owed(other_chat, 9).await;
    env.soft_delete_chat(other_chat).await;
    let foreign_chat = env.quota.seed_chat_of(52, OTHER_TENANT, OTHER_USER).await;
    env.quota
        .db
        .insert(
            "attachments",
            crate::infra::db::test_support::attachment_row(
                Uuid::from_u128(0xF100_0777),
                OTHER_TENANT,
                foreign_chat,
            )
            .set("storage_backend", "openai")
            .set("provider_file_id", "file-foreign")
            .set("cleanup_status", "pending")
            .set("deleted_at", crate::chat_test_env::ts(5)),
        )
        .await
        .unwrap();
    let (a, b) = (Uuid::from_u128(0xF100_0009), Uuid::from_u128(0xF100_0777));
    let before = (env.att(a).await, env.att(b).await);

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!((env.att(a).await, env.att(b).await), before);
    assert!(
        !sequence(&env)
            .iter()
            .any(|call| call.contains("file-prov9") || call.contains("file-foreign"))
    );
}

// ================================================================================================
// the user-deleted attachment and the attachment cleanup handler
// ================================================================================================

#[tokio::test]
async fn an_attachment_the_user_deleted_before_the_chat_is_cleaned_by_the_chat_handler() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    // the user deleted the attachment (soft-deleted, cleanup still pending), then the chat
    let id = env.owed(chat, 1).await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(sequence(&env), vec![file_delete(1)]);
}

// ================================================================================================
// edge cases
// ================================================================================================

#[tokio::test]
async fn a_placeholder_store_row_is_deleted_without_a_provider_call() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    env.store_row(chat, None, "openai").await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert!(env.stores().await.is_empty());
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "no provider store was recorded"
    );
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(VectorStore)), 0);
}

#[tokio::test]
async fn a_bound_row_is_never_deleted_as_a_placeholder() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        "/api.openai.test/v1/vector_stores/vs_chat",
        500,
        &json!({}),
    );
    let chat = deleted_chat(&env, "m-oa", 0).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result));
    assert_eq!(env.stores().await.len(), 1);
}

#[tokio::test]
async fn an_attachment_row_without_a_cleanup_status_in_the_deleted_chat_is_claimed_and_cleaned() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    // a racing upload inserted its row after the chat delete marked the others
    let straggler = env
        .m
        .seed_attachment(chat, 1, |row| {
            row.set("storage_backend", "openai")
                .set("status", "uploaded")
        })
        .await;
    env.soft_delete_chat(chat).await;
    assert_eq!(env.att(straggler).await.cleanup_status, None);

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(
        env.att(straggler).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(sequence(&env), vec![file_delete(1)]);
    // the racing upload's later CAS (`cleanup_status IS NULL`) fails
    let repo = crate::infra::db::repos::attachments::SeaAttachmentRepository::new(
        Arc::clone(&env.quota.provider),
        Arc::clone(&env.quota.enqueuer),
    );
    let set = crate::domain::service::attachments::ports::AttachmentRepository::set_uploaded(
        &repo,
        USER_TENANT,
        straggler,
        "file-late",
        env.now(),
    )
    .await
    .unwrap();
    assert!(!set, "the upload lost its CAS");
}

#[tokio::test]
async fn an_attachment_whose_upload_never_reached_the_provider_is_done_without_a_call() {
    let env = WorkerEnv::new().await;
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    let id = env
        .owed_with(chat, 1, |row| {
            row.set("status", "pending").null("provider_file_id")
        })
        .await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(
        env.metrics.count(Metric::CleanupCompleted(File)),
        0,
        "no provider file was deleted"
    );
}

#[tokio::test]
async fn a_storage_backend_without_an_entry_is_a_failed_attempt_with_its_cause() {
    let env = WorkerEnv::new().await;
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    let id = env
        .owed_with(chat, 1, |row| row.set("storage_backend", "retired-backend"))
        .await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result), "{result:?}");
    let snap = env.att(id).await;
    assert_eq!(snap.cleanup_attempts, 1);
    assert_eq!(
        snap.last_cleanup_error.as_deref(),
        Some("storage backend not configured")
    );
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_gateway_failure_is_a_failed_attempt_named_gateway_error() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw.on("DELETE", "file-prov1", |_| {
        ProxyReply::Error(crate::test_doubles::Fault::Conflict)
    });
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let id = env.att_ids_of(chat).await[0];

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(
        env.att(id).await.last_cleanup_error.as_deref(),
        Some("gateway error")
    );
}

#[tokio::test]
async fn any_4xx_other_than_404_is_a_failed_attempt() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw
        .on_path_json("DELETE", &format!("{FILES}/file-prov1"), 403, &json!({}));
    let chat = deleted_chat(&env, "m-oa", 1).await;
    let id = env.att_ids_of(chat).await[0];

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result));
    assert_eq!(
        env.att(id).await.last_cleanup_error.as_deref(),
        Some("http 403")
    );
}

// ---- the secondary copy ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unresolvable_secondary_alias_skips_the_secondary_but_still_deletes_the_file() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    // the chat model is no longer in the catalog: the alias cannot be resolved at delete time
    let chat = env.m.stream.seed_chat(50, "retired-model").await;
    let id = env.seed_image_with_secondary(chat, 1, "file_ant_1").await;
    env.mark_pending(id).await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(sequence(&env), vec![file_delete(1)], "no secondary call");
    assert_eq!(env.metrics.count(Metric::SecondaryCleanupSkipped), 1);
}

#[tokio::test]
async fn a_chat_model_that_is_not_served_by_anthropic_has_no_secondary_target() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    let id = env.seed_image_with_secondary(chat, 1, "file_ant_1").await;
    env.mark_pending(id).await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result));
    assert_eq!(sequence(&env), vec![file_delete(1)]);
    assert_eq!(env.metrics.count(Metric::SecondaryCleanupSkipped), 1);
}

#[tokio::test]
async fn a_policy_plugin_failure_skips_the_secondary_and_never_fails_the_delivery() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.quota.policy.fail(PolicyStep::Snapshot);
    let chat = env.m.stream.seed_chat(50, "m-an").await;
    let id = env.seed_image_with_secondary(chat, 1, "file_ant_1").await;
    env.mark_pending(id).await;
    env.soft_delete_chat(chat).await;
    let logs = crate::test_doubles::LogCapture::start();

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.metrics.count(Metric::SecondaryCleanupSkipped), 1);
    assert!(!logs.at(Level::WARN).is_empty(), "a warning names the chat");
}

#[tokio::test]
async fn the_secondary_alias_is_resolved_once_per_delivery_from_the_current_snapshot() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-an").await;
    for n in 1..=3 {
        let id = env
            .seed_image_with_secondary(chat, n, &format!("file_ant_{n}"))
            .await;
        env.mark_pending(id).await;
    }
    env.soft_delete_chat(chat).await;

    deliver(&env, chat, 0).await;

    let snapshots = env
        .quota
        .policy
        .calls()
        .into_iter()
        .filter(|call| {
            matches!(
                call,
                crate::domain::service::quota::test_support::PolicyCall::Snapshot(..)
            )
        })
        .count();
    assert_eq!(snapshots, 1, "one resolution for three secondary copies");
    assert_eq!(
        env.delete_paths()
            .iter()
            .filter(|p| p.contains("/v1/files/file_ant_"))
            .count(),
        3
    );
}

#[tokio::test]
async fn a_secondary_delete_that_fails_is_best_effort_and_the_attachment_is_done() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        &format!("/{ANTHROPIC_ALIAS}/v1/files/*"),
        500,
        &json!({}),
    );
    let chat = env.m.stream.seed_chat(50, "m-an").await;
    let id = env.seed_image_with_secondary(chat, 1, "file_ant_1").await;
    env.mark_pending(id).await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_ok(&result), "{result:?}");
    let snap = env.att(id).await;
    assert_eq!(snap.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        snap.cleanup_attempts, 0,
        "the secondary never counts as an attempt"
    );
    assert_eq!(env.metrics.count(Metric::SecondaryCleanupSkipped), 1);
}

#[tokio::test]
async fn the_secondary_is_not_touched_while_the_primary_file_delete_fails() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw
        .on_path_json("DELETE", &format!("{FILES}/file-prov1"), 500, &json!({}));
    let chat = env.m.stream.seed_chat(50, "m-an").await;
    let id = env.seed_image_with_secondary(chat, 1, "file_ant_1").await;
    env.mark_pending(id).await;
    env.soft_delete_chat(chat).await;

    let result = deliver(&env, chat, 0).await;

    assert!(is_retry(&result));
    assert!(
        !sequence(&env)
            .iter()
            .any(|call| call.contains("file_ant_1")),
        "{:?}",
        sequence(&env)
    );
}

#[tokio::test]
async fn the_delivery_makes_one_pass_per_attachment_in_creation_order() {
    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = deleted_chat(&env, "m-oa", 3).await;

    deliver(&env, chat, 0).await;

    let files: Vec<String> = sequence(&env)
        .into_iter()
        .filter(|c| c.contains("/files/"))
        .collect();
    assert_eq!(files, vec![file_delete(1), file_delete(2), file_delete(3)]);
}

#[tokio::test]
async fn no_message_content_or_provider_id_reaches_a_log_line_at_warn_or_above() {
    let logs = crate::test_doubles::LogCapture::start();
    let env = WorkerEnv::new().await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        &format!("{FILES}/file-prov1"),
        500,
        &json!({ "error": { "message": "secret provider text" } }),
    );
    let chat = deleted_chat(&env, "m-oa", 1).await;

    deliver(&env, chat, 0).await;

    let text = logs.all_text();
    assert!(!text.contains("secret provider text"), "{text}");
}

// ================================================================================================
// the attachment cleanup handler steps aside for a deleted chat
// ================================================================================================

#[tokio::test]
async fn the_attachment_cleanup_handler_steps_aside_for_the_chat_that_the_chat_handler_owns() {
    use crate::domain::service::attachments::model::AttachmentKind;
    use crate::domain::service::attachments::payload::{CleanupEvent, CleanupMessage};
    use crate::domain::service::attachments::ports::{
        AttachmentMetrics, AttachmentRepository, BackgroundRuntime, StorageClient, StorageDirectory,
    };
    use crate::domain::service::attachments::test_support::RecordingAttachmentMetrics;
    use crate::infra::db::repos::attachments::SeaAttachmentRepository;
    use crate::infra::outbox::attachment_cleanup::{
        AttachmentCleanupHandler, CleanupHandlerDeps, encode_cleanup,
    };

    let env = WorkerEnv::new().await;
    env.script_happy();
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    // the user deleted the attachment: its own cleanup is still pending when the chat is deleted
    let id = env.owed(chat, 1).await;
    env.soft_delete_chat(chat).await;
    let handler = AttachmentCleanupHandler::new(CleanupHandlerDeps {
        repo: Arc::new(SeaAttachmentRepository::new(
            Arc::clone(&env.quota.provider),
            Arc::clone(&env.quota.enqueuer),
        )) as Arc<dyn AttachmentRepository>,
        storage: Arc::clone(&env.storage) as Arc<dyn StorageClient>,
        directory: Arc::clone(&env.directory) as Arc<dyn StorageDirectory>,
        metrics: RecordingAttachmentMetrics::new() as Arc<dyn AttachmentMetrics>,
        runtime: Arc::clone(&env.runtime) as Arc<dyn BackgroundRuntime>,
        max_attempts: 5,
        clock: env.wall(),
        delete_timeout: std::time::Duration::from_secs(10),
    });
    let payload = encode_cleanup(&CleanupMessage {
        event: CleanupEvent::AttachmentDeleted,
        tenant_id: USER_TENANT,
        chat_id: chat,
        attachment_id: id,
        provider_file_id: Some("file-prov1".to_owned()),
        storage_backend: "openai".to_owned(),
        kind: AttachmentKind::Document,
        deleted_at: env.now(),
        secondary_ref: None,
    })
    .unwrap();

    let stepped_aside = handler.handle(&outbox_message(payload, 0)).await;

    assert!(is_ok(&stepped_aside), "{stepped_aside:?}");
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "the attachment handler makes no provider call"
    );
    assert_eq!(
        env.att(id).await.cleanup_status.as_deref(),
        Some("pending"),
        "the row is left to the chat handler"
    );

    let cleaned = deliver(&env, chat, 0).await;

    assert!(is_ok(&cleaned), "{cleaned:?}");
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(sequence(&env), vec![file_delete(1)]);
}
