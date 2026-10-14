//! The chat-cleanup handler over fakes (store, storage provider, policy, metrics) and a virtual
//! timer: the rejection of a malformed message and of a missing or active chat, the database
//! errors that retry without any budget, the per-row attachment file cleanup (2xx and 404 as
//! success, the guarded `done` write, the attempt record with a fixed cause, `failed` at the limit
//! while the other rows go on, a row without a provider file, an unresolvable backend, a lost
//! guard, a database failure), the secondary Anthropic delete (memoized snapshot read, a skip with
//! the metric, never failing the primary row), the vector store cleanup (eligibility, placeholder,
//! 2xx and 404, the debt metric, the reject rule over the delivery count, the row kept on a
//! failure, a database failure), the fixed 5 s call deadline and the 20 s delivery budget checked
//! before every provider-bound call (including the case "primary finishes just before 20 s, the
//! snapshot is slow"), the provider purge states and the idempotence of every step.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithms
//! `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler` (`inst-cc-timer`,
//! `inst-cc-parse`, `inst-cc-malformed`, `inst-cc-return-malformed`, `inst-cc-load-chat`,
//! `inst-cc-chat-db`, `inst-cc-return-chat-db`, `inst-cc-active`, `inst-cc-return-active`,
//! `inst-cc-load-pending`, `inst-cc-pending-db`, `inst-cc-return-pending-db`, `inst-cc-each`,
//! `inst-cc-budget-stop`, `inst-cc-file`, `inst-cc-file-db`, `inst-cc-file-budget`,
//! `inst-cc-count-pending`, `inst-cc-still-pending`, `inst-cc-return-still-pending`,
//! `inst-cc-budget-accounting`, `inst-cc-vector-store`, `inst-cc-idempotent`, `inst-cc-bounded`,
//! `inst-cc-return`), `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file` (`inst-af-no-file`, `inst-af-return-no-file`,
//! `inst-af-target`, `inst-af-budget`, `inst-af-return-budget`, `inst-af-delete`,
//! `inst-af-success`, `inst-af-secondary`, `inst-af-mark-done`, `inst-af-failed`,
//! `inst-af-record-attempt`, `inst-af-exhausted`, `inst-af-return-pending`, `inst-af-guard-lost`,
//! `inst-af-return-skipped`, `inst-af-db`, `inst-af-return-db`, `inst-af-no-terminal`,
//! `inst-af-return`), `cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file` (`inst-sf-none`, `inst-sf-return-none`,
//! `inst-sf-budget`, `inst-sf-return-budget`, `inst-sf-resolve`, `inst-sf-snapshot`,
//! `inst-sf-fallback`, `inst-sf-skip`, `inst-sf-return-skip`, `inst-sf-budget-delete`,
//! `inst-sf-return-budget-delete`, `inst-sf-delete`, `inst-sf-no-block`, `inst-sf-return`),
//! `cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup` (`inst-vs-eligible`, `inst-vs-load`, `inst-vs-load-db`,
//! `inst-vs-return-load-db`, `inst-vs-none`, `inst-vs-return-none`, `inst-vs-placeholder`,
//! `inst-vs-return-placeholder`, `inst-vs-budget`, `inst-vs-return-budget`, `inst-vs-failed-rows`,
//! `inst-vs-record-debt`, `inst-vs-target`, `inst-vs-delete`, `inst-vs-success`,
//! `inst-vs-delete-row`, `inst-vs-delete-row-db`, `inst-vs-return-ok`, `inst-vs-failed`,
//! `inst-vs-keep-row`, `inst-vs-return-reject`, `inst-vs-return-retry`, `inst-vs-files-separate`),
//! `cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries` (`inst-cb-active`, `inst-cb-attachment-path`, `inst-cb-deleted`,
//! `inst-cb-transfer`, `inst-cb-no-lock`, `inst-cb-return`) and `cpt-cf-mini-chat-algo-background-maintenance-metrics` (`inst-mt-cleanup`,
//! `inst-mt-labels`); state `cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge`
//! (`inst-pp-files-retry`, `inst-pp-files-done`, `inst-pp-files-purged`, `inst-pp-files-debt`,
//! `inst-pp-store-purged`, `inst-pp-store-debt`, `inst-pp-store-retry`, `inst-pp-debt-terminal`),
//! the transition `inst-vsm-chat-cleanup` of
//! `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store` and the state
//! transitions `inst-cs-done`, `inst-cs-retry` and `inst-cs-failed` of
//! `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`; flow
//! `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup` (`inst-cd-handle`,
//! `inst-cd-retry`, `inst-cd-redeliver`, `inst-cd-rejected`, `inst-cd-dead-letter`,
//! `inst-cd-return`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`,
//! `cpt-cf-mini-chat-dod-background-maintenance-vector-store-ordering` and
//! `cpt-cf-mini-chat-dod-background-maintenance-cleanup-boundaries`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::Value;
use tracing_test::traced_test;
use uuid::Uuid;

use super::HandlerOutcome;
use crate::domain::attachments::provider::ProviderFailure;
use crate::domain::provider_registry::ProviderRegistry;
use crate::test_support::TENANT_A;
use crate::test_support::maintenance::{
    ANTHROPIC_ALIAS, AZURE_ALIAS, BACKEND, CallKind, FakeChatStore, FakeRow, PolicyMode, Step,
    UnitCleanup, chat_cleanup_payload, providers_json, reason, registry_of, unit_cleanup,
    unit_cleanup_with,
};
use crate::test_support::send::ANTHROPIC_MODEL;

const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_00C1);
const REQUEST: Uuid = Uuid::from_u128(0x5151_0000_0000_0000_0000_0000_0000_0C07);
const STORE_REASON: &str = "vector store delete: max attempts (5) reached";

fn payload() -> Value {
    chat_cleanup_payload(TENANT_A, CHAT, REQUEST)
}

async fn run(unit: &UnitCleanup, attempts: i16) -> HandlerOutcome {
    unit.worker
        .handle(&serde_json::to_vec(&payload()).unwrap(), attempts)
        .await
}

fn row(n: u128) -> FakeRow {
    FakeRow::pending(n, &format!("file-{n}"))
}

fn rows(count: u128) -> Vec<FakeRow> {
    (1..=count).map(row).collect()
}

fn std_chat(rows: Vec<FakeRow>) -> std::sync::Arc<FakeChatStore> {
    FakeChatStore::deleted_chat(crate::test_support::quota::STANDARD, rows)
}

