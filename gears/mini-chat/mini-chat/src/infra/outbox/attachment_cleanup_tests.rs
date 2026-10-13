#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the attachment cleanup handler that replaces the foundation
//! placeholder (`cpt-cf-mini-chat-algo-attachments-handle-cleanup`, row-bearing
//! events): provider file delete, secondary delete, the `cleanup_status` state
//! machine, attempts, and the Ok / Retry / Reject results.

use std::time::Duration;

use serde_json::{Value, json};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use uuid::Uuid;

use crate::attachment_test_env::{ALIAS, ANTHROPIC_ALIAS, AZURE_ALIAS, AttEnv, AttOptions};
use crate::chat_test_env::ts;
use crate::domain::service::attachments::payload::{CleanupEvent, SecondaryRef};
use crate::domain::service::attachments::test_support::{Metric, TestRuntime};
use crate::test_doubles::{Fault, ProxyReply, S2S_SUBJECT, USER_TENANT};

/// A row of a live chat that is owed provider cleanup.
async fn owed(env: &AttEnv, chat: Uuid, n: u128) -> Uuid {
    env.seed(chat, n, |row| {
        row.set("deleted_at", ts(5))
            .set("cleanup_status", "pending")
            .set("provider_file_id", format!("file-prov{n}"))
    })
    .await
}

fn payload(env: &AttEnv, event: CleanupEvent, chat: Uuid, id: Uuid, file: Option<&str>) -> Value {
    AttEnv::payload_json(&env.cleanup_message(event, chat, id, file))
}

async fn deliver(env: &AttEnv, payload: &Value, attempts: i16) -> MessageResult {
    env.cleanup_handler()
        .handle(&AttEnv::message(payload, attempts))
        .await
}

fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn is_reject(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Reject(_))
}

fn no_vector_store_call(env: &AttEnv) {
    assert!(
        env.oagw
            .proxied()
            .iter()
            .all(|c| !c.uri.contains("vector_stores")),
        "the handler never calls the Vector Stores API: {:?}",
        env.sequence()
    );
}

// ================================================================================================
// success
// ================================================================================================

#[tokio::test]
async fn a_deleted_message_deletes_the_provider_file_and_marks_the_row_done() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;

    let result = deliver(
        &env,
        &payload(
            &env,
            CleanupEvent::AttachmentDeleted,
            chat,
            id,
            Some("file-prov1"),
        ),
        0,
    )
    .await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(
        env.sequence(),
        vec![format!("DELETE /{ALIAS}/v1/files/file-prov1")]
    );
    let call = &env.oagw.proxied()[0];
    assert_eq!(
        call.subject, S2S_SUBJECT,
        "outside a request the S2S context is used"
    );
    let snap = env.att(id).await;
    assert_eq!(snap.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        snap.cleanup_updated_at.as_deref(),
        Some(AttEnv::stamp(env.now()).as_str())
    );
    assert_eq!(snap.cleanup_attempts, 0);
    assert_eq!(env.metrics.count(&Metric::CleanupCompleted), 1);
    no_vector_store_call(&env);
}

#[tokio::test]
async fn a_404_from_the_provider_also_counts_as_done() {
    let env = AttEnv::new().await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 404, &json!({}));
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;

    let result = deliver(
        &env,
        &payload(
            &env,
            CleanupEvent::AttachmentDeleted,
            chat,
            id,
            Some("file-prov1"),
        ),
        0,
    )
    .await;

    assert!(is_ok(&result));
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn every_row_bearing_event_is_handled_the_same_way() {
    for event in [
        CleanupEvent::AttachmentDeleted,
        CleanupEvent::AttachmentUploadAbandoned,
        CleanupEvent::AttachmentIndexingFailed,
        CleanupEvent::AttachmentUploadFailed,
    ] {
        let env = AttEnv::new().await;
        env.script_happy();
        let chat = env.chat("m-oa").await;
        let id = owed(&env, chat, 1).await;

        let result = deliver(&env, &payload(&env, event, chat, id, Some("file-prov1")), 0).await;

        assert!(is_ok(&result), "{}: {result:?}", event.as_str());
        assert_eq!(env.deletes().len(), 1, "{}", event.as_str());
        assert_eq!(
            env.att(id).await.cleanup_status.as_deref(),
            Some("done"),
            "{}",
            event.as_str()
        );
    }
}

