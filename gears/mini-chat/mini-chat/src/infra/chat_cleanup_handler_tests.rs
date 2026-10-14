//! The chat-cleanup handler in-process over `SQLite`, the real OAGW storage client and the mock
//! gateway, without network: the mapping of the domain outcome to the shared outbox result; the
//! deletion flow end to end (the real DELETE transaction enqueues, the handler deletes every file
//! and then the store, 404 included, the user sees 404 and no provider call happens at request
//! time); the file that fails five times while the others finish; the store that fails on every
//! delivery, is rejected on the fifth and deleted by a replay; a provider that never answers (the 5 s
//! call deadline as a counted attempt, a logged skip of the secondary, a failed store delete); a
//! scripted slow provider that ends a delivery at the 20 s budget with `Retry`; a slow policy
//! snapshot after a primary delete that finished just before 20 s; the secondary Anthropic copy on
//! the alias of the chat model's provider, on the single Anthropic provider after the model left
//! the catalog, and skipped with the metric and a warning when several exist; the Azure storage
//! backend; the retention of every local row; a turn that is running at deletion time; the fixed
//! cause text of a failed row without provider text.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: flow
//! `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup` (`inst-cd-delete`,
//! `inst-cd-deliver`, `inst-cd-handle`, `inst-cd-retry`, `inst-cd-redeliver`, `inst-cd-rejected`,
//! `inst-cd-dead-letter`, `inst-cd-user-view`, `inst-cd-return`), flow
//! `cpt-cf-mini-chat-flow-background-maintenance-admin-observability` (`inst-adm-store-alert`,
//! `inst-adm-store-replay`, `inst-adm-file-debt`, `inst-adm-file-debt-query`,
//! `inst-adm-outbox-surface`), algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring` (`inst-hw-build-cleanup`,
//! `inst-hw-register`, `inst-hw-outcomes`, `inst-hw-attempts`, `inst-hw-no-local-worker`),
//! `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler` (`inst-cc-timer`, `inst-cc-file`, `inst-cc-budget-stop`,
//! `inst-cc-budget-accounting`), `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file` (`inst-af-delete`, `inst-af-secondary`,
//! `inst-af-failed`, `inst-af-record-attempt`, `inst-af-exhausted`), `cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file`
//! (`inst-sf-snapshot`, `inst-sf-fallback`, `inst-sf-skip`, `inst-sf-return-skip`,
//! `inst-sf-budget-delete`, `inst-sf-delete`), `cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup` (`inst-vs-delete`,
//! `inst-vs-success`, `inst-vs-delete-row`, `inst-vs-failed`, `inst-vs-return-reject`),
//! `cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries` (`inst-cb-deleted`, `inst-cb-transfer`) and `cpt-cf-mini-chat-algo-background-maintenance-retention`
//! (`inst-rt-no-purge`, `inst-rt-target`, `inst-rt-purged`, `inst-rt-running-turn`,
//! `inst-rt-no-audit`, `inst-rt-content`, `inst-rt-return`); state
//! `cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge` (`inst-pp-files-retry`,
//! `inst-pp-files-done`, `inst-pp-files-purged`, `inst-pp-files-debt`, `inst-pp-store-purged`,
//! `inst-pp-store-debt`, `inst-pp-store-retry`, `inst-pp-debt-terminal`), the transition
//! `inst-vsm-chat-cleanup` of
//! `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`; definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`,
//! `cpt-cf-mini-chat-dod-background-maintenance-vector-store-ordering`,
//! `cpt-cf-mini-chat-dod-background-maintenance-cleanup-boundaries`,
//! `cpt-cf-mini-chat-dod-background-maintenance-retention` and
//! `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::{Value, json};
use toolkit_db::outbox::LeasedMessageHandler;
use tracing_test::traced_test;
use uuid::Uuid;

use crate::domain::chats::ChatStore;
use crate::domain::maintenance::HandlerOutcome;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{
    RowSeed, assert_no_provider_text, seed_row, soft_delete_chat,
};
use crate::test_support::chats::{a1, at, attachment_rows, owner_scope};
use crate::test_support::maintenance::{
    ANTHROPIC_ALIAS, AZURE_ALIAS, AZURE_RESP_ALIAS, MaintOptions, MaintRig, PolicyMode,
    ScriptedPolicy, SlowGateway, catalog, catalog_without, chat_cleanup_payload, cleanup_message,
    cleanup_payload_of, maint_rig, maint_rig_with, providers_json, registry_of, result_label,
    result_reason, seed_range, seed_store, table_count,
};
use crate::test_support::send::{ANTHROPIC_MODEL, SendRigOptions, send_rig_with};
use crate::test_support::storage_mock::{Answer, Route};
use crate::test_support::{TENANT_A, quota};