fn slow() -> Step {
    // a call that answers 4.9 s into its 5 s deadline
    Step::takes(Duration::from_millis(4900), Step::Ok)
}

fn statuses(unit: &UnitCleanup) -> Vec<Option<String>> {
    unit.store
        .statuses()
        .into_iter()
        .map(|(status, _)| status)
        .collect()
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the expected status of a row is an Option"
)]
fn st(value: &str) -> Option<String> {
    Some(value.to_owned())
}

// ── the message and the chat ─────────────────────────────────────────────────

#[tokio::test]
async fn a_malformed_message_is_rejected_at_once_without_touching_anything() {
    // inst-cc-parse, inst-cc-malformed, inst-cc-return-malformed, inst-cd-rejected,
    // inst-cd-dead-letter
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    for bytes in [&b"garbage"[..], b"{}", b"[]", b""] {
        let outcome = unit.worker.handle(bytes, 0).await;
        assert!(matches!(outcome, HandlerOutcome::Reject(_)), "{outcome:?}");
    }
    assert!(unit.store.calls().is_empty());
    assert!(unit.storage.calls().is_empty());
    assert!(unit.metrics.is_silent());
}

#[tokio::test]
async fn a_chat_load_error_retries_without_any_provider_call() {
    // inst-cc-load-chat, inst-cc-chat-db, inst-cc-return-chat-db: infrastructure failure, no limit
    let unit = unit_cleanup(std_chat(rows(1)).fail("load_chat"));
    for attempts in [0, 4, 30] {
        assert_eq!(
            run(&unit, attempts).await,
            HandlerOutcome::Retry,
            "{attempts}"
        );
    }
    assert!(unit.storage.calls().is_empty());
    assert_eq!(unit.store.calls().len(), 3, "only the chat reads");
}

#[tokio::test]
async fn an_active_chat_is_rejected_without_a_provider_call() {
    // inst-cc-active, inst-cc-return-active, inst-cb-active, inst-cb-attachment-path
    let store = std_chat(rows(2)).with_store(Some("vs_1"));
    store.chat.lock().unwrap().as_mut().unwrap().deleted = false;
    let unit = unit_cleanup(store);
    let outcome = run(&unit, 0).await;
    assert_eq!(
        reason(&outcome).as_deref(),
        Some("chat is not soft-deleted")
    );
    assert!(unit.storage.calls().is_empty());
    assert!(unit.secondary.calls().is_empty());
    assert!(unit.store.writes().is_empty());
    assert_eq!(statuses(&unit), vec![st("pending"), st("pending")]);
    assert!(unit.store.has_store_row());
}

#[tokio::test]
async fn a_missing_chat_is_rejected_like_an_active_one() {
    // inst-cc-active: reason `chat not found`
    let store = std_chat(rows(1));
    *store.chat.lock().unwrap() = None;
    let unit = unit_cleanup(store);
    let outcome = run(&unit, 0).await;
    assert_eq!(reason(&outcome).as_deref(), Some("chat not found"));
    assert!(unit.storage.calls().is_empty());
}

#[tokio::test]
async fn the_pending_rows_query_error_retries() {
    // inst-cc-load-pending, inst-cc-pending-db, inst-cc-return-pending-db
    let unit = unit_cleanup(std_chat(rows(1)).fail("pending_attachments"));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls().is_empty());
    assert_eq!(unit.store.statuses(), vec![(st("pending"), 0)]);
}

#[tokio::test]
async fn a_deleted_chat_is_owned_by_chat_cleanup() {
    // inst-cb-deleted, inst-cb-transfer, inst-cb-return: the handler deletes the files of the
    // pending rows of a soft-deleted chat
    let unit = unit_cleanup(std_chat(rows(2)));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.storage.calls_of(CallKind::Primary).len(), 2);
    assert_eq!(statuses(&unit), vec![st("done"), st("done")]);
}

// ── the files ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_pending_file_is_deleted_then_marked_done() {
    // inst-cc-each, inst-cc-file, inst-cc-return, inst-af-delete, inst-af-success, inst-af-mark-done,
    // inst-af-return, inst-cs-done, inst-mt-cleanup, inst-cd-handle, inst-cd-return
    let unit = unit_cleanup(std_chat(rows(3)));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    let primary = unit.storage.calls_of(CallKind::Primary);
    assert_eq!(
        primary
            .iter()
            .map(|c| c.resource.as_str())
            .collect::<Vec<_>>(),
        vec!["file-1", "file-2", "file-3"],
        "in row order"
    );
    assert!(
        primary
            .iter()
            .all(|c| c.alias == AZURE_ALIAS && c.backend == BACKEND)
    );
    assert_eq!(statuses(&unit), vec![st("done"); 3]);
    assert_eq!(unit.metrics.completed(), vec!["file"; 3]);
    assert!(
        unit.storage.calls_of(CallKind::VectorStore).is_empty(),
        "no store row exists"
    );
}

#[tokio::test]
async fn a_404_counts_as_success() {
    // inst-af-success: the file is already gone
    let unit = unit_cleanup(std_chat(rows(1)));
    unit.storage.queue(CallKind::Primary, Step::NotFound);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done")]);
    assert_eq!(unit.metrics.completed(), vec!["file"]);
    assert!(unit.metrics.retries().is_empty());
}

#[tokio::test]
async fn the_done_write_is_guarded_and_the_row_is_never_marked_before_the_delete() {
    // inst-af-mark-done, inst-cs-done: provider delete first, then the write
    let unit = unit_cleanup(std_chat(rows(1)));
    run(&unit, 0).await;
    assert_eq!(unit.store.writes().len(), 1);
    assert!(unit.store.writes()[0].starts_with("mark_done"));
}

#[tokio::test]
async fn a_row_without_a_provider_file_is_done_without_a_provider_call() {
    // inst-af-no-file, inst-af-return-no-file: the upload never reached the provider
    let unit = unit_cleanup(std_chat(vec![row(1).no_file(), row(2)]));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.storage.calls_of(CallKind::Primary).len(), 1);
    assert_eq!(statuses(&unit), vec![st("done"), st("done")]);
    assert_eq!(unit.metrics.completed(), vec!["file", "file"]);
}

