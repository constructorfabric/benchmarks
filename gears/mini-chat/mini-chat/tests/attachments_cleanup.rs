#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Provider-side cleanup (DESIGN "Cleanup on Chat Deletion", "Attachment cleanup state machine",
//! "Vector store cleanup ordering", "Attachment Deletion" phase 2, B.9.2, B.9.3): the attachment
//! cleanup handler (queue `mini-chat.attachment_cleanup`) and the chat cleanup handler (queue
//! `mini-chat.chat_cleanup`), driven by the real outbox pipeline.
//!
//! Messages of the gear (attachment `DELETE`, chat `DELETE`) are handled as soon as they are
//! enqueued; hand-made messages are injected with `enqueue_raw_outbox`. Retries wait for the shared
//! outbox backoff, so a few tests take a couple of seconds.

mod attachments;
mod billing;
mod common;
mod live;

use attachments::{
    ATTACHMENT_CLEANUP_QUEUE, CHAT_CLEANUP_QUEUE, RowSeed, call_lines, calls, calls_to, chat_rows,
    delete_attachment, delete_ok, deletes, detail_id, drain_cleanup, file_path, position_of,
    provider_error, row, seed_store, settle, store_rows, upload_ready_document, upload_ready_image,
    vector_store_path, wait_calls, wait_outcomes, wait_queue_handled, wait_row,
};
use common::{chat_id, chat_row, create_default_chat, delete_chat, exec_sql, new_chat};
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

async fn env_with_max_attempts(max_attempts: u32) -> TestEnv {
    TestEnvBuilder::new()
        .gear_config(json!({ "cleanup_worker": { "max_attempts": max_attempts } }))
        .build()
        .await
}

/// An attachment cleanup message as the gear writes it.
fn cleanup_message(
    user: &TestUser,
    chat: &str,
    attachment: Uuid,
    provider_file: Option<&str>,
    event_type: &str,
    secondary: Option<&Value>,
) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "event_type": event_type,
        "tenant_id": user.tenant_id,
        "chat_id": chat,
        "attachment_id": attachment,
        "provider_file_id": provider_file,
        "vector_store_id": null,
        "storage_backend": "openai",
        "attachment_kind": "document",
        "deleted_at": "2026-01-02T00:00:00Z",
        "secondary_ref": secondary,
    }))
    .unwrap()
}

/// A chat cleanup message as the soft delete writes it.
fn chat_cleanup_message(user: &TestUser, chat: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "tenant_id": user.tenant_id,
        "chat_id": chat,
        "system_request_id": Uuid::new_v4(),
        "reason": "chat_soft_delete",
        "chat_deleted_at": "2026-01-02T00:00:00Z",
    }))
    .unwrap()
}

/// Makes the next `count` deletes of `provider_file` fail with `status`.
fn fail_file_deletes(env: &TestEnv, provider_file: &str, status: u16, count: usize) {
    for _ in 0..count {
        env.provider().enqueue(
            &format!("/v1/files/{provider_file}"),
            provider_error(status),
        );
    }
}

fn fail_store_deletes(env: &TestEnv, vs: &str, status: u16, count: usize) {
    for _ in 0..count {
        env.provider()
            .enqueue(&format!("/v1/vector_stores/{vs}"), provider_error(status));
    }
}

/// A chat with one uploaded document `file-doc1` in `vs_chat1`, as a ready attachment.
async fn chat_with_document() -> (TestEnv, TestUser, String, Uuid) {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let detail =
        upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    (env, user, chat, detail_id(&detail))
}

// ===========================================================================
// Attachment cleanup handler
// ===========================================================================

