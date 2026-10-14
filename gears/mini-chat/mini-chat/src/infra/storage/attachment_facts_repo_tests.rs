//! `SQLite` tests of the two fact algorithms and the reads of the tool contributions: the ready
//! document count (`ready`, not deleted, `for_file_search` true; never the attachment kind), the
//! ready code interpreter files (`for_code_interpreter`, a provider file id, ordered by
//! `created_at` chronologically then `id`), the bound vector store of the chat and the citation
//! file list, each scoped to the tenant and the chat, and the exclusion of a deleted attachment
//! from every fact.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`
//! (`inst-rdf-*`), `-ready-code-interpreter-fact` (`inst-rci-*`), `-file-search-contribution`
//! (`inst-fs-vector-store`, `inst-fs-citation-map`) and `-delete-transaction`
//! (`inst-dtx-exclusion`); definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-tool-contributions`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use crate::domain::attachments::facts::{AttachmentFactsStore, ReadyFile};
use crate::domain::attachments::store::{AttachmentStore, DeleteOutcome, DeleteRequest};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{RowSeed, seed_row, seed_vector_store};
use crate::test_support::chats::{a1, at, seed_chat};
use crate::test_support::quota::{QuotaRig, STANDARD, quota_rig};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1};

async fn rig() -> (QuotaRig, ChatRecord) {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    (rig, chat)
}

fn facts(rig: &QuotaRig) -> &dyn AttachmentFactsStore {
    &*rig.db.provider
}

async fn documents(rig: &QuotaRig, chat: &ChatRecord) -> u32 {
    facts(rig)
        .ready_document_count(TENANT_A, chat.id)
        .await
        .unwrap()
}

async fn ci_ids(rig: &QuotaRig, chat: &ChatRecord) -> Vec<String> {
    facts(rig)
        .ready_code_interpreter_files(TENANT_A, chat.id)
        .await
        .unwrap()
        .into_iter()
        .map(|file| file.provider_file_id)
        .collect()
}

// ── the ready document fact ──────────────────────────────────────────────────

#[tokio::test]
async fn an_empty_chat_has_no_ready_document() {
    // inst-rdf-count, inst-rdf-return
    let (rig, chat) = rig().await;
    assert_eq!(documents(&rig, &chat).await, 0);
}

#[tokio::test]
async fn the_count_is_the_ready_non_deleted_file_search_rows() {
    // inst-rdf-count
    let (rig, chat) = rig().await;
    for _ in 0..3 {
        seed_row(&rig.db, &chat, RowSeed::document()).await;
    }
    for status in ["pending", "uploaded", "failed"] {
        seed_row(
            &rig.db,
            &chat,
            RowSeed::document().status(match status {
                "pending" => "pending",
                "uploaded" => "uploaded",
                _ => "failed",
            }),
        )
        .await;
    }
    seed_row(&rig.db, &chat, RowSeed::document().deleted(at(5))).await;
    assert_eq!(documents(&rig, &chat).await, 3);
}

#[tokio::test]
async fn an_xlsx_file_and_an_image_are_not_ready_documents() {
    // inst-rdf-not-kind: kind `document` with `for_file_search` false is not in the vector store
    let (rig, chat) = rig().await;
    seed_row(&rig.db, &chat, RowSeed::document().code_interpreter_only()).await;
    seed_row(&rig.db, &chat, RowSeed::image()).await;
    assert_eq!(documents(&rig, &chat).await, 0);
    seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert_eq!(documents(&rig, &chat).await, 1);
}

#[tokio::test]
async fn a_document_routed_to_both_tools_counts_once() {
    let (rig, chat) = rig().await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().for_code_interpreter(true),
    )
    .await;
    assert_eq!(documents(&rig, &chat).await, 1);
}

#[tokio::test]
async fn the_count_does_not_read_the_vector_store_table() {
    // inst-rdf-no-store: a ready row with `for_file_search` implies a bound store
    let (rig, chat) = rig().await;
    seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert!(
        rig.db
            .provider
            .bound_vector_store_id(TENANT_A, chat.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(documents(&rig, &chat).await, 1);
}

#[tokio::test]
async fn the_count_is_scoped_to_the_tenant_and_the_chat() {
    // inst-rdf-count: a foreign tenant or foreign chat row never counts
    let (rig, chat) = rig().await;
    let other = seed_chat(
        &rig.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        STANDARD,
        at(0),
    )
    .await;
    seed_row(&rig.db, &chat, RowSeed::document()).await;
    seed_row(&rig.db, &other, RowSeed::document()).await;
    seed_row(&rig.db, &other, RowSeed::document()).await;
    assert_eq!(documents(&rig, &chat).await, 1);
    assert_eq!(documents(&rig, &other).await, 2);
    assert_eq!(
        facts(&rig)
            .ready_document_count(TENANT_B, chat.id)
            .await
            .unwrap(),
        0,
        "the same chat id under another tenant"
    );
}

#[tokio::test]
async fn another_uploaders_ready_document_counts_for_the_chat() {
    // the fact is about the chat vector store, which holds every document of the chat
    let (rig, chat) = rig().await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().uploader(crate::test_support::USER_A2),
    )
    .await;
    assert_eq!(documents(&rig, &chat).await, 1);
    let _ = USER_A1;
}

// ── the ready code interpreter fact ──────────────────────────────────────────

#[tokio::test]
async fn the_ready_code_interpreter_files_are_listed_with_their_provider_ids() {
    // inst-rci-select, inst-rci-return
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .file_id("file-sheet")
            .created(at(1)),
    )
    .await;
    let found = facts(&rig)
        .ready_code_interpreter_files(TENANT_A, chat.id)
        .await
        .unwrap();
    assert_eq!(
        found,
        vec![ReadyFile {
            attachment_id: id,
            provider_file_id: "file-sheet".to_owned(),
            filename: "seed.pdf".to_owned(),
        }]
    );
}

