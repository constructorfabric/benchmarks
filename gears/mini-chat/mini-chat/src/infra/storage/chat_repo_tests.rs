//! Repository tests of the chat-management writes and reads over in-memory `SQLite`: the
//! `ChatDetail` projection, `message_count`, the list page query, the title write, the activity
//! bump of the message-activity contract and the two steps of the delete transaction.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: algorithms `message-count`,
//! `chat-detail-projection`, `list-query`, `activity-ordering`, `model-immutability`,
//! `delete-transaction` (soft-delete and attachment marking steps), `chat-isolation`; states
//! `chat-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm_migration::MigratorTrait;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};
use uuid::Uuid;

use super::{ChatRecord, ChatRepo};
use crate::domain::error::{DomainError, ResourceKind};
use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::migrations::Migrator;
use crate::test_support::chats::{
    a1, a2, at, attachment_rows, b1, blob, chat_row, children_counts, message_rows, owner_scope,
    seed_attachment, seed_chat, seed_message, seed_message_in, seed_reaction, seed_running_turn,
    soft_delete_message,
};
use crate::test_support::{TENANT_A, TestDb, raw_exec, sqlite_db};

/// The test database is `SQLite`.
const STORAGE: TimeStorage = TimeStorage::Rfc3339Text;

async fn setup() -> TestDb {
    sqlite_db(Migrator::migrations()).await
}

async fn chat_of(db: &TestDb, who: &toolkit_security::SecurityContext, title: &str) -> ChatRecord {
    seed_chat(db, who, Uuid::new_v4(), Some(title), "gpt-5.2", at(0)).await
}

fn assert_chat_not_found<T: std::fmt::Debug>(result: Result<T, DomainError>, what: &str) {
    match result {
        Err(DomainError::NotFound {
            resource: ResourceKind::Chat,
            ..
        }) => {}
        other => panic!("{what}: {other:?}"),
    }
}

// ── chat detail projection ───────────────────────────────────────────────────

#[test]
fn the_projection_carries_the_row_and_the_count_and_is_never_temporary() {
    let record = ChatRecord {
        id: Uuid::new_v4(),
        tenant_id: TENANT_A,
        user_id: Uuid::new_v4(),
        model: "gpt-5.2".to_owned(),
        title: Some("Q3".to_owned()),
        created_at: at(1),
        updated_at: at(2),
    };
    let detail = record.clone().into_detail(12);
    assert_eq!(detail.id, record.id);
    assert_eq!(detail.model, "gpt-5.2");
    assert_eq!(detail.title.as_deref(), Some("Q3"));
    assert!(!detail.is_temporary);
    assert_eq!(detail.message_count, 12);
    assert_eq!(detail.created_at, at(1));
    assert_eq!(detail.updated_at, at(2));

    let untitled = ChatRecord {
        title: None,
        ..record
    };
    assert_eq!(untitled.into_detail(0).title, None);
}

// ── message count ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_new_chat_has_no_messages() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "empty").await;
    let count = ChatRepo::count_messages(&db.provider.conn().unwrap(), &chat)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn the_count_covers_all_roles_and_only_the_non_deleted_messages_of_the_chat() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "counted").await;
    let other = chat_of(&db, &a1(), "other").await;
    seed_message(&db, &chat, "user").await;
    seed_message(&db, &chat, "assistant").await;
    seed_message(&db, &chat, "user").await;
    let deleted = seed_message(&db, &chat, "assistant").await;
    soft_delete_message(&db, deleted).await;
    seed_message(&db, &other, "user").await;

    // a row of the same chat id but another tenant is not part of the tenant's messages
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO messages (id, tenant_id, chat_id, role, content, content_type, created_at) \
             VALUES ({}, {}, {}, 'user', 'x', 'text', '2026-01-01 00:00:00+00:00')",
            blob(Uuid::new_v4()),
            blob(Uuid::new_v4()),
            blob(chat.id)
        ),
    )
    .await
    .unwrap();

    let conn = db.provider.conn().unwrap();
    assert_eq!(ChatRepo::count_messages(&conn, &chat).await.unwrap(), 3);
    assert_eq!(ChatRepo::count_messages(&conn, &other).await.unwrap(), 1);
}