#[tokio::test]
async fn a_deleted_document_has_its_provider_file_deleted_and_the_cleanup_marked_done() {
    let (env, user, chat, id) = chat_with_document().await;

    delete_ok(&env, &user, &chat, id).await;

    assert_eq!(
        wait_queue_handled(&env, ATTACHMENT_CLEANUP_QUEUE).await,
        vec!["ok"]
    );
    let done = row(&env, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert!(done.deleted_at.is_some());
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-doc1")).len(), 1);
    assert_eq!(
        deletes(&env),
        vec![file_path("file-doc1")],
        "no Vector Stores API call: the store is deleted with the chat"
    );
}

#[tokio::test]
async fn a_deleted_image_has_its_provider_file_deleted() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let id = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await);

    delete_ok(&env, &user, &chat, id).await;

    wait_queue_handled(&env, ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(row(&env, id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(deletes(&env), vec![file_path("file-img1")]);
}

#[tokio::test]
async fn a_provider_404_counts_as_success() {
    let (env, user, chat, id) = chat_with_document().await;
    fail_file_deletes(&env, "file-doc1", 404, 1);

    delete_ok(&env, &user, &chat, id).await;

    assert_eq!(
        wait_queue_handled(&env, ATTACHMENT_CLEANUP_QUEUE).await,
        vec!["ok"]
    );
    let done = row(&env, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert_eq!(done.cleanup_attempts, 0);
}

#[tokio::test]
async fn any_other_failing_status_is_a_failed_attempt_that_asks_for_a_retry() {
    for status in [400, 403, 500, 503] {
        let (env, user, chat, id) = chat_with_document().await;
        fail_file_deletes(&env, "file-doc1", status, 5);

        delete_ok(&env, &user, &chat, id).await;

        let attempted = wait_row(&env, id, "a recorded failed attempt", |row| {
            row.cleanup_attempts >= 1
        })
        .await;
        assert_eq!(
            attempted.cleanup_status.as_deref(),
            Some("pending"),
            "status {status}"
        );
        assert!(
            attempted
                .last_cleanup_error
                .as_deref()
                .is_some_and(|error| !error.is_empty()),
            "status {status}: the error of the attempt is recorded"
        );
        assert_eq!(
            wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await[0],
            "retry"
        );
    }
}

#[tokio::test]
async fn a_retry_that_succeeds_marks_the_cleanup_done() {
    let (env, user, chat, id) = chat_with_document().await;
    fail_file_deletes(&env, "file-doc1", 500, 1);

    delete_ok(&env, &user, &chat, id).await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 2).await,
        vec!["retry", "ok"]
    );
    let done = wait_row(&env, id, "the finished cleanup", |row| {
        row.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(done.cleanup_attempts, 1, "the failed attempt stays counted");
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-doc1")).len(), 2);
}

#[tokio::test]
async fn the_attempt_limit_makes_the_cleanup_failed_and_rejects_the_message() {
    let env = env_with_max_attempts(2).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let id = detail_id(
        &upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await,
    );
    fail_file_deletes(&env, "file-doc1", 500, 5);

    delete_ok(&env, &user, &chat, id).await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 2).await,
        vec!["retry", "reject"]
    );
    let failed = row(&env, id).await;
    assert_eq!(failed.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(failed.cleanup_attempts, 2);
    assert!(failed.last_cleanup_error.is_some());
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-doc1")).len(), 2);
    let dead = billing::dead_letters(&env, ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(dead.len(), 1, "the rejected message is dead-lettered");
    let payload: Value = serde_json::from_slice(&dead[0].payload).unwrap();
    assert_eq!(payload["event_type"], json!("attachment_deleted"));
    assert_eq!(payload["provider_file_id"], json!("file-doc1"));
}

#[tokio::test]
async fn a_database_failure_asks_for_a_retry_without_counting_an_attempt() {
    let (env, user, chat, id) = chat_with_document().await;
    exec_sql(
        &env,
        "CREATE TRIGGER fail_cleanup_done BEFORE UPDATE ON attachments \
         WHEN NEW.cleanup_status = 'done' BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    )
    .await;

    delete_ok(&env, &user, &chat, id).await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await[0],
        "retry"
    );
    let pending = row(&env, id).await;
    assert_eq!(pending.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        pending.cleanup_attempts, 0,
        "an infrastructure failure is not a provider attempt"
    );
    exec_sql(&env, "DROP TRIGGER fail_cleanup_done").await;
    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 2).await[1],
        "ok"
    );
    assert_eq!(row(&env, id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn a_message_for_a_deleted_chat_is_acknowledged_without_action() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = chat_id(&create_default_chat(&env, &user).await);
    delete_chat(&env, &user, &chat).await;
    wait_queue_handled(&env, CHAT_CLEANUP_QUEUE).await;
    let attachment = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-orphan")
        .deleted()
        .cleanup("pending")
        .insert(&env)
        .await;
    let before = call_lines(&env);

    env.enqueue_raw_outbox(
        ATTACHMENT_CLEANUP_QUEUE,
        user.tenant_id,
        &cleanup_message(
            &user,
            &chat,
            attachment,
            Some("file-orphan"),
            "attachment_deleted",
            None,
        ),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await,
        vec!["ok"]
    );
    assert_eq!(
        call_lines(&env),
        before,
        "chat cleanup owns the provider files"
    );
    assert_eq!(
        row(&env, attachment).await.cleanup_status.as_deref(),
        Some("pending"),
        "the row is left to the chat cleanup"
    );
}

#[tokio::test]
async fn an_attachment_without_a_provider_file_is_marked_done_without_a_provider_call() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let attachment = RowSeed::new(&user, &chat)
        .status("failed")
        .error_code("upload_failed")
        .deleted()
        .cleanup("pending")
        .insert(&env)
        .await;

    env.enqueue_raw_outbox(
        ATTACHMENT_CLEANUP_QUEUE,
        user.tenant_id,
        &cleanup_message(&user, &chat, attachment, None, "attachment_deleted", None),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await,
        vec!["ok"]
    );
    assert_eq!(
        row(&env, attachment).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn abandoned_upload_and_failed_indexing_events_delete_the_provider_file_too() {
    for event_type in ["attachment_upload_abandoned", "attachment_indexing_failed"] {
        let env = TestEnv::new().await;
        let user = TestUser::new();
        let chat = new_chat(&env, &user).await;
        let attachment = RowSeed::new(&user, &chat)
            .status("failed")
            .error_code("upload_abandoned")
            .provider_file("file-stale")
            .cleanup("pending")
            .insert(&env)
            .await;

        env.enqueue_raw_outbox(
            ATTACHMENT_CLEANUP_QUEUE,
            user.tenant_id,
            &cleanup_message(
                &user,
                &chat,
                attachment,
                Some("file-stale"),
                event_type,
                None,
            ),
        )
        .await;

        assert_eq!(
            wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await,
            vec!["ok"],
            "{event_type}"
        );
        let done = wait_row(&env, attachment, "the finished cleanup", |row| {
            row.cleanup_status.as_deref() == Some("done")
        })
        .await;
        assert_eq!(
            done.status, "failed",
            "{event_type}: the row is not soft-deleted"
        );
        assert!(done.deleted_at.is_none());
        assert_eq!(deletes(&env), vec![file_path("file-stale")], "{event_type}");
    }
}

#[tokio::test]
async fn malformed_messages_are_rejected_at_once_and_dead_lettered() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    for payload in [
        b"not json at all".to_vec(),
        br#"{"event_type":"attachment_deleted"}"#.to_vec(),
    ] {
        env.enqueue_raw_outbox(ATTACHMENT_CLEANUP_QUEUE, user.tenant_id, &payload)
            .await;
    }

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 2).await,
        vec!["reject", "reject"],
        "a malformed payload is never retried"
    );
    assert_eq!(
        billing::dead_letters(&env, ATTACHMENT_CLEANUP_QUEUE)
            .await
            .len(),
        2
    );
    assert!(calls(&env).is_empty());
}

async fn env_with_anthropic() -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "providers": { "anthropic": {
            "kind": "anthropic_messages",
            "host": "anthropic.test",
            "api_path": "/v1/messages",
            "storage_kind": "openai",
            "rag_provider": "openai",
            "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "auth_config": {
                "header": "x-api-key", "prefix": "", "secret_ref": "cred://anthropic-key"
            }
        } } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