#[tokio::test]
async fn an_azure_backend_deletes_through_the_azure_route() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-az").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("deleted_at", ts(5))
                .set("cleanup_status", "pending")
                .set("storage_backend", "azure")
        })
        .await;
    let mut message = env.cleanup_message(
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    message.storage_backend = "azure".to_owned();

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(is_ok(&result));
    assert_eq!(
        env.oagw.proxied()[0].uri,
        format!("/{AZURE_ALIAS}/openai/files/file-prov1?api-version=2025-01-01-preview")
    );
}

#[tokio::test]
async fn a_message_without_a_provider_file_is_done_without_a_provider_call() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("deleted_at", ts(5))
                .set("cleanup_status", "pending")
        })
        .await;

    let result = deliver(
        &env,
        &payload(&env, CleanupEvent::AttachmentDeleted, chat, id, None),
        0,
    )
    .await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        env.metrics.count(&Metric::CleanupCompleted),
        0,
        "no provider file was deleted"
    );
}

// ================================================================================================
// failures and attempts
// ================================================================================================

#[tokio::test]
async fn a_provider_failure_counts_an_attempt_and_retries_until_the_budget_is_spent() {
    let env = AttEnv::new().await;
    env.oagw.on_path_json(
        "DELETE",
        &format!("/{ALIAS}/v1/files/*"),
        500,
        &json!({ "error": "boom" }),
    );
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    let message = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );

    let first = deliver(&env, &message, 0).await;
    let after_first = env.att(id).await;
    let second = deliver(&env, &message, 1).await;
    let third = deliver(&env, &message, 2).await;

    assert!(is_retry(&first), "{first:?}");
    assert_eq!(after_first.cleanup_attempts, 1);
    assert_eq!(after_first.last_cleanup_error.as_deref(), Some("http 500"));
    assert_eq!(after_first.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        after_first.cleanup_updated_at.as_deref(),
        Some(AttEnv::stamp(env.now()).as_str())
    );
    assert!(is_retry(&second), "{second:?}");
    assert!(
        is_reject(&third),
        "the third failure reaches max_attempts (3): {third:?}"
    );
    let last = env.att(id).await;
    assert_eq!(last.cleanup_attempts, 3);
    assert_eq!(
        last.cleanup_status.as_deref(),
        Some("failed"),
        "terminal provider-file debt"
    );
    assert_eq!(
        (
            env.metrics.count(&Metric::CleanupRetry),
            env.metrics.count(&Metric::CleanupFailed),
            env.metrics.count(&Metric::CleanupCompleted)
        ),
        (2, 1, 0)
    );
    assert_eq!(env.deletes().len(), 3);
}

#[tokio::test]
async fn any_non_2xx_non_404_status_is_a_failed_attempt_with_its_status_as_the_cause() {
    for status in [400_u16, 401, 403, 409, 429, 502] {
        let env = AttEnv::new().await;
        env.oagw.on_path_json(
            "DELETE",
            &format!("/{ALIAS}/v1/files/*"),
            status,
            &json!({}),
        );
        let chat = env.chat("m-oa").await;
        let id = owed(&env, chat, 1).await;

        let result = deliver(
            &env,
            &payload(
                &env,
                CleanupEvent::AttachmentDeleted,
                chat,
                id,
                Some("file-prov1"),
            ),
            0,
        )
        .await;

        assert!(is_retry(&result), "{status}: {result:?}");
        assert_eq!(
            env.att(id).await.last_cleanup_error.as_deref(),
            Some(format!("http {status}").as_str())
        );
    }
}

#[tokio::test]
async fn a_gateway_failure_is_a_failed_attempt_named_gateway_error() {
    for fault in [Fault::Internal, Fault::DeadlineExceeded] {
        let env = AttEnv::new().await;
        env.oagw.push_proxy(ProxyReply::Error(fault));
        let chat = env.chat("m-oa").await;
        let id = owed(&env, chat, 1).await;

        let result = deliver(
            &env,
            &payload(
                &env,
                CleanupEvent::AttachmentDeleted,
                chat,
                id,
                Some("file-prov1"),
            ),
            0,
        )
        .await;

        assert!(is_retry(&result), "{fault:?}");
        assert_eq!(
            env.att(id).await.last_cleanup_error.as_deref(),
            Some("gateway error"),
            "{fault:?}"
        );
    }
}

#[tokio::test]
async fn a_delete_that_does_not_answer_is_cut_by_its_timeout() {
    let env = AttEnv::new().await;
    env.oagw.push_proxy(ProxyReply::Pending);
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    let handler = env.cleanup_handler_timeout(Duration::from_millis(100));
    let message = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );

    let started = std::time::Instant::now();
    let result = handler.handle(&AttEnv::message(&message, 0)).await;

    assert!(is_retry(&result), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let snap = env.att(id).await;
    assert_eq!(snap.last_cleanup_error.as_deref(), Some("timeout"));
    assert_eq!(snap.cleanup_attempts, 1);
}