#[tokio::test]
async fn messages_written_later_are_counted_through_the_same_rule() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "growing").await;
    let conn = db.provider.conn().unwrap();
    for expected in 1..=3 {
        seed_message(&db, &chat, "user").await;
        assert_eq!(
            ChatRepo::count_messages(&conn, &chat).await.unwrap(),
            expected
        );
    }
}

// ── title write ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn update_title_returns_the_row_as_written() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "before").await;
    let conn = db.provider.conn().unwrap();
    let written = ChatRepo::update_title(&conn, &owner_scope(&a1()), chat.id, "after")
        .await
        .unwrap();
    assert_eq!(written.id, chat.id);
    assert_eq!(written.title.as_deref(), Some("after"));
    assert!(
        written.updated_at > chat.updated_at,
        "updated_at moves forward"
    );

    let row = chat_row(&db, chat.id).await.unwrap();
    assert_eq!(row.title.as_deref(), Some("after"));
    assert_eq!(
        row.updated_at, written.updated_at,
        "the returned row is the stored row"
    );
}

#[tokio::test]
async fn update_title_writes_only_title_and_updated_at() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "before").await;
    seed_message(&db, &chat, "user").await;
    seed_attachment(&db, &chat, "document", Some("pending")).await;
    let messages_before = message_rows(&db, chat.id).await;
    let attachments_before = attachment_rows(&db, chat.id).await;
    let before = chat_row(&db, chat.id).await.unwrap();

    ChatRepo::update_title(
        &db.provider.conn().unwrap(),
        &owner_scope(&a1()),
        chat.id,
        "after",
    )
    .await
    .unwrap();

    let after = chat_row(&db, chat.id).await.unwrap();
    assert_eq!(after.model, before.model, "model is immutable");
    assert_eq!(after.is_temporary, before.is_temporary);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.tenant_id, before.tenant_id);
    assert_eq!(after.user_id, before.user_id);
    assert_eq!(after.deleted_at, None);
    assert_eq!(message_rows(&db, chat.id).await, messages_before);
    assert_eq!(attachment_rows(&db, chat.id).await, attachments_before);
}

#[tokio::test]
async fn update_title_cannot_cross_the_owner_tenant_or_deletion_boundary() {
    let db = setup().await;
    let mine = chat_of(&db, &a1(), "mine").await;
    let conn = db.provider.conn().unwrap();
    for (who, label) in [(a2(), "other user"), (b1(), "other tenant")] {
        assert_chat_not_found(
            ChatRepo::update_title(&conn, &owner_scope(&who), mine.id, "hijacked").await,
            label,
        );
    }
    assert_eq!(
        chat_row(&db, mine.id).await.unwrap().title.as_deref(),
        Some("mine")
    );

    ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&a1()), mine.id)
        .await
        .unwrap();
    assert_chat_not_found(
        ChatRepo::update_title(&conn, &owner_scope(&a1()), mine.id, "late").await,
        "soft-deleted",
    );
    assert_chat_not_found(
        ChatRepo::update_title(&conn, &owner_scope(&a1()), Uuid::new_v4(), "ghost").await,
        "missing",
    );
}

// ── activity ordering ────────────────────────────────────────────────────────

#[tokio::test]
async fn touching_a_chat_moves_updated_at_forward_and_writes_nothing_else() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "active").await;
    let before = chat_row(&db, chat.id).await.unwrap();
    let conn = db.provider.conn().unwrap();

    let bumped = ChatRepo::touch_chat(&conn, &owner_scope(&a1()), chat.id)
        .await
        .unwrap();
    assert!(bumped > before.updated_at);

    let after = chat_row(&db, chat.id).await.unwrap();
    assert_eq!(after.updated_at, bumped);
    assert_eq!(after.title, before.title);
    assert_eq!(after.model, before.model);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.deleted_at, None);
}

#[tokio::test]
async fn touching_a_foreign_or_deleted_chat_is_a_chat_not_found() {
    let db = setup().await;
    let mine = chat_of(&db, &a1(), "mine").await;
    let conn = db.provider.conn().unwrap();
    assert_chat_not_found(
        ChatRepo::touch_chat(&conn, &owner_scope(&a2()), mine.id).await,
        "other user",
    );
    ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&a1()), mine.id)
        .await
        .unwrap();
    assert_chat_not_found(
        ChatRepo::touch_chat(&conn, &owner_scope(&a1()), mine.id).await,
        "deleted",
    );
}