#[tokio::test]
async fn a_failed_delete_records_an_attempt_and_keeps_the_row_pending() {
    // inst-af-failed, inst-af-record-attempt, inst-af-return-pending, inst-cs-retry,
    // inst-cc-still-pending, inst-cc-return-still-pending, inst-pp-files-retry
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    unit.storage
        .queue(CallKind::Primary, Step::Fail(ProviderFailure::Transient));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.store.statuses(), vec![(st("pending"), 1)]);
    assert_eq!(
        unit.metrics.retries(),
        vec![("file".to_owned(), "provider_error".to_owned())]
    );
    assert!(unit.metrics.failed().is_empty());
    assert!(
        unit.storage.calls_of(CallKind::VectorStore).is_empty(),
        "the store is not touched"
    );
    assert!(unit.store.has_store_row());
}

#[tokio::test]
async fn the_last_error_is_a_fixed_cause_without_provider_text_or_identifiers() {
    // inst-af-record-attempt
    for failure in [ProviderFailure::Transient, ProviderFailure::Other] {
        let unit = unit_cleanup(std_chat(rows(1)));
        unit.storage.queue(CallKind::Primary, Step::Fail(failure));
        run(&unit, 0).await;
        let cause = unit
            .store
            .row(row(1).id)
            .last_error
            .expect("a cause is recorded");
        assert!(!cause.is_empty());
        for leaked in ["file-1", "example.com", "http", BACKEND] {
            assert!(!cause.contains(leaked), "{failure:?}: {cause}");
        }
    }
}

#[tokio::test]
async fn the_attempt_budget_makes_a_row_failed_while_the_others_finish() {
    // inst-af-exhausted, inst-cs-failed, inst-cc-each: `failed` is debt, the loop goes on
    let unit = unit_cleanup(std_chat(vec![row(1).attempts(4), row(2)]));
    unit.storage
        .queue(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("failed"), st("done")]);
    assert_eq!(unit.store.row(row(1).id).attempts, 5);
    assert_eq!(unit.metrics.failed(), vec!["file"]);
    assert_eq!(unit.metrics.completed(), vec!["file"]);
    assert!(
        unit.metrics.retries().is_empty(),
        "a terminal failure is not a retry"
    );
}

#[tokio::test]
async fn the_budget_is_cleanup_worker_max_attempts() {
    // inst-af-exhausted with a configured limit of 2
    let store = std_chat(rows(1));
    let unit = unit_cleanup_with(
        store,
        crate::test_support::maintenance::providers(),
        2,
        true,
    );
    unit.storage
        .always(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.store.statuses(), vec![(st("pending"), 1)]);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(unit.store.statuses(), vec![(st("failed"), 2)]);
}

#[tokio::test]
async fn an_unresolvable_storage_backend_is_a_failed_attempt() {
    // inst-af-target: cause `the storage backend maps to no provider`, no provider call
    let mut stray = row(1);
    stray.storage_backend = "no-such-backend".to_owned();
    let unit = unit_cleanup(std_chat(vec![stray]));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls().is_empty());
    assert_eq!(unit.store.statuses(), vec![(st("pending"), 1)]);
    assert_eq!(
        unit.store.row(row(1).id).last_error.as_deref(),
        Some("the storage backend maps to no provider")
    );
}

#[tokio::test]
async fn a_hung_delete_hits_the_five_second_deadline_as_a_counted_attempt() {
    // inst-af-delete: the call that hits the deadline is a failed call with a fixed cause
    let unit = unit_cleanup(std_chat(rows(1)));
    unit.storage.queue(CallKind::Primary, Step::Hang);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.time.elapsed(), Duration::from_secs(5));
    assert_eq!(unit.store.statuses(), vec![(st("pending"), 1)]);
    assert_eq!(
        unit.store.row(row(1).id).last_error.as_deref(),
        Some("the provider call exceeded its deadline")
    );
    assert_eq!(unit.metrics.retries().len(), 1);
}

#[tokio::test]
async fn a_database_error_on_the_done_write_retries_without_an_attempt() {
    // inst-af-db, inst-af-return-db, inst-cc-file-db: the next delivery repeats the delete
    let store = std_chat(rows(3));
    let unit = unit_cleanup(store.fail("mark_done"));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(
        unit.store.statuses()[0],
        (st("pending"), 0),
        "no attempt is counted"
    );
    assert_eq!(
        unit.storage.calls_of(CallKind::Primary).len(),
        1,
        "the loop stopped at once"
    );
    unit.store.stop_failing();
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done"); 3]);
}

#[tokio::test]
async fn a_database_error_on_the_attempt_write_retries_without_an_attempt() {
    // inst-af-db, inst-cc-file-db
    let unit = unit_cleanup(std_chat(rows(2)).fail("record_failed_attempt"));
    unit.storage
        .always(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.store.statuses()[0], (st("pending"), 0));
    assert_eq!(unit.storage.calls_of(CallKind::Primary).len(), 1);
    assert!(unit.metrics.is_silent());
}

#[tokio::test]
async fn rows_finished_before_a_database_error_stay_finished() {
    // inst-cc-file-db: rows already finished in this delivery stay finished, the rest wait
    let store = std_chat(rows(3)).fail_on_call("mark_done", 2);
    let unit = unit_cleanup(store);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(
        statuses(&unit),
        vec![st("done"), st("pending"), st("pending")]
    );
    assert_eq!(unit.store.statuses()[1].1, 0, "no attempt is counted");
    assert_eq!(
        unit.storage.calls_of(CallKind::Primary).len(),
        2,
        "the loop stopped at once"
    );
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done"); 3]);
}

#[tokio::test]
async fn a_lost_guard_skips_the_row_without_an_error_or_a_metric() {
    // inst-af-guard-lost, inst-af-return-skipped, inst-cb-no-lock: the attachment cleanup handler
    // finished the row first; 404 as success makes the duplicate delete harmless
    let target = row(1);
    let store = std_chat(vec![target.clone(), row(2)]).lose_guard_for(target.id);
    let unit = unit_cleanup(store);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done"), st("done")]);
    assert_eq!(
        unit.metrics.completed(),
        vec!["file"],
        "only the row this path finished counts"
    );
}