#[tokio::test]
async fn an_unconfigured_storage_backend_is_a_failed_attempt_without_a_provider_call() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    let mut message = env.cleanup_message(
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    message.storage_backend = "gcs".to_owned();

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    let snap = env.att(id).await;
    assert_eq!(
        snap.last_cleanup_error.as_deref(),
        Some("storage backend not configured")
    );
    assert_eq!(snap.cleanup_attempts, 1);
    assert_eq!(env.metrics.count(&Metric::CleanupRetry), 1);
}

#[tokio::test]
async fn a_database_failure_while_counting_the_attempt_retries_without_counting() {
    let env = AttEnv::new().await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 500, &json!({}));
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    env.fail_updates(
        "no_attempts",
        "NEW.cleanup_attempts <> OLD.cleanup_attempts",
    )
    .await;

    let message = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );

    let result = deliver(&env, &message, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(env.att(id).await.cleanup_attempts, 0);
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("pending"));
    env.heal("no_attempts").await;
    let counted = deliver(&env, &message, 1).await;
    assert!(is_retry(&counted));
    assert_eq!(
        env.att(id).await.cleanup_attempts,
        1,
        "without the failure the attempt is counted"
    );
}

#[tokio::test]
async fn a_database_failure_while_marking_done_retries_and_a_redelivery_finishes() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    let message = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    env.fail_updates("no_done", "NEW.cleanup_status = 'done'")
        .await;

    let first = deliver(&env, &message, 0).await;
    env.heal("no_done").await;
    let second = deliver(&env, &message, 1).await;

    assert!(is_retry(&first), "{first:?}");
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert!(
        is_ok(&second),
        "the repeated delete is harmless: {second:?}"
    );
    assert_eq!(env.deletes().len(), 2);
}

// ================================================================================================
// redelivery and ownership
// ================================================================================================

#[tokio::test]
async fn a_redelivery_for_a_finished_row_is_ok_without_a_provider_call() {
    for state in ["done", "failed"] {
        let env = AttEnv::new().await;
        let chat = env.chat("m-oa").await;
        let id = env
            .seed(chat, 1, |row| {
                row.set("deleted_at", ts(5)).set("cleanup_status", state)
            })
            .await;
        let before = env.attachments_dump().await;

        let result = deliver(
            &env,
            &payload(
                &env,
                CleanupEvent::AttachmentDeleted,
                chat,
                id,
                Some("file-prov1"),
            ),
            3,
        )
        .await;

        assert!(is_ok(&result), "{state}: {result:?}");
        assert_eq!(env.oagw.proxy_call_count(), 0, "{state}");
        assert_eq!(env.attachments_dump().await, before, "{state}");
    }
}