#[tokio::test]
async fn a_message_write_and_the_activity_bump_commit_or_roll_back_together() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "message activity").await;
    let before = chat_row(&db, chat.id).await.unwrap().updated_at;

    // committed: the message and the bump are both visible
    let (scope, record) = (owner_scope(&a1()), chat.clone());
    db.provider
        .transaction(move |tx| {
            Box::pin(async move {
                seed_message_in(tx, &record).await?;
                ChatRepo::touch_chat(tx, &scope, record.id).await
            })
        })
        .await
        .expect("commit");
    let committed = chat_row(&db, chat.id).await.unwrap().updated_at;
    assert!(committed > before);
    assert_eq!(message_rows(&db, chat.id).await.len(), 1);

    // rolled back: neither the message nor the bump survives
    let (scope, record) = (owner_scope(&a1()), chat.clone());
    let outcome: Result<(), DomainError> = db
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                seed_message_in(tx, &record).await?;
                ChatRepo::touch_chat(tx, &scope, record.id).await?;
                Err(DomainError::internal("forced failure after the bump"))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(chat_row(&db, chat.id).await.unwrap().updated_at, committed);
    assert_eq!(message_rows(&db, chat.id).await.len(), 1);
}

// ── list page ────────────────────────────────────────────────────────────────

fn order(field: &str, dir: SortDir) -> ODataQuery {
    ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: field.to_owned(),
        dir,
    }]))
}

#[tokio::test]
async fn the_page_holds_only_the_callers_live_chats_most_recently_updated_first() {
    let db = setup().await;
    let old = seed_chat(&db, &a1(), Uuid::new_v4(), Some("old"), "m", at(10)).await;
    let new = seed_chat(&db, &a1(), Uuid::new_v4(), Some("new"), "m", at(30)).await;
    let mid = seed_chat(&db, &a1(), Uuid::new_v4(), Some("mid"), "m", at(20)).await;
    let deleted = seed_chat(&db, &a1(), Uuid::new_v4(), Some("deleted"), "m", at(40)).await;
    seed_chat(&db, &a2(), Uuid::new_v4(), Some("other user"), "m", at(50)).await;
    seed_chat(
        &db,
        &b1(),
        Uuid::new_v4(),
        Some("other tenant"),
        "m",
        at(60),
    )
    .await;
    let conn = db.provider.conn().unwrap();
    ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&a1()), deleted.id)
        .await
        .unwrap();

    let page = ChatRepo::list_chat_page(&conn, &owner_scope(&a1()), &ODataQuery::new(), STORAGE)
        .await
        .unwrap();
    let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![new.id, mid.id, old.id]);
    assert_eq!(page.page_info.limit, 20, "the default page size");
    assert_eq!(page.page_info.next_cursor, None);
    assert_eq!(page.page_info.prev_cursor, None);
}

#[tokio::test]
async fn the_page_size_defaults_to_20_and_is_clamped_to_100() {
    let db = setup().await;
    for n in 0..105 {
        seed_chat(&db, &a1(), Uuid::new_v4(), Some("c"), "m", at(n)).await;
    }
    let conn = db.provider.conn().unwrap();
    let scope = owner_scope(&a1());

    let default_page = ChatRepo::list_chat_page(&conn, &scope, &ODataQuery::new(), STORAGE)
        .await
        .unwrap();
    assert_eq!(default_page.items.len(), 20);
    assert_eq!(default_page.page_info.limit, 20);
    assert!(default_page.page_info.next_cursor.is_some());

    let clamped =
        ChatRepo::list_chat_page(&conn, &scope, &ODataQuery::new().with_limit(5_000), STORAGE)
            .await
            .unwrap();
    assert_eq!(clamped.items.len(), 100);
    assert_eq!(clamped.page_info.limit, 100);

    let small = ChatRepo::list_chat_page(&conn, &scope, &ODataQuery::new().with_limit(7), STORAGE)
        .await
        .unwrap();
    assert_eq!(small.items.len(), 7);
    assert_eq!(small.page_info.limit, 7);
}