#[tokio::test]
async fn a_lost_guard_on_an_attempt_write_is_skipped_too() {
    // inst-af-guard-lost: the guarded attempt matches no row
    let target = row(1);
    let store = std_chat(vec![target.clone()]).lose_guard_for(target.id);
    let unit = unit_cleanup(store);
    unit.storage
        .queue(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert!(unit.metrics.retries().is_empty() && unit.metrics.failed().is_empty());
}

#[tokio::test]
async fn done_and_failed_rows_are_never_processed() {
    // inst-af-no-terminal: a `failed` row of a deleted chat is not re-attempted by any delivery
    let rows = vec![
        row(1).with_status("done"),
        row(2).with_status("failed").attempts(5),
    ];
    let unit = unit_cleanup(std_chat(rows));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert!(unit.storage.calls().is_empty());
    assert_eq!(
        unit.store.statuses(),
        vec![(st("done"), 0), (st("failed"), 5)]
    );
    assert!(unit.store.writes().is_empty());
}

// ── the secondary Anthropic file ─────────────────────────────────────────────

fn anthropic_chat(rows: Vec<FakeRow>) -> std::sync::Arc<FakeChatStore> {
    FakeChatStore::deleted_chat(ANTHROPIC_MODEL, rows)
}

#[tokio::test]
async fn the_secondary_file_is_deleted_on_the_alias_of_the_chat_model_provider() {
    // inst-sf-delete, inst-af-secondary, inst-sf-resolve, inst-sf-return
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    let secondary = unit.secondary.calls_of(CallKind::Secondary);
    assert_eq!(secondary.len(), 1);
    assert_eq!(secondary[0].resource, "file_ant_1");
    assert_eq!(secondary[0].alias, ANTHROPIC_ALIAS);
    assert_eq!(statuses(&unit), vec![st("done")]);
    assert!(unit.metrics.skipped().is_empty());
}

#[tokio::test]
async fn the_primary_delete_comes_before_the_secondary_delete() {
    // inst-af-secondary: the secondary cleanup runs only after the primary file is gone
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.storage.queue(
        CallKind::Primary,
        Step::takes(Duration::from_secs(1), Step::Ok),
    );
    run(&unit, 0).await;
    let primary = unit.storage.calls_of(CallKind::Primary);
    let secondary = unit.secondary.calls_of(CallKind::Secondary);
    assert!(secondary[0].at >= primary[0].at + Duration::from_secs(1));
}

#[tokio::test]
async fn a_failed_primary_delete_makes_no_secondary_call() {
    // inst-af-failed: the secondary is part of the success branch
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.storage
        .queue(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.secondary.calls().is_empty());
    assert_eq!(unit.policy.call_count(), 0);
}

#[tokio::test]
async fn the_snapshot_is_read_at_most_once_per_delivery() {
    // inst-sf-resolve: the per-delivery memo of the secondary target
    let unit = unit_cleanup(anthropic_chat(vec![
        row(1).with_secondary("file_ant_1"),
        row(2).with_secondary("file_ant_2"),
        row(3).with_secondary("file_ant_3"),
    ]));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.policy.call_count(), 1);
    assert_eq!(unit.secondary.calls_of(CallKind::Secondary).len(), 3);
}

#[tokio::test]
async fn the_snapshot_is_read_without_a_user_id() {
    // inst-sf-snapshot
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    run(&unit, 0).await;
    assert_eq!(unit.policy.users(), vec![Uuid::nil()]);
}

#[tokio::test]
async fn no_snapshot_is_read_when_no_row_has_a_secondary_copy() {
    // inst-sf-none, inst-sf-return-none
    let mut not_uploaded = row(2).with_secondary("file_ant_2");
    not_uploaded.secondary_status = "failed".to_owned();
    let mut no_id = row(3).with_secondary("x");
    no_id.secondary_file_id = None;
    let unit = unit_cleanup(anthropic_chat(vec![row(1), not_uploaded, no_id]));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.policy.call_count(), 0);
    assert!(unit.secondary.calls().is_empty());
    assert!(unit.metrics.skipped().is_empty());
}

#[tokio::test]
async fn a_secondary_404_is_success() {
    // inst-sf-delete
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.secondary.queue(CallKind::Secondary, Step::NotFound);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done")]);
}

#[traced_test]
#[tokio::test]
async fn a_failed_secondary_delete_is_logged_not_retried_and_never_fails_the_row() {
    // inst-sf-delete, inst-sf-no-block: logged without the file id, the primary row stays done
    let unit = unit_cleanup(anthropic_chat(vec![
        row(1).with_secondary("file_ant_SECRET"),
    ]));
    unit.secondary
        .queue(CallKind::Secondary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(
        unit.secondary.calls_of(CallKind::Secondary).len(),
        1,
        "not retried"
    );
    assert_eq!(
        unit.store.statuses(),
        vec![(st("done"), 0)],
        "the primary row is untouched by it"
    );
    assert!(
        !logs_contain("file_ant_SECRET"),
        "the file id is not logged"
    );
}

#[tokio::test]
async fn a_hung_secondary_delete_hits_the_deadline_and_is_not_retried() {
    // inst-sf-delete: the five-second deadline, a logged skip, the row still done
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.secondary.queue(CallKind::Secondary, Step::Hang);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.time.elapsed(), Duration::from_secs(5));
    assert_eq!(unit.secondary.calls_of(CallKind::Secondary).len(), 1);
    assert_eq!(unit.store.statuses(), vec![(st("done"), 0)]);
}

#[traced_test]
#[tokio::test]
async fn without_an_anthropic_client_the_secondary_is_skipped_with_the_metric_and_a_warning() {
    // inst-sf-skip, inst-sf-return-skip: the label is the row's secondary provider kind
    let store = anthropic_chat(vec![row(1).with_secondary("file_ant_SECRET")]);
    let unit = unit_cleanup_with(
        store,
        crate::test_support::maintenance::providers(),
        5,
        false,
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.skipped(), vec!["anthropic"]);
    assert_eq!(
        unit.store.statuses(),
        vec![(st("done"), 0)],
        "never failed because of it"
    );
    assert!(unit.secondary.calls().is_empty());
    assert!(logs_contain(&CHAT.to_string()), "the chat id is logged");
    assert!(
        logs_contain(&row(1).id.to_string()),
        "the attachment id is logged"
    );
    assert!(!logs_contain("file_ant_SECRET"), "no file id");
}