#[tokio::test]
async fn a_soft_deleted_chat_is_acknowledged_without_action() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    env.delete_chat(chat).await;
    let before = env.attachments_dump().await;

    let result = deliver(
        &env,
        &payload(
            &env,
            CleanupEvent::AttachmentDeleted,
            chat,
            id,
            Some("file-prov1"),
        ),
        0,
    )
    .await;

    assert!(is_ok(&result), "chat cleanup owns its files: {result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn a_missing_chat_is_acknowledged_without_action() {
    let env = AttEnv::new().await;
    let result = deliver(
        &env,
        &payload(
            &env,
            CleanupEvent::AttachmentDeleted,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Some("file-1"),
        ),
        0,
    )
    .await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_missing_row_is_rejected_without_a_provider_call() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;

    let result = deliver(
        &env,
        &payload(
            &env,
            CleanupEvent::AttachmentDeleted,
            chat,
            Uuid::new_v4(),
            Some("file-1"),
        ),
        0,
    )
    .await;

    assert!(is_reject(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_malformed_payload_is_rejected() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    let handler = env.cleanup_handler();
    let mut unknown = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    unknown["event_type"] = json!("attachment_exploded");
    let mut missing = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    missing.as_object_mut().unwrap().remove("tenant_id");
    let mut bad_id = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    bad_id["attachment_id"] = json!("nope");

    for bytes in [
        b"not json".to_vec(),
        Vec::new(),
        serde_json::to_vec(&unknown).unwrap(),
        serde_json::to_vec(&missing).unwrap(),
        serde_json::to_vec(&bad_id).unwrap(),
    ] {
        let result = handler
            .handle(&crate::test_doubles::outbox_message(bytes, 0))
            .await;

        assert!(is_reject(&result), "{result:?}");
    }
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn without_the_s2s_context_the_message_is_retried() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let id = owed(&env, chat, 1).await;
    let handler = env.cleanup_handler_with_runtime(TestRuntime::unbound());
    let message = payload(
        &env,
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );

    let result = handler.handle(&AttEnv::message(&message, 0)).await;

    assert!(is_retry(&result), "the gear has not started: {result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(
        env.att(id).await.cleanup_attempts,
        0,
        "not a provider failure"
    );
    let started = deliver(&env, &message, 1).await;
    assert!(
        is_ok(&started),
        "once the gear has started the same message is handled: {started:?}"
    );
}

// ================================================================================================
// the secondary copy
// ================================================================================================

fn with_secondary(message: &mut crate::domain::service::attachments::payload::CleanupMessage) {
    message.secondary_ref = Some(SecondaryRef {
        file_id: "file_ant_1".to_owned(),
        upstream_alias: ANTHROPIC_ALIAS.to_owned(),
    });
}

#[tokio::test]
async fn a_secondary_reference_deletes_the_anthropic_copy_after_the_primary_file() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-an").await;
    let id = owed(&env, chat, 1).await;
    let mut message = env.cleanup_message(
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    with_secondary(&mut message);

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(
        env.sequence(),
        vec![
            format!("DELETE /{ALIAS}/v1/files/file-prov1"),
            format!("DELETE /{ANTHROPIC_ALIAS}/v1/files/file_ant_1"),
        ]
    );
    let secondary = &env.oagw.proxied()[1];
    assert_eq!(secondary.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        secondary.header("anthropic-beta"),
        Some("files-api-2025-04-14")
    );
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn a_failed_secondary_delete_never_blocks_done() {
    let env = AttEnv::new().await;
    env.script_happy();
    env.oagw.on_path_json(
        "DELETE",
        &format!("/{ANTHROPIC_ALIAS}/v1/files/*"),
        500,
        &json!({}),
    );
    let chat = env.chat("m-an").await;
    let id = owed(&env, chat, 1).await;
    let mut message = env.cleanup_message(
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    with_secondary(&mut message);

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(env.metrics.count(&Metric::SecondarySkipped), 1);
    assert_eq!(
        env.att(id).await.cleanup_attempts,
        0,
        "not counted as a failed attempt"
    );
}

#[tokio::test]
async fn a_failing_primary_delete_leaves_the_secondary_copy_for_the_retry() {
    let env = AttEnv::new().await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 500, &json!({}));
    env.oagw.on_path_json(
        "DELETE",
        &format!("/{ANTHROPIC_ALIAS}/v1/files/*"),
        200,
        &json!({}),
    );
    let chat = env.chat("m-an").await;
    let id = owed(&env, chat, 1).await;
    let mut message = env.cleanup_message(
        CleanupEvent::AttachmentDeleted,
        chat,
        id,
        Some("file-prov1"),
    );
    with_secondary(&mut message);

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(is_retry(&result));
    assert_eq!(
        env.deletes().len(),
        1,
        "the secondary delete waits for the primary one"
    );
}

#[tokio::test]
async fn a_message_without_a_provider_file_skips_the_secondary_delete_too() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-an").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("deleted_at", ts(5))
                .set("cleanup_status", "pending")
                .null("provider_file_id")
        })
        .await;
    let mut message = env.cleanup_message(CleanupEvent::AttachmentDeleted, chat, id, None);
    with_secondary(&mut message);

    let result = deliver(&env, &AttEnv::payload_json(&message), 0).await;

    assert!(is_ok(&result));
    assert_eq!(env.oagw.proxy_call_count(), 0);
    let _ = USER_TENANT;
}

// ================================================================================================
// the wiring
// ================================================================================================

#[tokio::test]
async fn the_handler_replaces_the_placeholder_on_the_started_pipeline() {
    let env = AttEnv::with(AttOptions {
        real_cleanup: true,
        ..AttOptions::default()
    })
    .await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;

    let reply = env.delete_attachment(chat, id).await;
    assert_eq!(reply.status.as_u16(), 204, "{}", reply.text);

    env.wait_for(
        "the cleanup message is delivered and the row is done",
        || async { env.att(id).await.cleanup_status.as_deref() == Some("done") },
    )
    .await;
    assert_eq!(
        env.sequence(),
        vec![format!("DELETE /{ALIAS}/v1/files/file-prov1")]
    );
}