#[tokio::test]
async fn every_effective_order_is_total_with_the_id_tiebreak_in_the_primary_direction() {
    let db = setup().await;
    // equal updated_at and equal titles everywhere: only the id tiebreak orders the chats
    let mut ids = Vec::new();
    for _ in 0..6 {
        let id = Uuid::new_v4();
        seed_chat(&db, &a1(), id, Some("same"), "m", at(5)).await;
        ids.push(id);
    }
    ids.sort();
    let mut descending = ids.clone();
    descending.reverse();
    let conn = db.provider.conn().unwrap();
    let scope = owner_scope(&a1());

    let ids_of = |page: &toolkit_odata::Page<ChatRecord>| -> Vec<Uuid> {
        page.items.iter().map(|c| c.id).collect()
    };
    let default_order = ChatRepo::list_chat_page(&conn, &scope, &ODataQuery::new(), STORAGE)
        .await
        .unwrap();
    assert_eq!(
        ids_of(&default_order),
        descending,
        "updated_at desc, id desc"
    );

    let cases = vec![
        (order("updated_at", SortDir::Desc), descending.clone()),
        (order("updated_at", SortDir::Asc), ids.clone()),
        (order("title", SortDir::Desc), descending.clone()),
        (order("title", SortDir::Asc), ids.clone()),
        (order("id", SortDir::Desc), descending.clone()),
        (order("id", SortDir::Asc), ids.clone()),
    ];
    for (query, expected) in cases {
        let page = ChatRepo::list_chat_page(&conn, &scope, &query, STORAGE)
            .await
            .unwrap();
        assert_eq!(ids_of(&page), expected, "{:?}", query.order);
    }
}

#[tokio::test]
async fn an_order_outside_the_allowed_fields_is_an_odata_error() {
    let db = setup().await;
    seed_chat(&db, &a1(), Uuid::new_v4(), Some("c"), "m", at(1)).await;
    let outcome = ChatRepo::list_chat_page(
        &db.provider.conn().unwrap(),
        &owner_scope(&a1()),
        &order("model", SortDir::Asc),
        STORAGE,
    )
    .await;
    assert!(
        matches!(
            outcome,
            Err(DomainError::ODataQuery(
                toolkit_odata::Error::InvalidOrderByField(_)
            ))
        ),
        "{outcome:?}"
    );
}

// ── delete transaction steps ─────────────────────────────────────────────────

#[tokio::test]
async fn the_soft_delete_sets_deleted_at_and_updated_at_to_the_returned_time_and_keeps_the_row() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "doomed").await;
    let before = chat_row(&db, chat.id).await.unwrap();
    let deleted_at =
        ChatRepo::soft_delete_chat_at(&db.provider.conn().unwrap(), &owner_scope(&a1()), chat.id)
            .await
            .unwrap();

    let row = chat_row(&db, chat.id).await.expect("the row is kept");
    assert_eq!(row.deleted_at, Some(deleted_at));
    assert_eq!(
        row.updated_at, deleted_at,
        "one current time for both columns"
    );
    assert!(deleted_at > before.updated_at);
    assert_eq!(row.title, before.title);
    assert_eq!(row.model, before.model);
}

#[tokio::test]
async fn a_second_soft_delete_and_a_foreign_soft_delete_report_not_found_and_change_nothing() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "doomed").await;
    let conn = db.provider.conn().unwrap();

    for (who, label) in [(a2(), "other user"), (b1(), "other tenant")] {
        assert_chat_not_found(
            ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&who), chat.id).await,
            label,
        );
    }
    assert_eq!(chat_row(&db, chat.id).await.unwrap().deleted_at, None);

    let first = ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&a1()), chat.id)
        .await
        .unwrap();
    assert_chat_not_found(
        ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&a1()), chat.id).await,
        "second delete",
    );
    assert_eq!(
        chat_row(&db, chat.id).await.unwrap().deleted_at,
        Some(first),
        "the first deletion time is kept"
    );
}