#[tokio::test]
async fn an_unresolvable_alias_skips_the_secondary_with_the_metric() {
    // inst-sf-skip: no Anthropic provider exists for the tenant
    let mut config = providers_json();
    config["providers"]
        .as_object_mut()
        .unwrap()
        .remove("anthropic");
    let unit = unit_cleanup_with(
        anthropic_chat(vec![row(1).with_secondary("file_ant_1")]),
        registry_of(config),
        5,
        true,
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.skipped(), vec!["anthropic"]);
    assert!(unit.secondary.calls().is_empty());
    assert_eq!(statuses(&unit), vec![st("done")]);
}

#[tokio::test]
async fn several_anthropic_providers_with_the_model_gone_skip_the_secondary() {
    // inst-sf-fallback, inst-sf-skip: a guess between several accounts is never made
    let mut config = providers_json();
    config["providers"]["anthropic_eu"] = serde_json::json!({
        "kind": "anthropic_messages", "host": "anthropic-eu.example.com",
        "storage_kind": "openai", "api_path": "/v1/messages"
    });
    let store = FakeChatStore::deleted_chat(
        "a-model-gone-from-the-catalog",
        vec![row(1).with_secondary("file_ant_1")],
    );
    let unit = unit_cleanup_with(store, registry_of(config), 5, true);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.skipped(), vec!["anthropic"]);
    assert!(unit.secondary.calls().is_empty());
}

#[tokio::test]
async fn an_unreadable_snapshot_falls_back_to_the_single_anthropic_provider() {
    // inst-sf-snapshot, inst-sf-fallback
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.policy.set_mode(PolicyMode::Fail);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(
        unit.secondary.calls_of(CallKind::Secondary)[0].alias,
        ANTHROPIC_ALIAS
    );
    assert_eq!(unit.policy.call_count(), 1);
}

#[tokio::test]
async fn a_model_gone_from_the_catalog_falls_back_to_the_single_anthropic_provider() {
    // inst-sf-fallback
    let store =
        FakeChatStore::deleted_chat("retired-model", vec![row(1).with_secondary("file_ant_1")]);
    let unit = unit_cleanup(store);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(
        unit.secondary.calls_of(CallKind::Secondary)[0].alias,
        ANTHROPIC_ALIAS
    );
}

#[tokio::test]
async fn a_hung_snapshot_read_ends_at_the_deadline_and_falls_back() {
    // inst-sf-snapshot: bounded by the 5 s deadline
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.policy.set_mode(PolicyMode::Hang);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.time.elapsed(), Duration::from_secs(5));
    assert_eq!(
        unit.secondary.calls_of(CallKind::Secondary)[0].alias,
        ANTHROPIC_ALIAS
    );
}

// ── the delivery budget ──────────────────────────────────────────────────────

#[tokio::test]
async fn no_row_starts_after_the_delivery_budget() {
    // inst-cc-timer, inst-cc-each, inst-cc-budget-stop, inst-cc-bounded: rows start at 0, 4.9,
    // 9.8, 14.7 and 19.6 s; the sixth would start at 24.5 s
    let unit = unit_cleanup(std_chat(rows(6)));
    unit.storage.always(CallKind::Primary, slow());
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.storage.calls_of(CallKind::Primary).len(), 5);
    assert_eq!(
        statuses(&unit),
        vec![
            st("done"),
            st("done"),
            st("done"),
            st("done"),
            st("done"),
            st("pending")
        ]
    );
    assert_eq!(
        unit.store.statuses()[5].1,
        0,
        "an unattempted row keeps its attempts"
    );
    assert_eq!(unit.store.row(row(6).id).last_error, None);
}

#[tokio::test]
async fn the_next_delivery_continues_from_the_persisted_state() {
    // inst-cd-redeliver, inst-cc-idempotent, inst-pp-files-done: `done` rows are skipped
    let unit = unit_cleanup(std_chat(rows(6)).with_store(Some("vs_1")));
    unit.storage.always(CallKind::Primary, slow());
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(
        unit.storage.calls_of(CallKind::Primary).len(),
        6,
        "each file is deleted once"
    );
    assert_eq!(statuses(&unit), vec![st("done"); 6]);
    assert_eq!(unit.storage.calls_of(CallKind::VectorStore).len(), 1);
    assert!(!unit.store.has_store_row());
}

#[traced_test]
#[tokio::test]
async fn a_budget_stop_logs_the_chat_id_and_the_rows_left_without_file_ids() {
    // inst-cc-budget-stop: info level
    let unit = unit_cleanup(std_chat(rows(7)));
    unit.storage.always(CallKind::Primary, slow());
    run(&unit, 0).await;
    assert!(logs_contain(&CHAT.to_string()));
    assert!(
        !logs_contain("file-6") && !logs_contain("file-7"),
        "no file identifier"
    );
}