const REASON: &str = "vector store delete: max attempts (5) reached";

async fn deleted_chat(rig: &MaintRig, model: &str) -> ChatRecord {
    let chat = rig.chat(model).await;
    soft_delete_chat(rig.db(), chat.id, at(50)).await;
    chat
}

fn pending(n: i64, file: &str) -> RowSeed {
    RowSeed::document()
        .cleanup("pending")
        .file_id(file)
        .created(at(n))
}

async fn status_of(rig: &MaintRig, chat: &ChatRecord) -> Vec<(String, i32)> {
    let mut rows = attachment_rows(rig.db(), chat.id).await;
    rows.sort_by_key(|row| (row.created_at, row.id));
    rows.into_iter()
        .map(|row| (row.cleanup_status.unwrap_or_default(), row.cleanup_attempts))
        .collect()
}

fn files(rig: &MaintRig) -> Vec<String> {
    rig.storage
        .calls_of(Route::FilesDelete)
        .into_iter()
        .map(|call| call.resource_id.unwrap_or_default())
        .collect()
}

async fn deliver(rig: &MaintRig, chat: &ChatRecord, attempts: i16) -> &'static str {
    let handler = rig.cleanup_handler(5, true);
    result_label(
        &handler
            .handle(&cleanup_message(&cleanup_payload_of(chat), attempts))
            .await,
    )
}

// ── the outbox handler ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_handler_maps_the_outcomes_to_the_shared_outbox_results() {
    // inst-hw-outcomes, inst-hw-attempts: Ok, Retry and Reject(reason), `attempts` 0-based
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    rig.storage
        .set_default(Route::VsDelete, Answer::status(500));
    let handler = rig.cleanup_handler(5, true);
    let message = |attempts| cleanup_message(&cleanup_payload_of(&chat), attempts);
    assert_eq!(result_label(&handler.handle(&message(0)).await), "retry");
    let rejected = handler.handle(&message(4)).await;
    assert_eq!(result_label(&rejected), "reject");
    assert_eq!(result_reason(&rejected).as_deref(), Some(REASON));
    rig.storage
        .set_default(Route::VsDelete, Answer::ok(&json!({ "deleted": true })));
    assert_eq!(result_label(&handler.handle(&message(0)).await), "ok");
}

#[tokio::test]
async fn a_malformed_message_and_an_active_chat_are_rejected_without_a_provider_call() {
    // inst-hw-outcomes, inst-cb-active: the reason holds no identifier
    let rig = maint_rig().await;
    let handler = rig.cleanup_handler(5, true);
    let malformed =
        crate::test_support::maintenance::message_of("mini_chat.chat_cleanup", b"[]", 0);
    assert_eq!(result_label(&handler.handle(&malformed).await), "reject");
    let active = rig.chat(quota::STANDARD).await;
    seed_row(rig.db(), &active, pending(1, "file-1")).await;
    let result = handler
        .handle(&cleanup_message(&cleanup_payload_of(&active), 0))
        .await;
    assert_eq!(result_label(&result), "reject");
    let reason = result_reason(&result).unwrap();
    assert!(!reason.contains(&active.id.to_string()), "{reason}");
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(
        status_of(&rig, &active).await,
        vec![("pending".to_owned(), 0)]
    );
}

// ── the deletion flow end to end ─────────────────────────────────────────────

