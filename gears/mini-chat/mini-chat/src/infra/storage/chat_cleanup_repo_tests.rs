//! The chat-cleanup repository over in-memory `SQLite`: the chat read without the deleted filter,
//! the `pending` attachment rows of a deleted chat (soft-deleted attachments included, ordered by
//! `created_at` then `id`, tenant and chat isolation), the pending count and the failed check, the
//! guarded cleanup writes (`done`, the attempt increment with `last_cleanup_error` and
//! `cleanup_updated_at`, `failed` at the budget in the same write, a lost guard), and the
//! `chat_vector_stores` row reads and deletions (bound row, creation placeholder guarded by
//! `vector_store_id IS NULL`, tenant and chat scoping), with the database errors surfaced.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithms
//! `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler` (`inst-cc-load-chat`,
//! `inst-cc-load-pending`, `inst-cc-count-pending`, `inst-cc-idempotent`),
//! `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file` (`inst-af-mark-done`, `inst-af-record-attempt`, `inst-af-exhausted`,
//! `inst-af-guard-lost`, `inst-af-no-terminal`) and `cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup` (`inst-vs-load`,
//! `inst-vs-failed-rows`, `inst-vs-placeholder`, `inst-vs-return-placeholder`,
//! `inst-vs-delete-row`); the transitions `inst-cs-done`, `inst-cs-retry` and `inst-cs-failed` of
//! `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status` and `inst-vsm-chat-cleanup`
//! of `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`; state
//! `cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge` (`inst-pp-files-retry`,
//! `inst-pp-files-done`, `inst-pp-files-purged`, `inst-pp-files-debt`, `inst-pp-store-purged`,
//! `inst-pp-store-debt`, `inst-pp-debt-terminal`); algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-retention` (`inst-rt-no-purge`,
//! `inst-rt-purged`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`,
//! `cpt-cf-mini-chat-dod-background-maintenance-vector-store-ordering` and
//! `cpt-cf-mini-chat-dod-background-maintenance-retention`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use crate::domain::maintenance::chat_store::ChatCleanupStore;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{RowSeed, seed_row, soft_delete_chat};
use crate::test_support::chats::{at, attachment_rows};
use crate::test_support::maintenance::{MaintRig, maint_rig, seed_store, table_count};
use crate::test_support::quota::default_now;
use crate::test_support::{TENANT_A, TENANT_B};

async fn seeded() -> (MaintRig, ChatRecord) {
    let rig = maint_rig().await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    (rig, chat)
}

async fn pending_ids(rig: &MaintRig, chat: &ChatRecord) -> Vec<Uuid> {
    rig.db()
        .provider
        .pending_attachments(chat.tenant_id, chat.id)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect()
}

async fn row(
    rig: &MaintRig,
    chat: &ChatRecord,
    id: Uuid,
) -> crate::infra::storage::entity::attachment::Model {
    attachment_rows(rig.db(), chat.id)
        .await
        .into_iter()
        .find(|row| row.id == id)
        .expect("the row exists")
}

// ── the chat ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_live_chat_reads_its_model_and_is_not_deleted() {
    // inst-cc-load-chat
    let (rig, chat) = seeded().await;
    let facts = rig
        .db()
        .provider
        .load_chat(chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(facts.model, crate::test_support::quota::STANDARD);
    assert!(!facts.deleted);
}