#[tokio::test]
async fn the_first_row_of_every_delivery_is_always_started() {
    // inst-cc-bounded: each budget-driven Retry makes progress
    let unit = unit_cleanup(std_chat(rows(2)));
    unit.storage.always(
        CallKind::Primary,
        Step::takes(Duration::from_secs(25), Step::Ok),
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(statuses(&unit), vec![st("done"), st("pending")]);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done"), st("done")]);
}

#[tokio::test]
async fn the_budget_is_measured_from_the_entry_of_each_delivery() {
    // inst-cc-timer: the timer starts at handler entry, not at process start
    let unit = unit_cleanup(std_chat(rows(1)));
    unit.time.advance(Duration::from_secs(3600));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(statuses(&unit), vec![st("done")]);
}

#[tokio::test]
async fn the_budget_check_before_the_primary_delete_returns_without_a_write() {
    // inst-af-budget, inst-af-return-budget, inst-cc-file-budget: the row stays pending and no
    // attempt is counted, the loop stops
    let unit = unit_cleanup(std_chat(rows(6)));
    unit.storage.always(CallKind::Primary, slow());
    run(&unit, 0).await;
    let writes = unit.store.writes();
    assert_eq!(
        writes.len(),
        5,
        "five done writes, nothing for the sixth: {writes:?}"
    );
}

#[tokio::test]
async fn a_primary_that_ends_at_the_budget_leaves_the_secondary_for_the_next_delivery() {
    // inst-sf-budget, inst-sf-return-budget, inst-af-secondary, inst-cc-file-budget: the fifth
    // row's primary finishes at 20.1 s; the timer is re-checked before the snapshot read
    let mut all = rows(4);
    all.push(row(5).with_secondary("file_ant_5"));
    let unit = unit_cleanup(anthropic_chat(all));
    for _ in 0..4 {
        unit.storage.queue(CallKind::Primary, slow());
    }
    unit.storage.queue(
        CallKind::Primary,
        Step::takes(Duration::from_millis(500), Step::Ok),
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(
        unit.policy.call_count(),
        0,
        "no snapshot read after the budget"
    );
    assert!(unit.secondary.calls().is_empty());
    assert_eq!(
        unit.store.statuses()[4],
        (st("pending"), 0),
        "no attempt is counted"
    );
    assert_eq!(statuses(&unit)[..4], vec![st("done"); 4]);
    // the next delivery deletes the primary file again (404 counts as success) and then the
    // secondary
    unit.storage.queue(CallKind::Primary, Step::NotFound);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(unit.secondary.calls_of(CallKind::Secondary).len(), 1);
    assert_eq!(statuses(&unit)[4], st("done"));
}

#[tokio::test]
async fn a_primary_that_finishes_just_before_the_budget_with_a_slow_snapshot_starts_no_secondary() {
    // inst-sf-budget-delete, inst-sf-return-budget-delete, inst-cc-file-budget: primary done at
    // 19.6 s, the snapshot read uses its whole 5 s deadline, so no secondary delete may start
    let mut all = rows(3);
    all.push(row(4).with_secondary("file_ant_4"));
    let unit = unit_cleanup(anthropic_chat(all));
    for _ in 0..4 {
        unit.storage.queue(CallKind::Primary, slow());
    }
    unit.policy.set_mode(PolicyMode::Hang);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(
        unit.policy.call_count(),
        1,
        "the snapshot read started at 19.6 s"
    );
    assert_eq!(unit.policy.times(), vec![Duration::from_millis(19_600)]);
    assert!(
        unit.secondary.calls().is_empty(),
        "no secondary delete starts after the budget"
    );
    assert_eq!(
        unit.store.statuses()[3],
        (st("pending"), 0),
        "no attempt is counted"
    );
    assert_eq!(statuses(&unit)[..3], vec![st("done"); 3]);
}

#[tokio::test]
async fn a_budget_stop_after_the_snapshot_read_leaves_the_row_pending_and_the_next_delivery_reads_again()
 {
    // inst-sf-budget-delete: the snapshot read used the budget; the row stays pending without an
    // attempt, the memo lives within one delivery, so the next delivery reads again
    let unit = unit_cleanup(anthropic_chat(vec![row(1).with_secondary("file_ant_1")]));
    unit.policy
        .set_mode(PolicyMode::Takes(Duration::from_secs(21)));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(unit.policy.call_count(), 1);
    assert_eq!(unit.store.statuses(), vec![(st("pending"), 0)]);
    unit.policy.set_mode(PolicyMode::Serve);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(unit.policy.call_count(), 2);
}

// ── the vector store ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_store_is_not_touched_while_a_row_is_pending() {
    // inst-cc-count-pending, inst-cc-still-pending, inst-cc-return-still-pending, inst-vs-eligible,
    // inst-vs-files-separate: deleting a store never replaces deleting the files
    let unit = unit_cleanup(std_chat(rows(2)).with_store(Some("vs_1")));
    unit.storage.queue(CallKind::Primary, Step::Ok);
    unit.storage
        .queue(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
    assert!(unit.store.has_store_row());
    assert!(
        !unit
            .store
            .calls()
            .iter()
            .any(|c| c.starts_with("vector_store"))
    );
}

#[tokio::test]
async fn a_pending_count_error_retries_without_touching_the_store() {
    // inst-cc-count-pending, inst-cc-still-pending: a failed count is a Retry
    let unit = unit_cleanup(
        std_chat(rows(1))
            .with_store(Some("vs_1"))
            .fail("count_pending"),
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
    assert!(unit.store.has_store_row());
}

#[tokio::test]
async fn the_store_is_deleted_after_every_file_is_done() {
    // inst-cc-vector-store, inst-vs-load, inst-vs-target, inst-vs-delete, inst-vs-success,
    // inst-vs-delete-row, inst-vs-return-ok, inst-vsm-chat-cleanup, inst-pp-files-purged,
    // inst-pp-store-purged, inst-mt-cleanup
    let unit = unit_cleanup(std_chat(rows(2)).with_store(Some("vs_123")));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    let calls = unit.storage.calls();
    assert_eq!(
        calls.iter().map(|c| c.kind).collect::<Vec<_>>(),
        vec![CallKind::Primary, CallKind::Primary, CallKind::VectorStore],
        "files first, then the store"
    );
    assert_eq!(calls[2].resource, "vs_123");
    assert_eq!(calls[2].alias, AZURE_ALIAS);
    assert_eq!(calls[2].backend, BACKEND);
    assert!(
        !unit.store.has_store_row(),
        "the row is the durable completion marker"
    );
    assert_eq!(
        unit.metrics.completed(),
        vec!["file", "file", "vector_store"]
    );
    assert_eq!(unit.metrics.debts(), 0);
}

#[tokio::test]
async fn a_store_404_is_success() {
    // inst-vs-success
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    unit.storage.queue(CallKind::VectorStore, Step::NotFound);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert!(!unit.store.has_store_row());
    assert_eq!(
        unit.metrics.completed().last().map(String::as_str),
        Some("vector_store")
    );
}

#[tokio::test]
async fn a_deleted_chat_without_pending_rows_still_deletes_its_store() {
    // inst-pp-files-done (StorePending from the start): no `pending` row, a store row
    let unit = unit_cleanup(std_chat(vec![row(1).with_status("done")]).with_store(Some("vs_1")));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.storage.calls_of(CallKind::VectorStore).len(), 1);
    assert!(!unit.store.has_store_row());
}

#[tokio::test]
async fn a_chat_that_never_had_a_document_is_purged_at_once() {
    // inst-vs-none, inst-vs-return-none: no row, no provider call
    let unit = unit_cleanup(std_chat(Vec::new()));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert!(unit.storage.calls().is_empty());
    assert!(unit.metrics.is_silent());
}

#[tokio::test]
async fn a_load_error_of_the_store_row_retries_without_a_limit() {
    // inst-vs-load-db, inst-vs-return-load-db: even on the delivery that would reach the limit
    let unit = unit_cleanup(
        std_chat(rows(1))
            .with_store(Some("vs_1"))
            .fail("vector_store"),
    );
    for attempts in [0, 4, 9, 50] {
        assert_eq!(
            run(&unit, attempts).await,
            HandlerOutcome::Retry,
            "{attempts}"
        );
    }
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
}

#[tokio::test]
async fn a_placeholder_row_is_deleted_without_a_provider_call() {
    // inst-vs-placeholder, inst-vs-return-placeholder: guarded by `vector_store_id IS NULL`
    let unit = unit_cleanup(std_chat(rows(1)).with_store(None));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
    assert!(!unit.store.has_store_row());
    assert!(
        unit.store
            .writes()
            .iter()
            .any(|w| w.contains("placeholder_only=true")),
        "{:?}",
        unit.store.writes()
    );
}

#[tokio::test]
async fn a_placeholder_delete_error_retries() {
    // inst-vs-return-placeholder: `Retry` on a database error
    let unit = unit_cleanup(
        std_chat(Vec::new())
            .with_store(None)
            .fail("delete_vector_store_row"),
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.store.has_store_row());
    assert!(unit.storage.calls().is_empty());
}

#[tokio::test]
async fn failed_rows_record_the_debt_metric_before_the_store_delete() {
    // inst-vs-failed-rows, inst-vs-record-debt, inst-pp-store-debt, inst-pp-files-debt: the store
    // is released although provider-file debt remains
    let unit = unit_cleanup(
        std_chat(vec![row(1).with_status("failed").attempts(5), row(2)]).with_store(Some("vs_1")),
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert_eq!(unit.metrics.debts(), 1);
    assert_eq!(unit.storage.calls_of(CallKind::VectorStore).len(), 1);
    assert_eq!(
        unit.store.statuses()[0],
        (st("failed"), 5),
        "the debt stays"
    );
}

#[tokio::test]
async fn the_debt_metric_counts_again_for_a_retried_delete() {
    // inst-vs-record-debt: recorded before each delete attempt of a chat with `failed` rows
    let unit = unit_cleanup(std_chat(vec![row(1).with_status("failed")]).with_store(Some("vs_1")));
    unit.storage.always(
        CallKind::VectorStore,
        Step::Fail(ProviderFailure::Transient),
    );
    run(&unit, 0).await;
    run(&unit, 1).await;
    assert_eq!(unit.metrics.debts(), 2);
}

#[tokio::test]
async fn a_chat_without_failed_rows_records_no_debt() {
    // inst-vs-failed-rows
    let unit = unit_cleanup(std_chat(rows(2)).with_store(Some("vs_1")));
    run(&unit, 0).await;
    assert_eq!(unit.metrics.debts(), 0);
}

#[tokio::test]
async fn a_failed_store_delete_keeps_the_row_and_retries() {
    // inst-vs-failed, inst-vs-keep-row, inst-vs-return-retry, inst-pp-store-retry
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    unit.storage.always(
        CallKind::VectorStore,
        Step::Fail(ProviderFailure::Transient),
    );
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.store.has_store_row(), "the row is unchanged");
    assert!(
        !unit
            .store
            .writes()
            .iter()
            .any(|w| w.starts_with("delete_vector_store_row"))
    );
    assert_eq!(
        unit.metrics.retries(),
        vec![(
            "vector_store".to_owned(),
            "vector_store_delete_failed".to_owned()
        )]
    );
    assert!(unit.metrics.failed().is_empty());
}

#[tokio::test]
async fn the_delivery_that_reaches_the_limit_rejects_and_keeps_the_row() {
    // inst-vs-return-reject, inst-cd-rejected, inst-cd-dead-letter: counted over every delivery
    // of the message, 0-based `attempts` 4 is the fifth
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    unit.storage
        .always(CallKind::VectorStore, Step::Fail(ProviderFailure::Other));
    for attempts in 1..=3 {
        assert_eq!(
            run(&unit, attempts).await,
            HandlerOutcome::Retry,
            "{attempts}"
        );
    }
    assert_eq!(
        run(&unit, 4).await,
        HandlerOutcome::Reject(STORE_REASON.to_owned())
    );
    assert!(unit.store.has_store_row(), "a replay retries the delete");
    assert_eq!(unit.metrics.failed(), vec!["vector_store"]);
    assert_eq!(
        run(&unit, 11).await,
        HandlerOutcome::Reject(STORE_REASON.to_owned())
    );
}

#[tokio::test]
async fn the_store_limit_is_cleanup_worker_max_attempts() {
    // inst-vs-return-reject with a limit of 2: `attempts` at least `max_attempts` minus 1
    let unit = unit_cleanup_with(
        std_chat(rows(1)).with_store(Some("vs_1")),
        crate::test_support::maintenance::providers(),
        2,
        true,
    );
    unit.storage
        .always(CallKind::VectorStore, Step::Fail(ProviderFailure::Other));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert_eq!(
        run(&unit, 1).await,
        HandlerOutcome::Reject("vector store delete: max attempts (2) reached".to_owned())
    );
}

#[tokio::test]
async fn a_replayed_delete_succeeds_and_removes_the_row() {
    // inst-cd-dead-letter: a replay of the dead letter retries the delete
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    unit.storage
        .always(CallKind::VectorStore, Step::Fail(ProviderFailure::Other));
    assert!(matches!(run(&unit, 4).await, HandlerOutcome::Reject(_)));
    unit.storage.always(CallKind::VectorStore, Step::Ok);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    assert!(!unit.store.has_store_row());
}

#[tokio::test]
async fn a_hung_store_delete_is_a_failed_delete_after_five_seconds() {
    // inst-vs-delete, inst-vs-failed: the deadline hit counts as a failed delete
    let unit = unit_cleanup(std_chat(rows(1)).with_store(Some("vs_1")));
    unit.storage.always(CallKind::VectorStore, Step::Hang);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.store.has_store_row());
    let hung = unit.time.elapsed();
    assert_eq!(hung, Duration::from_secs(5));
    assert_eq!(
        run(&unit, 4).await,
        HandlerOutcome::Reject(STORE_REASON.to_owned())
    );
}

#[tokio::test]
async fn an_unresolvable_store_provider_label_is_a_failed_delete() {
    // inst-vs-target: no provider call, the failure rules apply
    let store = std_chat(rows(1)).with_store(Some("vs_1"));
    store.store_row.lock().unwrap().as_mut().unwrap().provider = "unknown-label".to_owned();
    let unit = unit_cleanup(store);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
    assert!(unit.store.has_store_row());
    assert_eq!(
        run(&unit, 4).await,
        HandlerOutcome::Reject(STORE_REASON.to_owned())
    );
}

#[tokio::test]
async fn a_row_delete_error_after_the_provider_delete_retries() {
    // inst-vs-delete-row-db: the next delivery deletes the store again and gets 404
    let store = std_chat(rows(1))
        .with_store(Some("vs_1"))
        .fail("delete_vector_store_row");
    let unit = unit_cleanup(store);
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    assert!(unit.store.has_store_row());
    assert!(
        !unit
            .metrics
            .completed()
            .contains(&"vector_store".to_owned())
    );
    unit.store.stop_failing();
    unit.storage.queue(CallKind::VectorStore, Step::NotFound);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert!(!unit.store.has_store_row());
}

#[tokio::test]
async fn the_budget_before_the_store_call_returns_retry_without_a_call_or_the_debt_metric() {
    // inst-vs-budget, inst-vs-return-budget, inst-cc-budget-accounting: no provider call, no debt,
    // no store attempt recorded, and never a Reject, even on the delivery that would reach the
    // limit
    let mut all = rows(5);
    all.push(row(6).with_status("failed"));
    let unit = unit_cleanup(std_chat(all).with_store(Some("vs_1")));
    unit.storage.always(CallKind::Primary, slow());
    assert_eq!(run(&unit, 4).await, HandlerOutcome::Retry);
    assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
    assert_eq!(unit.metrics.debts(), 0);
    assert!(unit.metrics.failed().is_empty());
    assert!(unit.store.has_store_row());
    assert_eq!(
        statuses(&unit)[..5],
        vec![st("done"); 5],
        "every row is already terminal"
    );
    // the next delivery goes straight to the store delete
    assert_eq!(run(&unit, 5).await, HandlerOutcome::Ok);
    assert!(!unit.store.has_store_row());
    assert_eq!(unit.metrics.debts(), 1);
}

#[tokio::test]
async fn a_budget_driven_retry_never_calls_the_store_while_a_row_is_pending() {
    // inst-cc-budget-accounting: the ordering invariant holds on every delivery
    let unit = unit_cleanup(std_chat(rows(8)).with_store(Some("vs_1")));
    unit.storage.always(CallKind::Primary, slow());
    for attempts in 0..2 {
        let outcome = run(&unit, attempts).await;
        let pending = unit
            .store
            .statuses()
            .iter()
            .filter(|(s, _)| s.as_deref() == Some("pending"))
            .count();
        if pending > 0 {
            assert_eq!(outcome, HandlerOutcome::Retry);
            assert!(unit.storage.calls_of(CallKind::VectorStore).is_empty());
        }
    }
}

// ── states, idempotence ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_with_file_debt_and_no_store_is_purged_with_debt_and_stays_so() {
    // inst-pp-files-debt, inst-pp-debt-terminal: failed rows are terminal in P1
    let unit = unit_cleanup(std_chat(vec![
        row(1).with_status("failed").attempts(5),
        row(2),
    ]));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    let calls_after_first = unit.storage.calls().len();
    for attempts in 1..4 {
        assert_eq!(run(&unit, attempts).await, HandlerOutcome::Ok);
    }
    assert_eq!(
        unit.storage.calls().len(),
        calls_after_first,
        "no later delivery re-attempts it"
    );
    assert_eq!(unit.store.statuses()[0], (st("failed"), 5));
}