#[tokio::test]
async fn the_delete_transaction_enqueues_and_the_handler_deletes_every_file_then_the_store() {
    // inst-cd-delete, inst-cd-deliver, inst-cd-handle, inst-cd-return, inst-af-delete,
    // inst-vs-delete, inst-vs-delete-row, inst-vsm-chat-cleanup, inst-pp-files-purged
    let rig = maint_rig().await;
    let chat = rig.chat(quota::STANDARD).await;
    seed_row(rig.db(), &chat, pending(1, "file-doc")).await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::image()
            .cleanup("pending")
            .file_id("file-img")
            .created(at(2)),
    )
    .await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .deleted(at(3))
            .cleanup("pending")
            .file_id("file-soft-deleted")
            .created(at(3)),
    )
    .await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .no_provider_file()
            .created(at(4)),
    )
    .await;
    seed_store(rig.db(), &chat, Some("vs_chat_1")).await;
    // the DELETE transaction: soft-delete, mark the attachments, enqueue; no provider call
    // (the rows were seeded `pending` as the transaction leaves them)
    rig.db()
        .provider
        .delete_chat(&owner_scope(&a1()), chat.id, rig.quota.enqueuer.clone())
        .await
        .expect("204");
    assert_eq!(
        rig.storage.call_count(),
        0,
        "no provider call at request time"
    );
    let rows = rig
        .rows_of(&rig.quota.outbox.chat_cleanup_queue_name.clone())
        .await;
    assert_eq!(rows.len(), 1);
    let message = crate::test_support::maintenance::message_of(
        "mini_chat.chat_cleanup",
        &serde_json::to_vec(&rows[0].payload).unwrap(),
        0,
    );
    let handler = rig.cleanup_handler(5, true);
    assert_eq!(result_label(&handler.handle(&message).await), "ok");
    // the files first, then the store
    let order: Vec<(String, Option<String>)> = rig
        .storage
        .calls()
        .into_iter()
        .map(|call| (call.method, call.resource_id))
        .collect();
    assert_eq!(order.len(), 4, "{order:?}");
    assert!(order[..3].iter().all(|(method, _)| method == "DELETE"));
    assert_eq!(
        order[3],
        ("DELETE".to_owned(), Some("vs_chat_1".to_owned()))
    );
    assert_eq!(
        files(&rig)[..3],
        ["file-doc", "file-img", "file-soft-deleted"].map(str::to_owned)
    );
    let store_call = rig.storage.calls_of(Route::VsDelete).remove(0);
    assert_eq!(
        store_call.uri,
        format!("/{AZURE_ALIAS}/v1/vector_stores/vs_chat_1")
    );
    for call in rig.storage.calls() {
        assert_eq!(
            call.subject,
            crate::test_support::send::provider_s2s().subject_id()
        );
        assert_eq!(
            call.tenant,
            crate::test_support::send::provider_s2s().subject_tenant_id()
        );
        for name in ["authorization", "api-key", "x-api-key"] {
            assert_eq!(call.header(name), None, "{name}");
        }
    }
    assert!(
        status_of(&rig, &chat)
            .await
            .iter()
            .all(|(status, _)| status == "done")
    );
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
    assert_eq!(
        rig.recorder
            .completed()
            .iter()
            .filter(|r| *r == "file")
            .count(),
        4
    );
    assert_eq!(
        rig.recorder
            .completed()
            .iter()
            .filter(|r| *r == "vector_store")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_404_of_a_file_and_of_the_store_counts_as_success() {
    // inst-af-success, inst-vs-success: already gone
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_row(rig.db(), &chat, pending(1, "file-1")).await;
    seed_row(rig.db(), &chat, pending(2, "file-2")).await;
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    rig.storage.queue(Route::FilesDelete, Answer::status(404));
    rig.storage.queue(Route::FilesDelete, Answer::status(404));
    rig.storage.queue(Route::VsDelete, Answer::status(404));
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert!(
        status_of(&rig, &chat)
            .await
            .iter()
            .all(|(status, attempts)| status == "done" && *attempts == 0)
    );
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
    assert!(rig.recorder.retries().is_empty());
}

#[tokio::test]
async fn an_azure_storage_backend_is_deleted_with_its_prefix_and_api_version() {
    // inst-af-delete: the storage dispatcher maps the row's `storage_backend`
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-az").backend("azure-resp-label"),
    )
    .await;
    crate::test_support::attachments::seed_vector_store(
        rig.db(),
        &chat,
        Some("vs_az"),
        "azure-resp-label",
        at(5),
    )
    .await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(
        rig.storage.calls_of(Route::FilesDelete)[0].uri,
        format!("/{AZURE_RESP_ALIAS}/openai/files/file-az?api-version=2025-03-01-preview")
    );
    assert_eq!(
        rig.storage.calls_of(Route::VsDelete)[0].uri,
        format!("/{AZURE_RESP_ALIAS}/openai/vector_stores/vs_az?api-version=2025-03-01-preview")
    );
}