fn secondary_ref() -> Value {
    json!({ "file_id": "file_011sec", "provider_kind": "anthropic", "upstream_alias": "anthropic.test" })
}

#[tokio::test]
async fn the_secondary_anthropic_file_is_deleted_after_the_primary_file() {
    let (env, user, chat) = env_with_anthropic().await;
    let attachment = RowSeed::new(&user, &chat)
        .image()
        .status("ready")
        .provider_file("file-primary")
        .secondary("file_011sec")
        .deleted()
        .cleanup("pending")
        .insert(&env)
        .await;

    env.enqueue_raw_outbox(
        ATTACHMENT_CLEANUP_QUEUE,
        user.tenant_id,
        &cleanup_message(
            &user,
            &chat,
            attachment,
            Some("file-primary"),
            "attachment_deleted",
            Some(&secondary_ref()),
        ),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await,
        vec!["ok"]
    );
    let removed = deletes(&env);
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert_eq!(removed[0], file_path("file-primary"));
    assert!(
        removed[1].starts_with("/anthropic.test/") && removed[1].ends_with("file_011sec"),
        "the secondary copy is deleted through its upstream alias: {removed:?}"
    );
    assert_eq!(
        row(&env, attachment).await.cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn a_failing_secondary_delete_does_not_block_the_cleanup() {
    let (env, user, chat) = env_with_anthropic().await;
    let attachment = RowSeed::new(&user, &chat)
        .image()
        .status("ready")
        .provider_file("file-primary")
        .secondary("file_011sec")
        .deleted()
        .cleanup("pending")
        .insert(&env)
        .await;
    env.provider()
        .enqueue("/anthropic.test/", provider_error(500));

    env.enqueue_raw_outbox(
        ATTACHMENT_CLEANUP_QUEUE,
        user.tenant_id,
        &cleanup_message(
            &user,
            &chat,
            attachment,
            Some("file-primary"),
            "attachment_deleted",
            Some(&secondary_ref()),
        ),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, ATTACHMENT_CLEANUP_QUEUE, 1).await,
        vec!["ok"]
    );
    assert_eq!(
        row(&env, attachment).await.cleanup_status.as_deref(),
        Some("done")
    );
}

// ===========================================================================
// Chat cleanup handler
// ===========================================================================

#[tokio::test]
async fn deleting_a_chat_removes_every_provider_resource_in_the_background() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let document = detail_id(
        &upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await,
    );
    let image = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await);
    assert_eq!(store_rows(&env, &chat).await.len(), 1);

    let resp = delete_chat(&env, &user, &chat).await;
    assert_eq!(resp.status.as_u16(), 204);
    drain_cleanup(&env).await;

    assert_eq!(env.outbox_outcomes(CHAT_CLEANUP_QUEUE).await, vec!["ok"]);
    let removed = deletes(&env);
    assert_eq!(removed.len(), 3, "{removed:?}");
    assert!(removed.contains(&file_path("file-doc1")));
    assert!(removed.contains(&file_path("file-img1")));
    assert_eq!(
        removed.last(),
        Some(&vector_store_path("vs_chat1")),
        "the store goes last"
    );
    let store_at = position_of(&env, "DELETE", &vector_store_path("vs_chat1")).unwrap();
    for file in ["file-doc1", "file-img1"] {
        assert!(
            position_of(&env, "DELETE", &file_path(file)).unwrap() < store_at,
            "{file} is deleted before the vector store"
        );
    }
    for id in [document, image] {
        let cleaned = row(&env, id).await;
        assert_eq!(cleaned.cleanup_status.as_deref(), Some("done"));
        assert_eq!(cleaned.cleanup_attempts, 0);
    }
    assert!(
        store_rows(&env, &chat).await.is_empty(),
        "the chat_vector_stores row is the completion marker"
    );
    assert!(
        chat_row(&env, &chat).await.unwrap().deleted,
        "soft-deleted rows are never purged"
    );
    assert_eq!(chat_rows(&env, &chat).await.len(), 2);
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty(),
        "the chat cleanup does not go through the attachment queue"
    );
}

