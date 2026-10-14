//! The attachment cleanup handler invoked directly with an outbox message over in-memory `SQLite`,
//! the real OAGW storage client and the mock gateway: the delete flow end to end (a `DELETE`
//! enqueues, the handler deletes the provider file, the row becomes `done`, no Vector Stores
//! call), 2xx and 404 as success, the retry budget with the attempts on the row, `failed` plus
//! `Reject` at the fifth failure, a database error without an attempt increment, a malformed
//! payload, a soft-deleted or missing parent chat, the secondary copy, tenant isolation, the
//! request the gateway sees (URI, S2S context, no credential) and the dead-letter replay of a
//! `failed` row.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler` (`inst-ch-*`),
//! state `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`, flow
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability` (`inst-adm-inspect`,
//! `inst-adm-replay`), flow `cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment`
//! (the provider file is deleted only by the handler); definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-delete-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use toolkit_db::outbox::LeasedMessageHandler;
use uuid::Uuid;

use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, AZURE_API_VERSION, AZURE_BACKEND, AZURE_RAG_ALIAS, AttachRig, OPENAI_ALIAS,
    OPENAI_BACKEND, RowSeed, cleanup_message, deleted_payload, outcome_of, raw_cleanup_message,
    rig, seed_row, soft_delete_chat,
};
use crate::test_support::chats::{at, fail_attachment_updates};
use crate::test_support::send::provider_s2s;
use crate::test_support::storage_mock::{Answer, Route};
use crate::test_support::{TENANT_A, TENANT_B};

const FILE: &str = "file-del-1";

async fn pending_row(rig: &AttachRig, chat: &ChatRecord, seed: RowSeed) -> Uuid {
    seed_row(&rig.db, chat, seed.cleanup("pending").file_id(FILE)).await
}

async fn deliver(rig: &AttachRig, payload: &Value, attempts: i16) -> &'static str {
    let cleanup = rig.cleanup();
    outcome_of(
        &cleanup
            .handler
            .handle(&cleanup_message(payload, attempts))
            .await,
    )
}

async fn row(
    rig: &AttachRig,
    chat: &ChatRecord,
    id: Uuid,
) -> crate::infra::storage::entity::attachment::Model {
    rig.row(chat.id, id).await
}

fn with_secondary(mut payload: Value, file: &str) -> Value {
    payload["secondary_ref"] = json!({
        "file_id": file, "provider_kind": "anthropic", "upstream_alias": ANTHROPIC_ALIAS
    });
    payload
}

// ── the delete flow end to end ───────────────────────────────────────────────

#[tokio::test]
async fn a_delete_then_the_handler_removes_the_provider_file_and_completes_the_cleanup() {
    // dod-delete-api: the provider file is deleted only by the cleanup handler; no Vector Stores
    // API call is made; the row becomes `done`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().file_id(FILE)).await;
    assert_eq!(
        rig.delete_attachment(chat.id, id).await.status.as_u16(),
        204
    );
    assert_eq!(
        rig.storage.call_count(),
        0,
        "the request made no provider call"
    );
    let messages = rig.attachment_cleanup_rows().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("pending")
    );

    let cleanup = rig.cleanup();
    let result = cleanup
        .handler
        .handle(&cleanup_message(&messages[0].payload, 0))
        .await;
    assert_eq!(outcome_of(&result), "ok");
    let deletes = rig.storage.calls_of(Route::FilesDelete);
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].resource_id.as_deref(), Some(FILE));
    assert_eq!(rig.storage.count_of(Route::VsDelete), 0);
    assert_eq!(rig.storage.count_of(Route::VsAddFile), 0);
    assert_eq!(rig.storage.count_of(Route::VsCreate), 0);
    let done = row(&rig, &chat, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert!(done.deleted_at.is_some(), "the row stays soft-deleted");
}

// ── the request the gateway sees ─────────────────────────────────────────────

#[tokio::test]
async fn the_delete_goes_to_the_files_route_of_the_storage_provider_under_the_s2s_context() {
    // inst-ch-delete-file: `DELETE /{alias}{prefix}/files/{provider_file_id}` through OAGW under
    // the S2S context, no credential
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    let call = rig.storage.calls_of(Route::FilesDelete).remove(0);
    assert_eq!(call.method, "DELETE");
    assert_eq!(call.uri, format!("/{OPENAI_ALIAS}/v1/files/{FILE}"));
    assert_eq!(call.subject, provider_s2s().subject_id());
    assert_eq!(call.tenant, provider_s2s().subject_tenant_id());
    for name in ["authorization", "api-key", "x-api-key"] {
        assert_eq!(call.header(name), None, "{name}");
    }
}

#[tokio::test]
async fn an_azure_storage_backend_is_deleted_with_its_prefix_and_api_version() {
    // inst-ch-delete-file: the storage dispatcher maps `storage_backend`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document().backend(AZURE_BACKEND)).await;
    let mut payload = deleted_payload(&chat, id, Some(FILE));
    payload["storage_backend"] = json!(AZURE_BACKEND);
    assert_eq!(deliver(&rig, &payload, 0).await, "ok");
    let call = rig.storage.calls_of(Route::FilesDelete).remove(0);
    assert_eq!(
        call.uri,
        format!("/{AZURE_RAG_ALIAS}/openai/files/{FILE}?api-version={AZURE_API_VERSION}")
    );
}

// ── success paths ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_2xx_marks_the_row_done_and_records_the_completion() {
    // inst-ch-mark-done, inst-ch-return-ok, inst-cs-done, inst-met-cleanup
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document().deleted(at(5))).await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    let done = row(&rig, &chat, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert_eq!(done.cleanup_attempts, 0);
    assert!(done.cleanup_updated_at.is_some());
    assert_eq!(rig.metrics.cleanup_completed(), 1);
}

#[tokio::test]
async fn a_provider_404_is_success() {
    // the file is already gone
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    rig.storage.queue(Route::FilesDelete, Answer::status(404));
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(row(&rig, &chat, id).await.cleanup_attempts, 0);
}

#[tokio::test]
async fn a_null_provider_file_id_is_done_without_a_provider_call() {
    // inst-ch-no-file, inst-ch-return-no-file
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("pending")
            .no_provider_file()
            .cleanup("pending"),
    )
    .await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, None), 0).await,
        "ok"
    );
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("done")
    );
}

// ── the retry budget ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_500_records_the_attempt_and_retries() {
    // inst-ch-delete-failed, inst-ch-record-attempt, inst-ch-return-retry, inst-cs-retry
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    rig.storage.queue(Route::FilesDelete, Answer::status(500));
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "retry"
    );
    let after = row(&rig, &chat, id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(after.cleanup_attempts, 1);
    assert!(after.last_cleanup_error.is_some());
    assert_eq!(
        rig.metrics.cleanup_retries(),
        vec!["provider_error".to_owned()]
    );
    let cause = after.last_cleanup_error.unwrap();
    assert!(
        !cause.contains("provider-secret-error-text"),
        "no provider text: {cause}"
    );
    assert!(!cause.contains(FILE), "no file identifier: {cause}");
}

#[tokio::test]
async fn a_gateway_failure_is_a_failed_attempt_too() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    rig.storage.queue(
        Route::FilesDelete,
        Answer::Fail(crate::test_support::provider::GatewayFailure::Unavailable),
    );
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "retry"
    );
    assert_eq!(row(&rig, &chat, id).await.cleanup_attempts, 1);
}

#[tokio::test]
async fn the_fifth_failed_attempt_fails_the_row_and_rejects() {
    // inst-ch-exhausted, inst-cs-failed: the row is terminal unresolved provider-file debt
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    rig.storage
        .set_default(Route::FilesDelete, Answer::status(500));
    let payload = deleted_payload(&chat, id, Some(FILE));
    for attempt in 1..=4 {
        assert_eq!(
            deliver(&rig, &payload, 0).await,
            "retry",
            "attempt {attempt}"
        );
        assert_eq!(row(&rig, &chat, id).await.cleanup_attempts, attempt);
    }
    assert_eq!(deliver(&rig, &payload, 0).await, "reject");
    let failed = row(&rig, &chat, id).await;
    assert_eq!(failed.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(failed.cleanup_attempts, 5);
    assert_eq!(rig.metrics.cleanup_failed(), 1);
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 5);
}

#[tokio::test]
async fn an_unresolvable_storage_backend_is_a_failed_attempt() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    let mut payload = deleted_payload(&chat, id, Some(FILE));
    payload["storage_backend"] = json!("retired-backend");
    assert_eq!(deliver(&rig, &payload, 0).await, "retry");
    assert_eq!(row(&rig, &chat, id).await.cleanup_attempts, 1);
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn the_budget_is_configurable() {
    // `cleanup_worker.max_attempts`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    rig.storage
        .set_default(Route::FilesDelete, Answer::status(500));
    let cleanup = rig.cleanup_with(2, true);
    let message = cleanup_message(&deleted_payload(&chat, id, Some(FILE)), 0);
    assert_eq!(outcome_of(&cleanup.handler.handle(&message).await), "retry");
    assert_eq!(
        outcome_of(&cleanup.handler.handle(&message).await),
        "reject"
    );
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("failed")
    );
}

// ── database errors ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_or_row_read_error_retries_without_a_provider_call_or_an_increment() {
    // inst-ch-db-error, inst-ch-return-db-retry
    for step in ["chat", "row"] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let id = pending_row(&rig, &chat, RowSeed::document()).await;
        let cleanup = rig.cleanup();
        match step {
            "chat" => cleanup.faults.chat_read.store(true, Ordering::SeqCst),
            _ => cleanup.faults.row_read.store(true, Ordering::SeqCst),
        }
        let result = cleanup
            .handler
            .handle(&cleanup_message(&deleted_payload(&chat, id, Some(FILE)), 0))
            .await;
        assert_eq!(outcome_of(&result), "retry", "{step}");
        assert_eq!(rig.storage.call_count(), 0, "{step}");
        let after = row(&rig, &chat, id).await;
        assert_eq!(after.cleanup_attempts, 0, "{step}");
        assert_eq!(after.cleanup_status.as_deref(), Some("pending"), "{step}");
    }
}

#[tokio::test]
async fn a_failing_done_write_retries_and_the_redelivery_completes() {
    // the delete is idempotent: the redelivery repeats it
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    fail_attachment_updates(&rig.db).await;
    let payload = deleted_payload(&chat, id, Some(FILE));
    assert_eq!(deliver(&rig, &payload, 0).await, "retry");
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(row(&rig, &chat, id).await.cleanup_attempts, 0);
    crate::test_support::raw_exec(&rig.db.raw, "DROP TRIGGER fail_attachment_update")
        .await
        .unwrap();
    rig.storage.queue(Route::FilesDelete, Answer::status(404));
    assert_eq!(deliver(&rig, &payload, 1).await, "ok");
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 2);
}

// ── acknowledged without action ──────────────────────────────────────────────

#[tokio::test]
async fn a_malformed_payload_is_rejected_at_once() {
    // inst-ch-malformed
    let rig = rig().await;
    let cleanup = rig.cleanup();
    for bytes in [
        &b"not json"[..],
        b"",
        b"{}",
        b"{\"event_type\":\"attachment_deleted\"}",
    ] {
        let result = cleanup.handler.handle(&raw_cleanup_message(bytes)).await;
        assert_eq!(outcome_of(&result), "reject", "{bytes:?}");
    }
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn a_soft_deleted_parent_chat_is_acknowledged_without_a_provider_call() {
    // inst-ch-chat-deleted: chat cleanup owns the provider files of a deleted chat
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document().deleted(at(5))).await;
    soft_delete_chat(&rig.db, chat.id, at(9)).await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(rig.storage.call_count(), 0);
    let untouched = row(&rig, &chat, id).await;
    assert_eq!(untouched.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(untouched.cleanup_attempts, 0);
}

#[tokio::test]
async fn a_missing_chat_or_row_is_acknowledged() {
    // inst-ch-terminal, the missing chat of the feature-local deviation
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let ghost_chat = ChatRecord {
        id: Uuid::new_v4(),
        ..chat.clone()
    };
    assert_eq!(
        deliver(
            &rig,
            &deleted_payload(&ghost_chat, Uuid::new_v4(), Some(FILE)),
            0
        )
        .await,
        "ok"
    );
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, Uuid::new_v4(), Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn a_done_row_is_acknowledged_without_a_provider_call() {
    // inst-ch-terminal: a redelivery or a replay of a finished message
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("done").attempts(1),
    )
    .await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(row(&rig, &chat, id).await.cleanup_attempts, 1);
}

#[tokio::test]
async fn the_handler_never_touches_a_row_of_another_tenant() {
    // the handler runs outside a user scope and filters every query on `tenant_id`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document()).await;
    let mut payload = deleted_payload(&chat, id, Some(FILE));
    payload["tenant_id"] = json!(TENANT_B);
    assert_eq!(
        deliver(&rig, &payload, 0).await,
        "ok",
        "the row is missing for that tenant"
    );
    assert_eq!(rig.storage.call_count(), 0);
    let untouched = row(&rig, &chat, id).await;
    assert_eq!(untouched.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(untouched.cleanup_attempts, 0);
    assert_eq!(chat.tenant_id, TENANT_A);
}

// ── the abandoned upload and the failed indexing events ──────────────────────

#[tokio::test]
async fn the_abandoned_upload_and_failed_indexing_messages_delete_the_provider_file() {
    // the row is `failed`, not soft-deleted; the message is handled identically
    for event in ["attachment_upload_abandoned", "attachment_indexing_failed"] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let id = pending_row(
            &rig,
            &chat,
            RowSeed::document()
                .status("failed")
                .error("upload_abandoned"),
        )
        .await;
        let mut payload = deleted_payload(&chat, id, Some(FILE));
        payload["event_type"] = json!(event);
        assert_eq!(deliver(&rig, &payload, 0).await, "ok", "{event}");
        assert_eq!(rig.storage.count_of(Route::FilesDelete), 1, "{event}");
        let done = row(&rig, &chat, id).await;
        assert_eq!(done.cleanup_status.as_deref(), Some("done"), "{event}");
        assert_eq!(done.status, "failed", "{event}");
        assert!(
            done.deleted_at.is_none(),
            "{event}: the row is not soft-deleted"
        );
    }
}

// ── the secondary copy ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_secondary_copy_is_deleted_on_the_anthropic_upstream() {
    // inst-ch-secondary, inst-ch-delete-secondary
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::image()).await;
    let payload = with_secondary(deleted_payload(&chat, id, Some(FILE)), "file_ant_3");
    assert_eq!(deliver(&rig, &payload, 0).await, "ok");
    let deletes = rig.storage.calls_of(Route::FilesDelete);
    assert_eq!(deletes.len(), 2);
    assert_eq!(deletes[0].alias, OPENAI_ALIAS, "the primary file first");
    assert_eq!(deletes[1].alias, ANTHROPIC_ALIAS);
    assert_eq!(deletes[1].resource_id.as_deref(), Some("file_ant_3"));
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn a_failing_secondary_delete_does_not_block_the_done_transition() {
    // inst-ch-delete-secondary: logged, not blocking
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::image()).await;
    rig.storage
        .queue_for(ANTHROPIC_ALIAS, Route::FilesDelete, Answer::status(500));
    let payload = with_secondary(deleted_payload(&chat, id, Some(FILE)), "file_ant_3");
    assert_eq!(deliver(&rig, &payload, 0).await, "ok");
    let after = row(&rig, &chat, id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("done"));
    assert_eq!(after.cleanup_attempts, 0);
}

#[tokio::test]
async fn without_an_anthropic_files_client_the_skip_is_counted() {
    // inst-met-cleanup: `mini_chat_secondary_cleanup_skipped{provider_kind}`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::image()).await;
    let cleanup = rig.cleanup_with(5, false);
    let payload = with_secondary(deleted_payload(&chat, id, Some(FILE)), "file_ant_3");
    let result = cleanup.handler.handle(&cleanup_message(&payload, 0)).await;
    assert_eq!(outcome_of(&result), "ok");
    assert_eq!(
        rig.metrics.secondary_skipped(),
        vec!["anthropic".to_owned()]
    );
    assert_eq!(rig.storage.calls_of(Route::FilesDelete).len(), 1);
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("done")
    );
}

// ── the dead-letter replay ───────────────────────────────────────────────────

async fn failed_row(rig: &AttachRig, chat: &ChatRecord) -> Uuid {
    seed_row(
        &rig.db,
        chat,
        RowSeed::document()
            .deleted(at(5))
            .cleanup("failed")
            .attempts(5)
            .last_error("provider answered 500")
            .file_id(FILE),
    )
    .await
}

#[tokio::test]
async fn a_replay_with_a_provider_success_sets_the_row_done() {
    // inst-ch-replay, inst-ch-replay-delete, inst-ch-replay-ok, inst-ch-replay-done,
    // inst-cs-replay-done, inst-adm-replay
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = failed_row(&rig, &chat).await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(
        rig.storage.count_of(Route::FilesDelete),
        1,
        "exactly one attempt"
    );
    let done = row(&rig, &chat, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert_eq!(done.cleanup_attempts, 5);
    assert_eq!(rig.metrics.cleanup_completed(), 1);
}

#[tokio::test]
async fn a_replay_with_a_provider_404_sets_the_row_done() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = failed_row(&rig, &chat).await;
    rig.storage.queue(Route::FilesDelete, Answer::status(404));
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn a_replay_with_a_provider_failure_stays_failed_and_rejects_again() {
    // inst-ch-replay-failed, inst-cs-replay-failed: no loop, no attempt growth
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = failed_row(&rig, &chat).await;
    rig.storage
        .set_default(Route::FilesDelete, Answer::status(500));
    let payload = deleted_payload(&chat, id, Some(FILE));
    for replay in 1..=3 {
        assert_eq!(
            deliver(&rig, &payload, 0).await,
            "reject",
            "replay {replay}"
        );
        let after = row(&rig, &chat, id).await;
        assert_eq!(after.cleanup_status.as_deref(), Some("failed"));
        assert_eq!(
            after.cleanup_attempts, 5,
            "it never grows beyond the budget"
        );
        assert!(after.last_cleanup_error.is_some());
        assert_eq!(
            rig.storage.count_of(Route::FilesDelete),
            usize::try_from(replay).unwrap()
        );
    }
    assert_eq!(
        rig.metrics.cleanup_failed(),
        0,
        "no second `cleanup_failed`"
    );
    assert!(
        rig.metrics.cleanup_retries().is_empty(),
        "no `cleanup_retry`"
    );
}

#[tokio::test]
async fn a_replay_of_a_done_row_or_a_soft_deleted_chat_is_acknowledged_without_a_call() {
    // inst-adm-replay: acked when the row is `done` or the parent chat is soft-deleted
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let done = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("done").file_id(FILE),
    )
    .await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, done, Some(FILE)), 0).await,
        "ok"
    );
    let failed = failed_row(&rig, &chat).await;
    soft_delete_chat(&rig.db, chat.id, at(9)).await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, failed, Some(FILE)), 0).await,
        "ok"
    );
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(
        row(&rig, &chat, failed).await.cleanup_status.as_deref(),
        Some("failed")
    );
}

#[tokio::test]
async fn a_failing_replay_done_write_retries_without_changing_the_attempts() {
    // inst-ch-replay-done: a database error returns `Retry`; the redelivery repeats the delete
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = failed_row(&rig, &chat).await;
    fail_attachment_updates(&rig.db).await;
    assert_eq!(
        deliver(&rig, &deleted_payload(&chat, id, Some(FILE)), 0).await,
        "retry"
    );
    let after = row(&rig, &chat, id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(after.cleanup_attempts, 5);
}

#[tokio::test]
async fn the_attempts_are_preserved_across_the_whole_lifecycle() {
    // pending -> failed through five deliveries, then a replay that succeeds
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = pending_row(&rig, &chat, RowSeed::document().deleted(at(5))).await;
    rig.storage
        .set_default(Route::FilesDelete, Answer::status(500));
    let payload = deleted_payload(&chat, id, Some(FILE));
    for _ in 0..4 {
        assert_eq!(deliver(&rig, &payload, 0).await, "retry");
    }
    assert_eq!(deliver(&rig, &payload, 0).await, "reject");
    rig.storage
        .set_default(Route::FilesDelete, Answer::ok(&json!({ "deleted": true })));
    assert_eq!(deliver(&rig, &payload, 0).await, "ok");
    let done = row(&rig, &chat, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert_eq!(done.cleanup_attempts, 5);
    assert_eq!(rig.metrics.cleanup_failed(), 1);
    assert_eq!(rig.metrics.cleanup_completed(), 1);
}

#[test]
fn the_handler_is_a_leased_outbox_handler() {
    fn assert_leased<T: LeasedMessageHandler>() {}
    assert_leased::<crate::infra::attachment_cleanup::AttachmentCleanupHandler>();
    // the constants of the default storage backends are what the payloads above use
    assert_eq!(OPENAI_BACKEND, "azure_openai");
}