#[tokio::test]
async fn after_the_delete_the_chat_and_its_sub_resources_are_404_and_no_endpoint_shows_progress() {
    // inst-cd-user-view: the user keeps observing only the deletion
    let rig = send_rig_with(SendRigOptions::new()).await;
    let chat = rig.std_chat().await;
    rig.quota
        .db
        .provider
        .delete_chat(&owner_scope(&a1()), chat.id, rig.quota.enqueuer.clone())
        .await
        .unwrap();
    for uri in [
        format!("/mini-chat/v1/chats/{}", chat.id),
        format!("/mini-chat/v1/chats/{}/messages", chat.id),
        format!("/mini-chat/v1/chats/{}/turns/{}", chat.id, Uuid::new_v4()),
        format!("/mini-chat/v1/chats/{}/cleanup", chat.id),
        format!("/mini-chat/v1/chats/{}/cleanup-status", chat.id),
    ] {
        let reply = rig.get(&uri, &a1()).await;
        assert_eq!(reply.status.as_u16(), 404, "{uri}");
    }
}

// ── failure and retry ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_file_failing_five_times_becomes_failed_while_the_others_finish_and_the_store_is_deleted()
{
    // inst-af-failed, inst-af-record-attempt, inst-af-exhausted, inst-vs-record-debt,
    // inst-pp-files-retry, inst-pp-store-debt, inst-adm-file-debt, inst-adm-file-debt-query
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_row(rig.db(), &chat, pending(1, "file-bad")).await;
    seed_row(rig.db(), &chat, pending(2, "file-good")).await;
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    // delivery 1: the first file fails, the second succeeds; deliveries 2 to 5: only the first
    rig.storage.queue(Route::FilesDelete, Answer::status(500));
    rig.storage
        .queue(Route::FilesDelete, Answer::ok(&json!({ "deleted": true })));
    for _ in 0..4 {
        rig.storage.queue(Route::FilesDelete, Answer::status(503));
    }
    for attempts in 0..4 {
        assert_eq!(
            deliver(&rig, &chat, attempts).await,
            "retry",
            "delivery {}",
            attempts + 1
        );
        assert!(
            rig.storage.calls_of(Route::VsDelete).is_empty(),
            "the store waits for the row"
        );
    }
    assert_eq!(
        status_of(&rig, &chat).await,
        vec![("pending".to_owned(), 4), ("done".to_owned(), 0)]
    );
    assert_eq!(deliver(&rig, &chat, 4).await, "ok");
    assert_eq!(
        status_of(&rig, &chat).await,
        vec![("failed".to_owned(), 5), ("done".to_owned(), 0)]
    );
    assert_eq!(
        rig.storage.calls_of(Route::VsDelete).len(),
        1,
        "the store is still deleted"
    );
    assert_eq!(rig.recorder.debts(), 1);
    assert_eq!(rig.recorder.failed(), vec!["file"]);
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
    // the operator sees a fixed cause, no file content and no provider text
    let failed = attachment_rows(rig.db(), chat.id)
        .await
        .into_iter()
        .find(|r| r.cleanup_status.as_deref() == Some("failed"))
        .unwrap();
    let cause = failed.last_cleanup_error.expect("a cause");
    assert_no_provider_text(&cause);
    assert!(
        !cause.contains("file-bad") && !cause.contains(AZURE_ALIAS),
        "{cause}"
    );
    // the debt is terminal: a replay re-attempts nothing
    let calls = rig.storage.call_count();
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(rig.storage.call_count(), calls);
    assert_eq!(status_of(&rig, &chat).await[0], ("failed".to_owned(), 5));
}