#[tokio::test]
async fn a_soft_deleted_chat_is_read_without_the_deleted_filter() {
    // inst-cc-load-chat: the model chosen at creation, `deleted_at` set
    let (rig, chat) = seeded().await;
    soft_delete_chat(rig.db(), chat.id, at(50)).await;
    let facts = rig
        .db()
        .provider
        .load_chat(chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert!(facts.deleted);
    assert_eq!(facts.model, crate::test_support::quota::STANDARD);
}

#[tokio::test]
async fn a_missing_chat_or_a_foreign_tenant_reads_nothing() {
    // inst-cc-load-chat: filter `tenant_id` and `chat_id`
    let (rig, chat) = seeded().await;
    assert!(
        rig.db()
            .provider
            .load_chat(chat.tenant_id, Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        rig.db()
            .provider
            .load_chat(TENANT_B, chat.id)
            .await
            .unwrap()
            .is_none()
    );
}

// ── the pending rows ─────────────────────────────────────────────────────────

#[tokio::test]
async fn only_pending_rows_are_loaded_soft_deleted_ones_included() {
    // inst-cc-load-pending: `cleanup_status` pending; terminal and unset rows are skipped
    let (rig, chat) = seeded().await;
    let pending = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    let deleted = seed_row(
        rig.db(),
        &chat,
        RowSeed::image().cleanup("pending").deleted(at(5)),
    )
    .await;
    seed_row(rig.db(), &chat, RowSeed::document().cleanup("done")).await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("failed").attempts(5),
    )
    .await;
    seed_row(rig.db(), &chat, RowSeed::document()).await;
    let mut found = pending_ids(&rig, &chat).await;
    found.sort();
    let mut expected = vec![pending, deleted];
    expected.sort();
    assert_eq!(found, expected);
}

#[tokio::test]
async fn the_pending_row_carries_the_columns_the_handler_needs() {
    // inst-cc-load-pending: the file id, the backend, the attempts and the secondary columns
    let (rig, chat) = seeded().await;
    let id = seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .file_id("file-xyz")
            .backend("azure-rag-label")
            .attempts(3)
            .secondary("file_ant_1", "uploaded"),
    )
    .await;
    let loaded = rig
        .db()
        .provider
        .pending_attachments(chat.tenant_id, chat.id)
        .await
        .unwrap();
    assert_eq!(loaded.len(), 1);
    let row = &loaded[0];
    assert_eq!(row.id, id);
    assert_eq!(row.provider_file_id.as_deref(), Some("file-xyz"));
    assert_eq!(row.storage_backend, "azure-rag-label");
    assert_eq!(row.cleanup_attempts, 3);
    assert_eq!(row.secondary_file_id.as_deref(), Some("file_ant_1"));
    assert_eq!(row.secondary_status, "uploaded");
    assert_eq!(row.secondary_provider_kind.as_deref(), Some("anthropic"));
}

#[tokio::test]
async fn a_row_without_a_provider_file_is_loaded_with_none() {
    // inst-af-no-file: the upload never reached the provider
    let (rig, chat) = seeded().await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("pending").no_provider_file(),
    )
    .await;
    let loaded = rig
        .db()
        .provider
        .pending_attachments(chat.tenant_id, chat.id)
        .await
        .unwrap();
    assert_eq!(loaded[0].provider_file_id, None);
}

#[tokio::test]
async fn the_rows_are_ordered_by_created_at_then_id() {
    // inst-cc-load-pending: fractional seconds of different lengths sort chronologically
    let (rig, chat) = seeded().await;
    let later = seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .created(at(10) + time::Duration::microseconds(123_000)),
    )
    .await;
    let earlier = seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .created(at(10) + time::Duration::microseconds(120_000)),
    )
    .await;
    let middle = seed_row(
        rig.db(),
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .created(at(10) + time::Duration::microseconds(121_500)),
    )
    .await;
    let first = seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("pending").created(at(9)),
    )
    .await;
    assert_eq!(
        pending_ids(&rig, &chat).await,
        vec![first, earlier, middle, later]
    );
}

#[tokio::test]
async fn equal_creation_instants_are_ordered_by_id() {
    // inst-cc-load-pending: the deterministic tie-break
    let (rig, chat) = seeded().await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            seed_row(
                rig.db(),
                &chat,
                RowSeed::document().cleanup("pending").created(at(7)),
            )
            .await,
        );
    }
    ids.sort();
    assert_eq!(pending_ids(&rig, &chat).await, ids);
}