#[tokio::test]
async fn every_step_is_idempotent_under_redelivery() {
    // inst-cc-idempotent, inst-vs-none: a finished message re-run changes and calls nothing
    let unit = unit_cleanup(std_chat(rows(2)).with_store(Some("vs_1")));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Ok);
    let calls = unit.storage.calls().len();
    let writes = unit.store.writes().len();
    for attempts in 1..4 {
        assert_eq!(run(&unit, attempts).await, HandlerOutcome::Ok);
    }
    assert_eq!(unit.storage.calls().len(), calls);
    assert_eq!(unit.store.writes().len(), writes);
    assert_eq!(
        unit.metrics.completed().len(),
        3,
        "2 files and the store, once"
    );
}

#[tokio::test]
async fn a_crash_after_the_provider_delete_repeats_the_idempotent_delete() {
    // inst-cc-idempotent, inst-af-db: the done write failed; the redelivery deletes again (404)
    let unit = unit_cleanup(std_chat(rows(1)).fail("mark_done"));
    assert_eq!(run(&unit, 0).await, HandlerOutcome::Retry);
    unit.store.stop_failing();
    unit.storage.queue(CallKind::Primary, Step::NotFound);
    assert_eq!(run(&unit, 1).await, HandlerOutcome::Ok);
    assert_eq!(unit.storage.calls_of(CallKind::Primary).len(), 2);
    assert_eq!(statuses(&unit), vec![st("done")]);
}