#[tokio::test]
async fn a_store_failing_on_every_delivery_is_rejected_on_the_fifth_and_deleted_by_a_replay() {
    // inst-vs-failed, inst-vs-keep-row, inst-vs-return-reject, inst-cd-retry, inst-cd-redeliver,
    // inst-cd-rejected, inst-cd-dead-letter, inst-pp-store-retry, inst-adm-store-alert,
    // inst-adm-store-replay
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_store(rig.db(), &chat, Some("vs_stuck")).await;
    rig.storage
        .set_default(Route::VsDelete, Answer::status(500));
    for attempts in 0..4 {
        assert_eq!(deliver(&rig, &chat, attempts).await, "retry");
        assert_eq!(
            table_count(rig.db(), "chat_vector_stores").await,
            1,
            "the row is kept"
        );
    }
    let handler = rig.cleanup_handler(5, true);
    let result = handler
        .handle(&cleanup_message(&cleanup_payload_of(&chat), 4))
        .await;
    assert_eq!(result_label(&result), "reject");
    assert_eq!(result_reason(&result).as_deref(), Some(REASON));
    assert_eq!(
        table_count(rig.db(), "chat_vector_stores").await,
        1,
        "a kept row marks the replay"
    );
    assert_eq!(rig.recorder.failed(), vec!["vector_store"]);
    assert_eq!(
        rig.recorder
            .retries()
            .iter()
            .filter(|(r, _)| r == "vector_store")
            .count(),
        5
    );
    // the operator fixes the cause and replays the dead letter: one delete, then done
    rig.storage
        .set_default(Route::VsDelete, Answer::ok(&json!({ "deleted": true })));
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
    assert_eq!(rig.storage.calls_of(Route::VsDelete).len(), 6);
}

#[tokio::test]
async fn a_placeholder_row_is_deleted_without_a_provider_call() {
    // inst-vs-placeholder: nothing exists at the provider
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_store(rig.db(), &chat, None).await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
}

#[tokio::test]
async fn a_chat_without_a_store_is_purged_once_every_row_is_terminal() {
    // inst-pp-files-purged, inst-vs-none: idempotent under redelivery
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_row(rig.db(), &chat, pending(1, "file-1")).await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    let calls = rig.storage.call_count();
    for attempts in 1..3 {
        assert_eq!(deliver(&rig, &chat, attempts).await, "ok");
    }
    assert_eq!(rig.storage.call_count(), calls, "no duplicate effect");
}

// ── the 5 s call deadline ────────────────────────────────────────────────────

#[tokio::test]
async fn a_file_delete_that_never_answers_fails_at_the_five_second_deadline_as_a_counted_attempt() {
    // inst-af-delete, inst-af-failed: the call that hits the deadline is a failed call
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_row(rig.db(), &chat, pending(1, "file-hang")).await;
    rig.storage.queue(Route::FilesDelete, Answer::Pending);
    assert_eq!(deliver(&rig, &chat, 0).await, "retry");
    assert_eq!(rig.time.elapsed(), Duration::from_secs(5));
    assert_eq!(
        status_of(&rig, &chat).await,
        vec![("pending".to_owned(), 1)]
    );
    let row = attachment_rows(rig.db(), chat.id).await.remove(0);
    assert_eq!(
        row.last_cleanup_error.as_deref(),
        Some("the provider call exceeded its deadline")
    );
}

#[traced_test]
#[tokio::test]
async fn a_secondary_delete_that_never_answers_is_a_logged_skip_and_the_primary_row_is_done() {
    // inst-sf-delete: the 5 s deadline, not retried, never fails the primary row
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-1").secondary("file_ant_hang", "uploaded"),
    )
    .await;
    rig.storage
        .queue_for(ANTHROPIC_ALIAS, Route::FilesDelete, Answer::Pending);
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(status_of(&rig, &chat).await, vec![("done".to_owned(), 0)]);
    let secondary: Vec<_> = rig
        .storage
        .calls_of(Route::FilesDelete)
        .into_iter()
        .filter(|call| call.alias == ANTHROPIC_ALIAS)
        .collect();
    assert_eq!(secondary.len(), 1, "not retried");
    assert!(!logs_contain("file_ant_hang"), "the file id is not logged");
}

#[tokio::test]
async fn a_store_delete_that_never_answers_is_a_failed_delete() {
    // inst-vs-delete, inst-vs-failed: the row is kept, the message retried
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    seed_store(rig.db(), &chat, Some("vs_hang")).await;
    rig.storage.queue(Route::VsDelete, Answer::Pending);
    assert_eq!(deliver(&rig, &chat, 0).await, "retry");
    assert_eq!(rig.time.elapsed(), Duration::from_secs(5));
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 1);
    assert_eq!(
        rig.recorder.retries(),
        vec![(
            "vector_store".to_owned(),
            "vector_store_delete_failed".to_owned()
        )]
    );
}