#[tokio::test]
async fn a_chat_deletion_leaves_other_chats_untouched() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let doomed = new_chat(&env, &user).await;
    let kept = new_chat(&env, &user).await;
    upload_ready_document(
        &env,
        &user,
        &doomed,
        "a.pdf",
        "file-doomed",
        "vs_doomed",
        true,
    )
    .await;
    let kept_doc = detail_id(
        &upload_ready_document(&env, &user, &kept, "b.pdf", "file-kept", "vs_kept", true).await,
    );

    delete_chat(&env, &user, &doomed).await;
    drain_cleanup(&env).await;

    let removed = deletes(&env);
    assert!(
        removed.contains(&file_path("file-doomed"))
            && removed.contains(&vector_store_path("vs_doomed"))
    );
    assert!(
        removed.iter().all(|path| !path.contains("kept")),
        "the other chat's provider resources stay: {removed:?}"
    );
    assert!(row(&env, kept_doc).await.cleanup_status.is_none());
    assert_eq!(store_rows(&env, &kept).await.len(), 1);
}

#[tokio::test]
async fn a_chat_without_provider_resources_is_acknowledged_without_provider_calls() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    delete_chat(&env, &user, &chat).await;
    drain_cleanup(&env).await;

    assert_eq!(env.outbox_outcomes(CHAT_CLEANUP_QUEUE).await, vec!["ok"]);
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn provider_404s_count_as_success_for_files_and_the_vector_store() {
    let (env, user, chat, id) = chat_with_document().await;
    fail_file_deletes(&env, "file-doc1", 404, 1);
    fail_store_deletes(&env, "vs_chat1", 404, 1);

    delete_chat(&env, &user, &chat).await;
    drain_cleanup(&env).await;

    assert_eq!(env.outbox_outcomes(CHAT_CLEANUP_QUEUE).await, vec!["ok"]);
    assert_eq!(row(&env, id).await.cleanup_status.as_deref(), Some("done"));
    assert!(store_rows(&env, &chat).await.is_empty());
}

