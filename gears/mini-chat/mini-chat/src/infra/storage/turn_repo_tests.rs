//! Repository tests of the turn and attachment reads over in-memory `SQLite`: the idempotency
//! key lookup (soft-deleted rows included), the running-turn check, the status lookup and the
//! preflight attachment classification.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `turn-admission-checks`, `turn-status`, `send-preflight` (steps 4 and 5); definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-idempotency` and
//! `cpt-cf-mini-chat-dod-message-streaming-turn-status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

use super::{AttachmentRepo, TurnRepo};
use crate::domain::turns::TurnState;
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, at, seed_chat};
use crate::test_support::send::{AttachmentSeed, TurnSeedFull, seed_att, seed_turn_full, t};
use crate::test_support::{TENANT_B, TestDb, USER_A2, sqlite_db};

async fn setup() -> (TestDb, ChatRecord) {
    let db = sqlite_db(Migrator::migrations()).await;
    let chat = seed_chat(&db, &a1(), Uuid::new_v4(), Some("chat"), "m", at(0)).await;
    (db, chat)
}

// ── the idempotency key lookup ───────────────────────────────────────────────

#[tokio::test]
async fn the_key_lookup_returns_the_turn_of_the_chat_in_every_state() {
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    for state in ["completed", "failed", "cancelled", "running"] {
        let seed = TurnSeedFull::new(state);
        let request_id = seed.request_id;
        let id = seed_turn_full(&db, &chat, seed).await;
        let row = TurnRepo::find_by_key(&conn, &chat, request_id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{state}: no row"));
        assert_eq!(row.id, id);
        assert_eq!(row.request_id, request_id);
        assert_eq!(row.chat_id, chat.id);
        assert_eq!(row.state, TurnState::parse(state).unwrap());
        assert_eq!(row.deleted_at, None);
    }
}

#[tokio::test]
async fn the_key_lookup_includes_soft_deleted_rows_with_their_deletion_time() {
    let (db, chat) = setup().await;
    let seed = TurnSeedFull::new("completed").deleted(t(5));
    let request_id = seed.request_id;
    seed_turn_full(&db, &chat, seed).await;
    let conn = db.provider.conn().unwrap();
    let row = TurnRepo::find_by_key(&conn, &chat, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.deleted_at, Some(t(5)));
    // the status lookup hides it
    assert_eq!(
        TurnRepo::find_active(&conn, &chat, request_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn the_key_is_scoped_to_the_chat() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let seed = TurnSeedFull::new("completed");
    let request_id = seed.request_id;
    seed_turn_full(&db, &other, seed).await;
    let conn = db.provider.conn().unwrap();
    assert_eq!(
        TurnRepo::find_by_key(&conn, &chat, request_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        TurnRepo::find_by_key(&conn, &chat, Uuid::new_v4())
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn the_row_carries_the_columns_the_replay_and_the_status_need() {
    let (db, chat) = setup().await;
    let assistant = Uuid::new_v4();
    let seed = TurnSeedFull::new("failed")
        .error("provider_error")
        .model("gpt-5.2")
        .updated(t(40));
    let request_id = seed.request_id;
    seed_turn_full(&db, &chat, seed).await;
    let conn = db.provider.conn().unwrap();
    let row = TurnRepo::find_active(&conn, &chat, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.error_code.as_deref(), Some("provider_error"));
    assert_eq!(row.effective_model.as_deref(), Some("gpt-5.2"));
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(row.updated_at, t(40));
    assert_eq!(row.started_at, t(0));
    assert!(row.requester_user_id.is_some());

    let seed = TurnSeedFull::new("completed").assistant(assistant);
    let request_id = seed.request_id;
    seed_turn_full(&db, &chat, seed).await;
    let row = TurnRepo::find_active(&conn, &chat, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.assistant_message_id, Some(assistant));
}

// ── the running-turn check ───────────────────────────────────────────────────

#[tokio::test]
async fn a_running_turn_of_the_chat_is_detected_whatever_its_request_id() {
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    assert!(!TurnRepo::has_running(&conn, &chat).await.unwrap());
    seed_turn_full(&db, &chat, TurnSeedFull::new("running")).await;
    assert!(TurnRepo::has_running(&conn, &chat).await.unwrap());
}

#[tokio::test]
async fn terminal_deleted_and_foreign_turns_do_not_count_as_running() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    for state in ["completed", "failed", "cancelled"] {
        seed_turn_full(&db, &chat, TurnSeedFull::new(state)).await;
    }
    seed_turn_full(&db, &chat, TurnSeedFull::new("running").deleted(t(3))).await;
    seed_turn_full(&db, &other, TurnSeedFull::new("running")).await;
    let conn = db.provider.conn().unwrap();
    assert!(!TurnRepo::has_running(&conn, &chat).await.unwrap());
    assert!(TurnRepo::has_running(&conn, &other).await.unwrap());
}

// ── the preflight attachment reads ───────────────────────────────────────────

#[tokio::test]
async fn referenced_attachments_are_classified_in_request_order() {
    let (db, chat) = setup().await;
    let image = seed_att(&db, &chat, AttachmentSeed::ready("image")).await;
    let document = seed_att(&db, &chat, AttachmentSeed::ready("document")).await;
    let pending_image =
        seed_att(&db, &chat, AttachmentSeed::ready("image").status("pending")).await;
    let conn = db.provider.conn().unwrap();
    let facts = AttachmentRepo::referenced(&conn, &chat, &[document, image, pending_image])
        .await
        .unwrap();
    let ids: Vec<Uuid> = facts.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![document, image, pending_image]);
    assert_eq!(facts[0].kind, "document");
    assert_eq!(facts[1].kind, "image");
    assert!(
        facts[1]
            .provider_file_id
            .as_deref()
            .is_some_and(|id| id.starts_with("file-"))
    );
}

#[tokio::test]
async fn unknown_deleted_and_other_chat_attachments_are_not_classified() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let mine = seed_att(&db, &chat, AttachmentSeed::ready("image")).await;
    let deleted = seed_att(&db, &chat, AttachmentSeed::ready("image").deleted(t(2))).await;
    let elsewhere = seed_att(&db, &other, AttachmentSeed::ready("image")).await;
    let foreign_tenant =
        seed_att(&db, &chat, AttachmentSeed::ready("image").tenant(TENANT_B)).await;
    let conn = db.provider.conn().unwrap();
    let facts = AttachmentRepo::referenced(
        &conn,
        &chat,
        &[mine, deleted, elsewhere, foreign_tenant, Uuid::new_v4()],
    )
    .await
    .unwrap();
    assert_eq!(facts.iter().map(|f| f.id).collect::<Vec<_>>(), vec![mine]);
    assert!(
        AttachmentRepo::referenced(&conn, &chat, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_chat_facts_count_ready_documents_and_ready_code_interpreter_attachments() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let conn = db.provider.conn().unwrap();
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (0, 0)
    );

    seed_att(&db, &chat, AttachmentSeed::ready("document")).await;
    seed_att(&db, &chat, AttachmentSeed::ready("document")).await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").status("uploaded"),
    )
    .await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").status("failed"),
    )
    .await;
    seed_att(&db, &chat, AttachmentSeed::ready("document").deleted(t(1))).await;
    seed_att(&db, &chat, AttachmentSeed::ready("image")).await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").code_interpreter(),
    )
    .await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .status("pending"),
    )
    .await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .deleted(t(1)),
    )
    .await;
    seed_att(
        &db,
        &other,
        AttachmentSeed::ready("document").code_interpreter(),
    )
    .await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").uploaded_by(USER_A2),
    )
    .await;

    // ready, non-deleted documents: 2 + 1 (code interpreter) + 1 (other uploader) = 4;
    // ready, non-deleted code interpreter attachments: 1
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (4, 1)
    );
}