// ── the 20 s delivery budget ─────────────────────────────────────────────────

#[tokio::test]
async fn a_slow_provider_ends_the_delivery_at_the_budget_with_retry_and_the_next_one_continues() {
    // inst-cc-timer, inst-cc-budget-stop, inst-cc-budget-accounting, inst-pp-files-retry: six
    // files of 4.9 s each; rows not attempted keep their attempts; the store is not called while a
    // row is pending
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    for n in 1..=6 {
        seed_row(rig.db(), &chat, pending(n, &format!("file-{n}"))).await;
    }
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    let gateway = SlowGateway::new(&rig.storage, &rig.time);
    gateway.always(Duration::from_millis(4900));
    let worker = rig.cleanup_worker_over(gateway.clone(), rig.quota.gateway.clone(), 5, true);
    let payload = serde_json::to_vec(&cleanup_payload_of(&chat)).unwrap();
    assert_eq!(worker.handle(&payload, 0).await, HandlerOutcome::Retry);
    assert_eq!(
        files(&rig).len(),
        5,
        "the sixth file would start after 20 s"
    );
    let statuses = status_of(&rig, &chat).await;
    assert_eq!(
        statuses[5],
        ("pending".to_owned(), 0),
        "no attempt for a row that was not attempted"
    );
    assert!(rig.storage.calls_of(Route::VsDelete).is_empty());
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 1);
    // the next delivery continues from the persisted state
    assert_eq!(worker.handle(&payload, 1).await, HandlerOutcome::Ok);
    assert_eq!(files(&rig).len(), 6, "every file was deleted once");
    assert_eq!(rig.storage.calls_of(Route::VsDelete).len(), 1);
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
}

#[tokio::test]
async fn a_budget_driven_retry_is_never_a_reject_for_the_store() {
    // inst-vs-return-budget: every row finishes late, the store call would start after 20 s
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    for n in 1..=5 {
        seed_row(rig.db(), &chat, pending(n, &format!("file-{n}"))).await;
    }
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    let gateway = SlowGateway::new(&rig.storage, &rig.time);
    gateway.always(Duration::from_millis(4900));
    let worker = rig.cleanup_worker_over(gateway, rig.quota.gateway.clone(), 5, true);
    let payload = serde_json::to_vec(&cleanup_payload_of(&chat)).unwrap();
    // the delivery that would reach the limit: still a Retry, no store call, no debt, no failure
    assert_eq!(worker.handle(&payload, 4).await, HandlerOutcome::Retry);
    assert!(rig.storage.calls_of(Route::VsDelete).is_empty());
    assert!(rig.recorder.failed().is_empty());
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 1);
}

#[tokio::test]
async fn a_primary_that_finishes_just_before_the_budget_with_a_slow_snapshot_starts_no_secondary() {
    // inst-sf-budget-delete, inst-cc-budget-accounting: 19.6 s into the delivery the snapshot read
    // uses its 5 s deadline; no secondary delete starts, the row stays `pending`, no attempt
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    for n in 1..=3 {
        seed_row(rig.db(), &chat, pending(n, &format!("file-{n}"))).await;
    }
    seed_row(
        rig.db(),
        &chat,
        pending(4, "file-4").secondary("file_ant_4", "uploaded"),
    )
    .await;
    let gateway = SlowGateway::new(&rig.storage, &rig.time);
    gateway.always(Duration::from_millis(4900));
    let policy = ScriptedPolicy::new(&rig.time, catalog(), PolicyMode::Hang);
    let worker = rig.cleanup_worker_over(gateway, policy.clone(), 5, true);
    let payload = serde_json::to_vec(&cleanup_payload_of(&chat)).unwrap();
    assert_eq!(worker.handle(&payload, 0).await, HandlerOutcome::Retry);
    assert_eq!(policy.call_count(), 1);
    let secondary_calls = rig
        .storage
        .calls_of(Route::FilesDelete)
        .into_iter()
        .filter(|call| call.alias == ANTHROPIC_ALIAS)
        .count();
    assert_eq!(
        secondary_calls, 0,
        "no secondary delete starts after the budget"
    );
    assert_eq!(status_of(&rig, &chat).await[3], ("pending".to_owned(), 0));
}

// ── the secondary Anthropic copy ─────────────────────────────────────────────