#[tokio::test]
async fn a_file_search_only_document_is_not_a_code_interpreter_file() {
    // inst-rci-not-kind
    let (rig, chat) = rig().await;
    seed_row(&rig.db, &chat, RowSeed::document()).await;
    seed_row(&rig.db, &chat, RowSeed::image()).await;
    assert!(ci_ids(&rig, &chat).await.is_empty());
}

#[tokio::test]
async fn only_ready_non_deleted_rows_with_a_provider_file_are_listed() {
    // inst-rci-select: `ready`, `deleted_at IS NULL`, `for_code_interpreter`, non-null file id
    let (rig, chat) = rig().await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .file_id("file-ok"),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .status("uploaded")
            .file_id("file-uploaded"),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .status("failed")
            .file_id("file-failed"),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .deleted(at(3))
            .file_id("file-deleted"),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .no_provider_file(),
    )
    .await;
    assert_eq!(ci_ids(&rig, &chat).await, vec!["file-ok".to_owned()]);
}

#[tokio::test]
async fn the_list_is_ordered_by_created_at_then_id() {
    // inst-rci-select
    let (rig, chat) = rig().await;
    for (file, created) in [("file-c", at(30)), ("file-a", at(10)), ("file-b", at(20))] {
        seed_row(
            &rig.db,
            &chat,
            RowSeed::document()
                .code_interpreter_only()
                .file_id(file)
                .created(created),
        )
        .await;
    }
    assert_eq!(
        ci_ids(&rig, &chat).await,
        vec![
            "file-a".to_owned(),
            "file-b".to_owned(),
            "file-c".to_owned()
        ]
    );
}

#[tokio::test]
async fn the_order_is_chronological_for_fractional_seconds_of_different_lengths() {
    // implementation note 1: `...:00.12Z` is older than `...:00.123Z` although its text sorts
    // after; the stable tie-break is the id
    let (rig, chat) = rig().await;
    let base = at(100);
    let newer = base + time::Duration::milliseconds(123);
    let older = base + time::Duration::milliseconds(120);
    for (file, created) in [
        ("file-newer", newer),
        ("file-older", older),
        ("file-whole", base),
    ] {
        seed_row(
            &rig.db,
            &chat,
            RowSeed::document()
                .code_interpreter_only()
                .file_id(file)
                .created(created),
        )
        .await;
    }
    assert_eq!(
        ci_ids(&rig, &chat).await,
        vec![
            "file-whole".to_owned(),
            "file-older".to_owned(),
            "file-newer".to_owned()
        ]
    );
}

#[tokio::test]
async fn equal_creation_times_fall_back_to_the_id() {
    let (rig, chat) = rig().await;
    let mut seeded = Vec::new();
    for n in 0..4u128 {
        let mut seed = RowSeed::document().code_interpreter_only().created(at(7));
        seed.id = Uuid::from_u128(0xABC0 + n);
        seed.provider_file_id = Some(format!("file-{n}"));
        seeded.push(seed_row(&rig.db, &chat, seed).await);
    }
    let mut expected = seeded.clone();
    expected.sort();
    let found: Vec<Uuid> = facts(&rig)
        .ready_code_interpreter_files(TENANT_A, chat.id)
        .await
        .unwrap()
        .into_iter()
        .map(|file| file.attachment_id)
        .collect();
    assert_eq!(found, expected);
}

#[tokio::test]
async fn the_list_is_scoped_to_the_tenant_and_the_chat() {
    let (rig, chat) = rig().await;
    let other = seed_chat(
        &rig.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        STANDARD,
        at(0),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .file_id("file-mine"),
    )
    .await;
    seed_row(
        &rig.db,
        &other,
        RowSeed::document()
            .code_interpreter_only()
            .file_id("file-theirs"),
    )
    .await;
    assert_eq!(ci_ids(&rig, &chat).await, vec!["file-mine".to_owned()]);
    assert!(
        facts(&rig)
            .ready_code_interpreter_files(TENANT_B, chat.id)
            .await
            .unwrap()
            .is_empty()
    );
}