#[tokio::test]
async fn marking_sets_pending_on_attachments_without_a_status_and_keeps_every_existing_status() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "with files").await;
    let document = seed_attachment(&db, &chat, "document", None).await;
    let image = seed_attachment(&db, &chat, "image", None).await;
    let pending = seed_attachment(&db, &chat, "document", Some("pending")).await;
    let done = seed_attachment(&db, &chat, "document", Some("done")).await;
    let failed = seed_attachment(&db, &chat, "image", Some("failed")).await;

    let marked = ChatRepo::mark_attachments_cleanup_pending(&db.provider.conn().unwrap(), &chat)
        .await
        .unwrap();
    assert_eq!(marked, 2, "only the rows without a status are marked");

    let status_of = |rows: &[crate::infra::storage::entity::attachment::Model], id: Uuid| {
        rows.iter()
            .find(|row| row.id == id)
            .unwrap()
            .cleanup_status
            .clone()
    };
    let rows = attachment_rows(&db, chat.id).await;
    assert_eq!(status_of(&rows, document).as_deref(), Some("pending"));
    assert_eq!(
        status_of(&rows, image).as_deref(),
        Some("pending"),
        "images are marked too"
    );
    assert_eq!(status_of(&rows, pending).as_deref(), Some("pending"));
    assert_eq!(status_of(&rows, done).as_deref(), Some("done"));
    assert_eq!(status_of(&rows, failed).as_deref(), Some("failed"));
    assert!(
        rows.iter().all(|row| row.deleted_at.is_none()),
        "the deleted_at of an attachment is never set by the marking"
    );
}

#[tokio::test]
async fn marking_is_limited_to_the_chat_and_its_tenant() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "marked").await;
    let sibling = chat_of(&db, &a1(), "sibling").await;
    let foreign_chat = chat_of(&db, &b1(), "foreign").await;
    seed_attachment(&db, &chat, "document", None).await;
    seed_attachment(&db, &sibling, "document", None).await;
    seed_attachment(&db, &foreign_chat, "document", None).await;

    // a row of this chat id that carries another tenant id is not this tenant's attachment
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
             status, attachment_kind, created_at, updated_at) \
             VALUES ({}, {}, {}, {}, 'x', 'ready', 'document', '2026-01-01 00:00:00+00:00', \
             '2026-01-01 00:00:00+00:00')",
            blob(Uuid::new_v4()),
            blob(Uuid::new_v4()),
            blob(chat.id),
            blob(chat.user_id)
        ),
    )
    .await
    .unwrap();

    let marked = ChatRepo::mark_attachments_cleanup_pending(&db.provider.conn().unwrap(), &chat)
        .await
        .unwrap();
    assert_eq!(marked, 1);
    for other in [sibling.id, foreign_chat.id] {
        let rows = attachment_rows(&db, other).await;
        assert!(rows.iter().all(|r| r.cleanup_status.is_none()), "{other}");
    }
    let rows = attachment_rows(&db, chat.id).await;
    assert_eq!(
        rows.iter()
            .filter(|r| r.cleanup_status.as_deref() == Some("pending"))
            .count(),
        1
    );
    assert_eq!(
        rows.iter().filter(|r| r.cleanup_status.is_none()).count(),
        1,
        "the foreign-tenant row is untouched"
    );
}

#[tokio::test]
async fn soft_delete_and_marking_leave_messages_turns_and_reactions_untouched() {
    let db = setup().await;
    let chat = chat_of(&db, &a1(), "busy").await;
    let user_message = seed_message(&db, &chat, "user").await;
    let assistant_message = seed_message(&db, &chat, "assistant").await;
    seed_reaction(&db, &chat, assistant_message).await;
    seed_running_turn(&db, &chat).await;
    seed_attachment(&db, &chat, "document", None).await;
    let messages_before = message_rows(&db, chat.id).await;
    let counts_before = children_counts(&db, chat.id).await;
    assert_eq!(counts_before, (2, 1, 1, 0));

    let conn = db.provider.conn().unwrap();
    ChatRepo::soft_delete_chat_at(&conn, &owner_scope(&a1()), chat.id)
        .await
        .unwrap();
    ChatRepo::mark_attachments_cleanup_pending(&conn, &chat)
        .await
        .unwrap();

    assert_eq!(children_counts(&db, chat.id).await, counts_before);
    assert_eq!(message_rows(&db, chat.id).await, messages_before);
    assert!(
        message_rows(&db, chat.id)
            .await
            .iter()
            .any(|m| m.id == user_message && m.deleted_at.is_none())
    );
}