fn anthropic_calls(rig: &MaintRig) -> Vec<crate::test_support::storage_mock::Recorded> {
    rig.storage
        .calls_of(Route::FilesDelete)
        .into_iter()
        .filter(|call| call.alias != AZURE_ALIAS)
        .collect()
}

#[tokio::test]
async fn the_secondary_copy_is_deleted_on_the_alias_of_the_chat_model_provider() {
    // inst-af-secondary, inst-sf-delete, inst-sf-snapshot: model `anthropic-1`, provider `anthropic`
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-1").secondary("file_ant_1", "uploaded"),
    )
    .await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    let calls = anthropic_calls(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].uri,
        format!("/{ANTHROPIC_ALIAS}/v1/files/file_ant_1")
    );
    assert_eq!(status_of(&rig, &chat).await, vec![("done".to_owned(), 0)]);
    assert!(rig.recorder.skipped().is_empty());
}

#[tokio::test]
async fn the_secondary_copy_uses_the_single_anthropic_provider_after_the_model_left_the_catalog() {
    // inst-sf-fallback
    let rig =
        maint_rig_with(MaintOptions::new().with_snapshot(catalog_without(ANTHROPIC_MODEL))).await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-1").secondary("file_ant_1", "uploaded"),
    )
    .await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(anthropic_calls(&rig)[0].alias, ANTHROPIC_ALIAS);
}

#[tokio::test]
async fn a_tenant_override_alias_is_used_for_the_secondary_copy() {
    // inst-sf-provider: the provider is resolved for the chat's tenant
    let mut config = providers_json();
    config["providers"]["anthropic"]["tenant_overrides"] =
        json!({ TENANT_A.to_string(): { "host": "tenant-ant.example.com" } });
    let rig = maint_rig_with(MaintOptions::new().with_providers(registry_of(config))).await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-1").secondary("file_ant_1", "uploaded"),
    )
    .await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(anthropic_calls(&rig)[0].alias, "tenant-ant.example.com");
}

#[traced_test]
#[tokio::test]
async fn several_anthropic_providers_skip_the_secondary_with_the_metric_and_a_warning() {
    // inst-sf-skip, inst-sf-return-skip: the model is gone, so there is no safe guess
    let mut config = providers_json();
    config["providers"]["anthropic_eu"] = json!({
        "kind": "anthropic_messages", "host": "anthropic-eu.example.com",
        "storage_kind": "openai", "api_path": "/v1/messages"
    });
    let rig = maint_rig_with(
        MaintOptions::new()
            .with_providers(registry_of(config))
            .with_snapshot(catalog_without(ANTHROPIC_MODEL)),
    )
    .await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    let id = seed_row(
        rig.db(),
        &chat,
        pending(1, "file-1").secondary("file_ant_secret", "uploaded"),
    )
    .await;
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert!(anthropic_calls(&rig).is_empty());
    assert_eq!(rig.recorder.skipped(), vec!["anthropic"]);
    assert_eq!(
        status_of(&rig, &chat).await,
        vec![("done".to_owned(), 0)],
        "never failed because of it"
    );
    assert!(logs_contain(&chat.id.to_string()) && logs_contain(&id.to_string()));
    assert!(!logs_contain("file_ant_secret"));
}

#[tokio::test]
async fn a_failed_secondary_delete_never_fails_the_primary_row() {
    // inst-sf-delete: any other failure is logged and not retried
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, ANTHROPIC_MODEL).await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-1").secondary("file_ant_1", "uploaded"),
    )
    .await;
    rig.storage
        .queue_for(ANTHROPIC_ALIAS, Route::FilesDelete, Answer::status(500));
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    assert_eq!(status_of(&rig, &chat).await, vec![("done".to_owned(), 0)]);
    assert_eq!(anthropic_calls(&rig).len(), 1);
}

// ── retention ────────────────────────────────────────────────────────────────