// ── the vector store ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bound_store_is_returned_and_a_placeholder_or_a_missing_row_is_not() {
    // inst-fs-vector-store: a non-NULL `vector_store_id` by tenant and chat
    let (rig, chat) = rig().await;
    assert_eq!(
        facts(&rig)
            .bound_vector_store_id(TENANT_A, chat.id)
            .await
            .unwrap(),
        None
    );
    let other = seed_chat(
        &rig.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        STANDARD,
        at(0),
    )
    .await;
    seed_vector_store(&rig.db, &other, None, "azure_openai", at(0)).await;
    assert_eq!(
        facts(&rig)
            .bound_vector_store_id(TENANT_A, other.id)
            .await
            .unwrap(),
        None,
        "a placeholder"
    );
    seed_vector_store(&rig.db, &chat, Some("vs_chat"), "azure_openai", at(0)).await;
    assert_eq!(
        facts(&rig)
            .bound_vector_store_id(TENANT_A, chat.id)
            .await
            .unwrap(),
        Some("vs_chat".to_owned())
    );
    assert_eq!(
        facts(&rig)
            .bound_vector_store_id(TENANT_B, chat.id)
            .await
            .unwrap(),
        None,
        "another tenant"
    );
}

// ── the citation files ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_citation_files_are_the_ready_live_rows_with_a_provider_file_of_every_kind() {
    // inst-fs-citation-map: ready, non-deleted attachments with a non-null `provider_file_id`
    let (rig, chat) = rig().await;
    let doc = seed_row(&rig.db, &chat, RowSeed::document().file_id("file-doc")).await;
    let sheet = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .file_id("file-sheet"),
    )
    .await;
    let image = seed_row(&rig.db, &chat, RowSeed::image().file_id("file-img")).await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("uploaded").file_id("file-up"),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().deleted(at(2)).file_id("file-del"),
    )
    .await;
    seed_row(&rig.db, &chat, RowSeed::document().no_provider_file()).await;
    let mut found: Vec<(Uuid, String)> = facts(&rig)
        .citation_files(TENANT_A, chat.id)
        .await
        .unwrap()
        .into_iter()
        .map(|file| (file.attachment_id, file.provider_file_id))
        .collect();
    found.sort();
    let mut expected = vec![
        (doc, "file-doc".to_owned()),
        (sheet, "file-sheet".to_owned()),
        (image, "file-img".to_owned()),
    ];
    expected.sort();
    assert_eq!(found, expected);
}

#[tokio::test]
async fn the_citation_files_carry_the_filename() {
    let (rig, chat) = rig().await;
    let mut seed = RowSeed::document().file_id("file-named");
    seed.filename = "annual report.pdf".to_owned();
    seed_row(&rig.db, &chat, seed).await;
    let found = facts(&rig).citation_files(TENANT_A, chat.id).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].filename, "annual report.pdf");
}

#[tokio::test]
async fn the_citation_files_are_scoped_to_the_tenant_and_the_chat() {
    let (rig, chat) = rig().await;
    let other = seed_chat(
        &rig.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        STANDARD,
        at(0),
    )
    .await;
    seed_row(&rig.db, &chat, RowSeed::document().file_id("file-mine")).await;
    seed_row(&rig.db, &other, RowSeed::document().file_id("file-theirs")).await;
    let found = facts(&rig).citation_files(TENANT_A, chat.id).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].provider_file_id, "file-mine");
    assert!(
        facts(&rig)
            .citation_files(TENANT_B, chat.id)
            .await
            .unwrap()
            .is_empty()
    );
}

// ── the exclusion of a deleted attachment ────────────────────────────────────

#[tokio::test]
async fn a_deleted_attachment_leaves_every_fact_at_once() {
    // inst-dtx-exclusion, inst-dtx-return-deleted: "from the commit on, exclude the attachment
    // from the `file_search` inclusion check and from citations"
    let (rig, chat) = rig().await;
    let doc = seed_row(&rig.db, &chat, RowSeed::document().file_id("file-doc")).await;
    let sheet = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .code_interpreter_only()
            .file_id("file-sheet"),
    )
    .await;
    assert_eq!(documents(&rig, &chat).await, 1);
    assert_eq!(ci_ids(&rig, &chat).await.len(), 1);
    assert_eq!(
        facts(&rig)
            .citation_files(TENANT_A, chat.id)
            .await
            .unwrap()
            .len(),
        2
    );
    for id in [doc, sheet] {
        let request = DeleteRequest {
            tenant_id: TENANT_A,
            chat_id: chat.id,
            attachment_id: id,
            uploader_id: USER_A1,
            now: at(1_000),
            secondary_upstream_alias: None,
        };
        let outcome = (&*rig.db.provider as &dyn AttachmentStore)
            .delete_attachment(&request, rig.enqueuer.clone())
            .await
            .unwrap();
        assert_eq!(outcome, DeleteOutcome::Deleted);
    }
    assert_eq!(documents(&rig, &chat).await, 0);
    assert!(ci_ids(&rig, &chat).await.is_empty());
    assert!(
        facts(&rig)
            .citation_files(TENANT_A, chat.id)
            .await
            .unwrap()
            .is_empty()
    );
}