// ── slice S2 of FEATURE 0005: the chat facts follow the two shared predicates ──────────────────

#[tokio::test]
async fn an_xlsx_only_chat_has_no_ready_document_and_one_code_interpreter_attachment() {
    // inst-rdf-not-kind, inst-rci-select, inst-ask-facts: kind `document`, `for_file_search`
    // false, `for_code_interpreter` true
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .no_file_search(),
    )
    .await;
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (0, 1)
    );
}

#[tokio::test]
async fn a_ready_pdf_is_a_ready_document_and_not_a_code_interpreter_attachment() {
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    seed_att(&db, &chat, AttachmentSeed::ready("document")).await;
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (1, 0)
    );
}

#[tokio::test]
async fn the_facts_of_a_mixed_chat_count_each_predicate_once_per_row() {
    // a PDF, an XLSX, a document routed to both tools, an image and unready or deleted rows
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    seed_att(&db, &chat, AttachmentSeed::ready("document")).await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .no_file_search(),
    )
    .await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").code_interpreter(),
    )
    .await;
    seed_att(&db, &chat, AttachmentSeed::ready("image")).await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document")
            .no_file_search()
            .status("uploaded"),
    )
    .await;
    seed_att(&db, &chat, AttachmentSeed::ready("document").deleted(t(1))).await;
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (2, 2)
    );
}

#[tokio::test]
async fn an_image_and_an_unflagged_document_are_not_ready_documents() {
    // inst-rdf-not-kind: an image has both flags false, and a document without `for_file_search`
    // is not in the vector store
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    seed_att(&db, &chat, AttachmentSeed::ready("image")).await;
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").no_file_search(),
    )
    .await;
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (0, 0)
    );
}

#[tokio::test]
async fn a_code_interpreter_attachment_without_a_provider_file_id_is_not_counted() {
    // inst-rci-select: a non-null `provider_file_id`
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .no_file_search()
            .no_provider_file(),
    )
    .await;
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (0, 0)
    );
}

#[tokio::test]
async fn the_chat_facts_are_scoped_to_the_chat_and_the_tenant() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let conn = db.provider.conn().unwrap();
    seed_att(
        &db,
        &other,
        AttachmentSeed::ready("document")
            .code_interpreter()
            .no_file_search(),
    )
    .await;
    seed_att(&db, &other, AttachmentSeed::ready("document")).await;
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &chat).await.unwrap(),
        (0, 0)
    );
    assert_eq!(
        AttachmentRepo::chat_facts(&conn, &other).await.unwrap(),
        (1, 1)
    );
    let _ = TENANT_B;
}