#[traced_test]
#[tokio::test]
async fn after_the_cleanup_every_local_row_is_still_there_and_only_the_store_row_is_gone() {
    // inst-rt-no-purge, inst-rt-purged, inst-rt-target, inst-rt-no-audit, inst-rt-content: the
    // chat, its messages, turns, reactions, attachments with thumbnails and its summary stay
    let rig = maint_rig().await;
    let chat = rig.chat(quota::STANDARD).await;
    let ids = seed_range(rig.db(), &chat, 4).await;
    crate::test_support::send::seed_summary(
        rig.db(),
        &chat,
        "PRIVATE SUMMARY TEXT",
        9,
        (crate::test_support::send::t(40), ids[3]),
    )
    .await;
    crate::test_support::send::seed_reaction_of(
        rig.db(),
        TENANT_A,
        crate::test_support::USER_A1,
        ids[1],
        "like",
    )
    .await;
    crate::test_support::send::seed_turn_full(
        rig.db(),
        &chat,
        crate::test_support::send::TurnSeedFull::new("completed")
            .request(Uuid::new_v4())
            .assistant(ids[1]),
    )
    .await;
    seed_row(
        rig.db(),
        &chat,
        pending(1, "file-img").thumbnail(8, 8, &[1, 2, 3]),
    )
    .await;
    seed_row(rig.db(), &chat, pending(2, "file-doc")).await;
    seed_store(rig.db(), &chat, Some("vs_1")).await;
    soft_delete_chat(rig.db(), chat.id, at(50)).await;
    let tables = [
        "chats",
        "messages",
        "chat_turns",
        "message_reactions",
        "attachments",
        "thread_summaries",
    ];
    let mut before = Vec::new();
    for table in tables {
        before.push(table_count(rig.db(), table).await);
    }
    let outbox_before = crate::test_support::chats::outbox_rows(rig.db())
        .await
        .len();
    assert_eq!(deliver(&rig, &chat, 0).await, "ok");
    let mut after = Vec::new();
    for table in tables {
        after.push(table_count(rig.db(), table).await);
    }
    assert_eq!(before, after, "nothing is hard-purged");
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
    let attachments = attachment_rows(rig.db(), chat.id).await;
    assert!(
        attachments.iter().all(|row| row.deleted_at.is_none()),
        "child rows keep `deleted_at` empty"
    );
    assert!(
        attachments.iter().any(|row| row.img_thumbnail.is_some()),
        "the thumbnail stays"
    );
    assert_eq!(
        crate::test_support::chats::outbox_rows(rig.db())
            .await
            .len(),
        outbox_before,
        "no audit or other event is emitted"
    );
    assert!(
        !logs_contain("PRIVATE SUMMARY TEXT"),
        "summary text never reaches a log"
    );
}

#[tokio::test]
async fn a_turn_running_at_deletion_time_is_finalized_and_billed() {
    // inst-rt-running-turn: not cancelled by the chat deletion, finalized normally
    use crate::test_support::provider::{Gate, Reply, Step, completed, delta};
    let gate = Gate::new();
    let upstream = crate::test_support::provider::MockProvider::answering(Reply::Sse(vec![
        delta("par"),
        Step::Wait(gate.clone()),
        delta("tial"),
        completed("partial", (120, 30)),
    ]));
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream)).await;
    let chat = rig.std_chat().await;
    let mut open = rig
        .open_stream(chat.id, &json!({ "content": "hello" }))
        .await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    // the chat is deleted while the provider is still answering
    rig.quota
        .db
        .provider
        .delete_chat(&owner_scope(&a1()), chat.id, rig.quota.enqueuer.clone())
        .await
        .expect("the delete commits");
    gate.open();
    let rest = open.rest().await;
    assert_eq!(rest.last().unwrap().event, "done", "{rest:?}");
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "completed");
    let usage: Vec<Value> = crate::test_support::chats::outbox_rows(&rig.quota.db)
        .await
        .into_iter()
        .filter(|row| row.queue == rig.quota.outbox.queue_name)
        .map(|row| row.payload)
        .collect();
    assert_eq!(usage.len(), 1, "the turn is billed");
    assert_eq!(usage[0]["billing_outcome"], "completed");
}

// ── the other chat-cleanup identity checks ───────────────────────────────────

#[tokio::test]
async fn the_payload_helper_matches_the_enqueue_contract() {
    // inst-cp-return at the wiring level: the message the DELETE transaction writes parses
    let rig = maint_rig().await;
    let chat = deleted_chat(&rig, quota::STANDARD).await;
    let payload = chat_cleanup_payload(chat.tenant_id, chat.id, Uuid::new_v4());
    let handler = rig.cleanup_handler(5, true);
    let result = handler.handle(&cleanup_message(&payload, 0)).await;
    assert_eq!(result_label(&result), "ok");
}