#[tokio::test]
async fn the_vector_store_is_kept_while_an_attachment_cleanup_is_pending() {
    let env = env_with_max_attempts(3).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let good = detail_id(
        &upload_ready_document(&env, &user, &chat, "a.pdf", "file-a", "vs_chat1", true).await,
    );
    let bad = detail_id(
        &upload_ready_document(&env, &user, &chat, "b.pdf", "file-b", "vs_chat1", false).await,
    );
    fail_file_deletes(&env, "file-b", 500, 5);

    delete_chat(&env, &user, &chat).await;

    let first = wait_row(&env, bad, "the first failed attempt", |row| {
        row.cleanup_attempts >= 1
    })
    .await;
    assert_eq!(first.cleanup_status.as_deref(), Some("pending"));
    assert!(first.last_cleanup_error.is_some());
    assert!(
        calls_to(&env, "DELETE", &vector_store_path("vs_chat1")).is_empty(),
        "no vector store delete while an attachment is pending"
    );
    assert_eq!(store_rows(&env, &chat).await.len(), 1);
    assert_eq!(wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 1).await[0], "retry");

    // The third failed attempt reaches cleanup_worker.max_attempts: the attachment is terminally
    // failed, the store is deleted anyway and the message completes.
    assert_eq!(
        wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 3).await,
        vec!["retry", "retry", "ok"]
    );
    wait_calls(&env, "DELETE", &vector_store_path("vs_chat1"), 1).await;
    let failed = row(&env, bad).await;
    assert_eq!(failed.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(failed.cleanup_attempts, 3);
    assert!(failed.last_cleanup_error.is_some());
    assert_eq!(
        row(&env, good).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(
        calls_to(&env, "DELETE", &file_path("file-a")).len(),
        1,
        "a finished attachment is not deleted again"
    );
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-b")).len(), 3);
    let store_at = position_of(&env, "DELETE", &vector_store_path("vs_chat1")).unwrap();
    let last_file_b = call_lines(&env)
        .iter()
        .rposition(|line| *line == format!("DELETE {}", file_path("file-b")))
        .unwrap();
    assert!(
        last_file_b < store_at,
        "the store is deleted after every attachment is terminal"
    );
    assert!(store_rows(&env, &chat).await.is_empty());
}

#[tokio::test]
async fn a_failing_vector_store_delete_keeps_the_row_and_dead_letters_at_the_attempt_limit() {
    let env = env_with_max_attempts(2).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let id = detail_id(
        &upload_ready_document(&env, &user, &chat, "a.pdf", "file-a", "vs_chat1", true).await,
    );
    fail_store_deletes(&env, "vs_chat1", 500, 5);

    delete_chat(&env, &user, &chat).await;

    assert_eq!(
        wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 2).await,
        vec!["retry", "reject"],
        "every delivery counts toward the limit"
    );
    assert_eq!(row(&env, id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(calls_to(&env, "DELETE", &file_path("file-a")).len(), 1);
    assert_eq!(
        calls_to(&env, "DELETE", &vector_store_path("vs_chat1")).len(),
        2
    );
    assert_eq!(
        store_rows(&env, &chat).await.len(),
        1,
        "the row is kept, so a dead-letter replay retries the delete"
    );
    let dead = billing::dead_letters(&env, CHAT_CLEANUP_QUEUE).await;
    assert_eq!(dead.len(), 1);
    assert!(
        dead[0]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("max attempts"),
        "{:?}",
        dead[0].last_error
    );
    let payload: Value = serde_json::from_slice(&dead[0].payload).unwrap();
    assert_eq!(payload["chat_id"], json!(chat));
}

#[tokio::test]
async fn finished_failed_and_fileless_attachments_are_not_deleted_again() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let done = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-done")
        .deleted()
        .cleanup("done")
        .insert(&env)
        .await;
    let failed = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-failed")
        .deleted()
        .cleanup("failed")
        .cleanup_attempts(5)
        .insert(&env)
        .await;
    let fileless = RowSeed::new(&user, &chat)
        .status("uploaded")
        .insert(&env)
        .await;
    let live = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-live")
        .insert(&env)
        .await;
    seed_store(&env, &user, &chat, Some("vs_seeded")).await;

    delete_chat(&env, &user, &chat).await;
    drain_cleanup(&env).await;

    assert_eq!(env.outbox_outcomes(CHAT_CLEANUP_QUEUE).await, vec!["ok"]);
    assert_eq!(
        deletes(&env),
        vec![file_path("file-live"), vector_store_path("vs_seeded")],
        "only the live attachment's file, then the store (a failed row does not block it)"
    );
    assert_eq!(
        row(&env, done).await.cleanup_status.as_deref(),
        Some("done")
    );
    let kept_failed = row(&env, failed).await;
    assert_eq!(kept_failed.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(
        kept_failed.cleanup_attempts, 5,
        "a terminal row is left alone"
    );
    assert_eq!(
        row(&env, fileless).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(
        row(&env, live).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert!(store_rows(&env, &chat).await.is_empty());
}

#[tokio::test]
async fn a_chat_cleanup_message_for_an_active_chat_is_rejected() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let attachment = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-live")
        .insert(&env)
        .await;
    seed_store(&env, &user, &chat, Some("vs_live")).await;

    env.enqueue_raw_outbox(
        CHAT_CLEANUP_QUEUE,
        chat.parse().unwrap(),
        &chat_cleanup_message(&user, &chat),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 1).await,
        vec!["reject"]
    );
    let dead = billing::dead_letters(&env, CHAT_CLEANUP_QUEUE).await;
    assert_eq!(dead.len(), 1);
    assert!(
        dead[0]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("chat is not soft-deleted"),
        "{:?}",
        dead[0].last_error
    );
    assert!(
        calls(&env).is_empty(),
        "an active chat's resources are never touched"
    );
    assert!(row(&env, attachment).await.cleanup_status.is_none());
    assert_eq!(store_rows(&env, &chat).await.len(), 1);
}

#[tokio::test]
async fn a_chat_cleanup_message_for_an_unknown_chat_is_rejected() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let unknown = Uuid::new_v4().to_string();

    env.enqueue_raw_outbox(
        CHAT_CLEANUP_QUEUE,
        unknown.parse().unwrap(),
        &chat_cleanup_message(&user, &unknown),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 1).await,
        vec!["reject"]
    );
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn replaying_a_finished_chat_cleanup_is_a_no_op() {
    let (env, user, chat, _id) = chat_with_document().await;
    delete_chat(&env, &user, &chat).await;
    drain_cleanup(&env).await;
    let before = call_lines(&env);

    env.enqueue_raw_outbox(
        CHAT_CLEANUP_QUEUE,
        chat.parse().unwrap(),
        &chat_cleanup_message(&user, &chat),
    )
    .await;

    assert_eq!(
        wait_outcomes(&env, CHAT_CLEANUP_QUEUE, 2).await,
        vec!["ok", "ok"]
    );
    settle(100).await;
    assert_eq!(
        call_lines(&env),
        before,
        "idempotent: nothing is left to delete"
    );
}

#[tokio::test]
async fn an_attachment_deleted_before_its_chat_is_not_deleted_twice() {
    let (env, user, chat, id) = chat_with_document().await;
    delete_ok(&env, &user, &chat, id).await;
    wait_queue_handled(&env, ATTACHMENT_CLEANUP_QUEUE).await;

    delete_chat(&env, &user, &chat).await;
    drain_cleanup(&env).await;

    assert_eq!(
        deletes(&env),
        vec![file_path("file-doc1"), vector_store_path("vs_chat1")],
        "the file was deleted by the attachment cleanup, the store by the chat cleanup"
    );
}

#[tokio::test]
async fn a_repeated_delete_of_an_attachment_in_a_deleted_chat_is_not_found() {
    let (env, user, chat, id) = chat_with_document().await;
    delete_chat(&env, &user, &chat).await;
    drain_cleanup(&env).await;

    let resp = delete_attachment(&env, &user, &chat, &id.to_string()).await;

    assert_eq!(resp.status.as_u16(), 404, "the chat is gone for the caller");
}