#[tokio::test]
async fn rows_of_another_chat_or_tenant_are_not_loaded() {
    // inst-cc-load-pending: filter `tenant_id` and `chat_id`
    let (rig, chat) = seeded().await;
    let mine = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    let other = rig.chat(crate::test_support::quota::STANDARD).await;
    seed_row(rig.db(), &other, RowSeed::document().cleanup("pending")).await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("pending").tenant(TENANT_B),
    )
    .await;
    assert_eq!(pending_ids(&rig, &chat).await, vec![mine]);
    assert!(
        rig.db()
            .provider
            .pending_attachments(TENANT_B, Uuid::new_v4())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_pending_count_follows_the_pending_rows() {
    // inst-cc-count-pending
    let (rig, chat) = seeded().await;
    let count = |rig: &MaintRig, chat: &ChatRecord| {
        let provider = rig.db().provider.clone();
        let (tenant, id) = (chat.tenant_id, chat.id);
        async move { provider.count_pending(tenant, id).await.unwrap() }
    };
    assert_eq!(count(&rig, &chat).await, 0);
    let a = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    seed_row(
        rig.db(),
        &chat,
        RowSeed::image().cleanup("pending").deleted(at(3)),
    )
    .await;
    seed_row(rig.db(), &chat, RowSeed::document().cleanup("done")).await;
    seed_row(rig.db(), &chat, RowSeed::document().cleanup("failed")).await;
    assert_eq!(count(&rig, &chat).await, 2);
    rig.db()
        .provider
        .mark_done(chat.tenant_id, a, default_now())
        .await
        .unwrap();
    assert_eq!(count(&rig, &chat).await, 1);
}

#[tokio::test]
async fn the_failed_check_sees_failed_rows_only() {
    // inst-vs-failed-rows
    let (rig, chat) = seeded().await;
    let has_failed = || {
        let provider = rig.db().provider.clone();
        let (tenant, id) = (chat.tenant_id, chat.id);
        async move { provider.has_failed(tenant, id).await.unwrap() }
    };
    assert!(!has_failed().await);
    seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    seed_row(rig.db(), &chat, RowSeed::document().cleanup("done")).await;
    assert!(!has_failed().await);
    seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("failed").attempts(5),
    )
    .await;
    assert!(has_failed().await);
}

// ── the guarded writes ───────────────────────────────────────────────────────

#[tokio::test]
async fn done_is_written_with_the_instant_and_only_for_a_pending_row() {
    // inst-af-mark-done, inst-cs-done: guarded by `cleanup_status` pending
    let (rig, chat) = seeded().await;
    let id = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    assert!(
        rig.db()
            .provider
            .mark_done(chat.tenant_id, id, default_now())
            .await
            .unwrap()
    );
    let done = row(&rig, &chat, id).await;
    assert_eq!(done.cleanup_status.as_deref(), Some("done"));
    assert_eq!(done.cleanup_updated_at, Some(default_now()));
    assert_eq!(done.cleanup_attempts, 0);
    // the guard: a second write matches no row
    assert!(
        !rig.db()
            .provider
            .mark_done(chat.tenant_id, id, at(900))
            .await
            .unwrap()
    );
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_updated_at,
        Some(default_now())
    );
}