#[tokio::test]
async fn the_metric_labels_are_the_documented_values_only() {
    // inst-mt-labels: resource types `file` and `vector_store`, no identifier
    let unit = unit_cleanup(
        std_chat(vec![
            row(1).attempts(4),
            row(2),
            row(3).with_status("failed"),
        ])
        .with_store(Some("vs_1")),
    );
    unit.storage
        .queue(CallKind::Primary, Step::Fail(ProviderFailure::Other));
    unit.storage
        .queue(CallKind::VectorStore, Step::Fail(ProviderFailure::Other));
    run(&unit, 0).await;
    let mut seen: Vec<String> = unit.metrics.completed();
    seen.extend(unit.metrics.failed());
    seen.extend(unit.metrics.retries().into_iter().map(|(r, _)| r));
    for resource in seen {
        assert!(
            resource == "file" || resource == "vector_store",
            "{resource}"
        );
    }
    for (_, reason) in unit.metrics.retries() {
        assert!(
            reason == "provider_error" || reason == "vector_store_delete_failed",
            "{reason}"
        );
    }
}

#[test]
fn the_registry_fixture_resolves_the_store_provider_label() {
    // fixture check: the `azure_openai` label of the rows maps to its provider entry
    let registry: std::sync::Arc<ProviderRegistry> = crate::test_support::maintenance::providers();
    let entry = registry
        .entries()
        .find(|entry| entry.base.storage_backend == BACKEND)
        .expect("a provider serves the label");
    assert_eq!(entry.base.upstream_alias, AZURE_ALIAS);
}