#[tokio::test]
async fn done_never_overwrites_a_failed_row_or_a_foreign_tenant_row() {
    // inst-af-no-terminal, inst-af-guard-lost: `failed` is terminal; the tenant guard holds
    let (rig, chat) = seeded().await;
    let failed = seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("failed").attempts(5),
    )
    .await;
    assert!(
        !rig.db()
            .provider
            .mark_done(chat.tenant_id, failed, default_now())
            .await
            .unwrap()
    );
    assert_eq!(
        row(&rig, &chat, failed).await.cleanup_status.as_deref(),
        Some("failed")
    );
    let pending = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    assert!(
        !rig.db()
            .provider
            .mark_done(TENANT_B, pending, default_now())
            .await
            .unwrap()
    );
    assert_eq!(
        row(&rig, &chat, pending).await.cleanup_status.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn an_attempt_increments_records_the_cause_and_keeps_the_row_pending() {
    // inst-af-record-attempt, inst-cs-retry
    let (rig, chat) = seeded().await;
    let id = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    let record = rig
        .db()
        .provider
        .record_failed_attempt(
            chat.tenant_id,
            id,
            "the provider rejected the delete",
            5,
            default_now(),
        )
        .await
        .unwrap()
        .expect("the guard matched");
    assert_eq!((record.attempts, record.exhausted), (1, false));
    let after = row(&rig, &chat, id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(after.cleanup_attempts, 1);
    assert_eq!(
        after.last_cleanup_error.as_deref(),
        Some("the provider rejected the delete")
    );
    assert_eq!(after.cleanup_updated_at, Some(default_now()));
}

#[tokio::test]
async fn the_attempt_that_reaches_the_budget_makes_the_row_failed_in_the_same_write() {
    // inst-af-exhausted, inst-cs-failed
    let (rig, chat) = seeded().await;
    let id = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    for expected in 1..=4 {
        let record = rig
            .db()
            .provider
            .record_failed_attempt(chat.tenant_id, id, "cause", 5, default_now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((record.attempts, record.exhausted), (expected, false));
    }
    let last = rig
        .db()
        .provider
        .record_failed_attempt(chat.tenant_id, id, "last cause", 5, default_now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((last.attempts, last.exhausted), (5, true));
    let after = row(&rig, &chat, id).await;
    assert_eq!(after.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(after.cleanup_attempts, 5);
    assert_eq!(after.last_cleanup_error.as_deref(), Some("last cause"));
}

#[tokio::test]
async fn an_attempt_on_a_row_that_is_not_pending_matches_nothing() {
    // inst-af-guard-lost, inst-af-no-terminal: the guard is `cleanup_status` pending
    let (rig, chat) = seeded().await;
    for status in ["done", "failed"] {
        let id = seed_row(
            rig.db(),
            &chat,
            RowSeed::document().cleanup(status).attempts(2),
        )
        .await;
        let result = rig
            .db()
            .provider
            .record_failed_attempt(chat.tenant_id, id, "cause", 5, default_now())
            .await
            .unwrap();
        assert!(result.is_none(), "{status}");
        let after = row(&rig, &chat, id).await;
        assert_eq!(after.cleanup_attempts, 2, "{status}");
        assert_eq!(after.last_cleanup_error, None, "{status}");
    }
    let foreign = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    assert!(
        rig.db()
            .provider
            .record_failed_attempt(TENANT_B, foreign, "c", 5, default_now())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_row_with_prior_attempts_fails_at_the_limit_counting_them() {
    // inst-af-exhausted: `cleanup_attempts` counts across deliveries
    let (rig, chat) = seeded().await;
    let id = seed_row(
        rig.db(),
        &chat,
        RowSeed::document().cleanup("pending").attempts(4),
    )
    .await;
    let record = rig
        .db()
        .provider
        .record_failed_attempt(chat.tenant_id, id, "cause", 5, default_now())
        .await
        .unwrap()
        .unwrap();
    assert!(record.exhausted);
    assert_eq!(
        row(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("failed")
    );
}

// ── the vector store row ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_vector_store_row_is_read_with_its_id_and_provider_label() {
    // inst-vs-load
    let (rig, chat) = seeded().await;
    assert!(
        rig.db()
            .provider
            .vector_store(chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_none()
    );
    let id = seed_store(rig.db(), &chat, Some("vs_abc")).await;
    let found = rig
        .db()
        .provider
        .vector_store(chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.vector_store_id.as_deref(), Some("vs_abc"));
    assert_eq!(found.provider, crate::test_support::maintenance::BACKEND);
}

#[tokio::test]
async fn a_placeholder_row_reads_with_a_null_store_id() {
    // inst-vs-placeholder: a creation placeholder
    let (rig, chat) = seeded().await;
    seed_store(rig.db(), &chat, None).await;
    let found = rig
        .db()
        .provider
        .vector_store(chat.tenant_id, chat.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.vector_store_id, None);
}

#[tokio::test]
async fn the_store_row_is_scoped_by_tenant_and_chat() {
    // inst-vs-load: filter `tenant_id` and `chat_id`
    let (rig, chat) = seeded().await;
    seed_store(rig.db(), &chat, Some("vs_abc")).await;
    assert!(
        rig.db()
            .provider
            .vector_store(TENANT_B, chat.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        rig.db()
            .provider
            .vector_store(TENANT_A, Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_bound_row_is_deleted_by_tenant_chat_and_id() {
    // inst-vs-delete-row, inst-vsm-chat-cleanup: the completion marker
    let (rig, chat) = seeded().await;
    let id = seed_store(rig.db(), &chat, Some("vs_abc")).await;
    assert!(
        rig.db()
            .provider
            .delete_vector_store_row(chat.tenant_id, chat.id, id, false)
            .await
            .unwrap()
    );
    assert!(
        rig.db()
            .provider
            .vector_store(chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !rig.db()
            .provider
            .delete_vector_store_row(chat.tenant_id, chat.id, id, false)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_placeholder_delete_is_guarded_by_a_null_store_id() {
    // inst-vs-return-placeholder: a bound row is not removed by the placeholder path; an upload
    // that created the store in the meantime loses its compare-and-set and deletes the store
    let (rig, chat) = seeded().await;
    let bound = seed_store(rig.db(), &chat, Some("vs_abc")).await;
    assert!(
        !rig.db()
            .provider
            .delete_vector_store_row(chat.tenant_id, chat.id, bound, true)
            .await
            .unwrap()
    );
    assert!(
        rig.db()
            .provider
            .vector_store(chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_some()
    );
    let (rig, chat) = seeded().await;
    let placeholder = seed_store(rig.db(), &chat, None).await;
    assert!(
        rig.db()
            .provider
            .delete_vector_store_row(chat.tenant_id, chat.id, placeholder, true)
            .await
            .unwrap()
    );
    assert!(
        rig.db()
            .provider
            .vector_store(chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_row_delete_for_another_tenant_chat_or_id_matches_nothing() {
    // inst-vs-delete-row: the row is addressed by `tenant_id`, `chat_id` and `id`
    let (rig, chat) = seeded().await;
    let id = seed_store(rig.db(), &chat, Some("vs_abc")).await;
    assert!(
        !rig.db()
            .provider
            .delete_vector_store_row(TENANT_B, chat.id, id, false)
            .await
            .unwrap()
    );
    assert!(
        !rig.db()
            .provider
            .delete_vector_store_row(chat.tenant_id, Uuid::new_v4(), id, false)
            .await
            .unwrap()
    );
    assert!(
        !rig.db()
            .provider
            .delete_vector_store_row(chat.tenant_id, chat.id, Uuid::new_v4(), false)
            .await
            .unwrap()
    );
    assert!(
        rig.db()
            .provider
            .vector_store(chat.tenant_id, chat.id)
            .await
            .unwrap()
            .is_some()
    );
}

// ── retention and errors ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_only_row_the_cleanup_deletes_is_the_vector_store_row() {
    // inst-rt-no-purge, inst-rt-purged, inst-rt-return: chat, messages and attachments stay in the
    // database
    let (rig, chat) = seeded().await;
    crate::test_support::maintenance::seed_range(rig.db(), &chat, 4).await;
    let id = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    let store = seed_store(rig.db(), &chat, Some("vs_abc")).await;
    soft_delete_chat(rig.db(), chat.id, at(50)).await;
    let (chats, messages, attachments) = (
        table_count(rig.db(), "chats").await,
        table_count(rig.db(), "messages").await,
        table_count(rig.db(), "attachments").await,
    );
    rig.db()
        .provider
        .mark_done(chat.tenant_id, id, default_now())
        .await
        .unwrap();
    rig.db()
        .provider
        .delete_vector_store_row(chat.tenant_id, chat.id, store, false)
        .await
        .unwrap();
    assert_eq!(table_count(rig.db(), "chats").await, chats);
    assert_eq!(table_count(rig.db(), "messages").await, messages);
    assert_eq!(table_count(rig.db(), "attachments").await, attachments);
    assert_eq!(table_count(rig.db(), "chat_vector_stores").await, 0);
    assert!(
        row(&rig, &chat, id).await.deleted_at.is_none(),
        "the child row keeps `deleted_at` empty"
    );
}

#[tokio::test]
async fn database_errors_are_reported_by_every_read_and_write() {
    // inst-cc-chat-db, inst-cc-pending-db, inst-vs-load-db, inst-af-db: errors surface so that the
    // handler can retry without consuming a budget
    let (rig, chat) = seeded().await;
    let id = seed_row(rig.db(), &chat, RowSeed::document().cleanup("pending")).await;
    let store = seed_store(rig.db(), &chat, Some("vs_abc")).await;
    crate::test_support::raw_exec(
        &rig.db().raw,
        "ALTER TABLE attachments RENAME TO attachments_gone",
    )
    .await
    .unwrap();
    crate::test_support::raw_exec(
        &rig.db().raw,
        "ALTER TABLE chat_vector_stores RENAME TO chat_vector_stores_gone",
    )
    .await
    .unwrap();
    let p = &rig.db().provider;
    assert!(
        p.pending_attachments(chat.tenant_id, chat.id)
            .await
            .is_err()
    );
    assert!(p.count_pending(chat.tenant_id, chat.id).await.is_err());
    assert!(p.has_failed(chat.tenant_id, chat.id).await.is_err());
    assert!(
        p.mark_done(chat.tenant_id, id, default_now())
            .await
            .is_err()
    );
    assert!(
        p.record_failed_attempt(chat.tenant_id, id, "c", 5, default_now())
            .await
            .is_err()
    );
    assert!(p.vector_store(chat.tenant_id, chat.id).await.is_err());
    assert!(
        p.delete_vector_store_row(chat.tenant_id, chat.id, store, false)
            .await
            .is_err()
    );
    // the chat table is untouched
    assert!(p.load_chat(chat.tenant_id, chat.id).await.is_ok());
}
