#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access` and
//! `cpt-cf-mini-chat-dod-gear-foundation-entities-scoping`.

use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

use super::{load_attachment, load_chat, load_message, load_turn, load_vector_store};
use crate::domain::error::{DomainError, ResourceKind};
use crate::infra::db::test_support::{TestDb, attachment_row, chat_row, message_row, turn_row};

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

const TENANT: u128 = 0x7E;
const OTHER_TENANT: u128 = 0x7F;
const USER: u128 = 0x55;
const OTHER_USER: u128 = 0x56;

/// Scope as compiled by the PEP for the chat resource: tenant and owner.
fn scope(tenant: u128, user: u128) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, id(tenant)),
        ScopeFilter::eq(pep_properties::OWNER_ID, id(user)),
    ]))
}

fn caller() -> AccessScope {
    scope(TENANT, USER)
}

fn assert_not_found(err: &DomainError, expected: ResourceKind) {
    match err {
        DomainError::NotFound { resource } => assert_eq!(*resource, expected),
        other => panic!("expected NotFound({expected:?}), got {other:?}"),
    }
}

/// Chat `1` is owned by the caller; chat `2` by another user of the same
/// tenant; chat `3` by a user of another tenant; chat `4` is the caller's but
/// soft-deleted; chat `5` is a second live chat of the caller.
async fn seeded() -> TestDb {
    let db = TestDb::migrated().await;
    for (chat, tenant, user) in [
        (1, TENANT, USER),
        (2, TENANT, OTHER_USER),
        (3, OTHER_TENANT, USER),
        (4, TENANT, USER),
        (5, TENANT, USER),
    ] {
        db.insert("chats", chat_row(id(chat), id(tenant), id(user)))
            .await
            .expect("insert chat");
    }
    db.soft_delete("chats", id(4)).await;
    db
}

// ---- chat ----------------------------------------------------------------------

#[tokio::test]
async fn returns_the_callers_live_chat() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let chat = load_chat(&conn, &caller(), id(1)).await.expect("chat");

    assert_eq!(chat.id, id(1));
    assert_eq!(chat.tenant_id, id(TENANT));
    assert_eq!(chat.user_id, id(USER));
}

#[tokio::test]
async fn hides_a_chat_of_another_user() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let err = load_chat(&conn, &caller(), id(2))
        .await
        .expect_err("hidden");

    assert_not_found(&err, ResourceKind::Chat);
}

#[tokio::test]
async fn hides_a_chat_of_another_tenant() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let err = load_chat(&conn, &caller(), id(3))
        .await
        .expect_err("hidden");

    assert_not_found(&err, ResourceKind::Chat);
}

#[tokio::test]
async fn hides_a_soft_deleted_chat() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let err = load_chat(&conn, &caller(), id(4))
        .await
        .expect_err("hidden");

    assert_not_found(&err, ResourceKind::Chat);
}

#[tokio::test]
async fn reports_an_unknown_chat_as_not_found() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let err = load_chat(&conn, &caller(), id(999))
        .await
        .expect_err("unknown");

    assert_not_found(&err, ResourceKind::Chat);
}

#[tokio::test]
async fn an_empty_scope_sees_no_chat() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let err = load_chat(&conn, &AccessScope::deny_all(), id(1))
        .await
        .expect_err("deny-all scope");

    assert_not_found(&err, ResourceKind::Chat);
}

// ---- message -------------------------------------------------------------------

#[tokio::test]
async fn returns_a_live_message_of_the_chat() {
    let db = seeded().await;
    db.insert("messages", message_row(id(10), id(TENANT), id(1)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let (chat, message) = load_message(&conn, &caller(), id(1), id(10))
        .await
        .expect("message");

    assert_eq!(chat.id, id(1));
    assert_eq!(message.id, id(10));
    assert_eq!(message.chat_id, id(1));
}

#[tokio::test]
async fn hides_a_message_of_another_chat_of_the_caller() {
    let db = seeded().await;
    db.insert("messages", message_row(id(11), id(TENANT), id(5)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_message(&conn, &caller(), id(1), id(11))
        .await
        .expect_err("message belongs to chat 5");

    assert_not_found(&err, ResourceKind::Message);
}

#[tokio::test]
async fn hides_a_soft_deleted_message() {
    let db = seeded().await;
    db.insert("messages", message_row(id(10), id(TENANT), id(1)))
        .await
        .unwrap();
    db.soft_delete("messages", id(10)).await;
    let conn = db.db.conn().unwrap();

    let err = load_message(&conn, &caller(), id(1), id(10))
        .await
        .expect_err("soft-deleted");

    assert_not_found(&err, ResourceKind::Message);
}

#[tokio::test]
async fn hides_a_message_row_of_another_tenant() {
    let db = seeded().await;
    db.insert("messages", message_row(id(12), id(OTHER_TENANT), id(1)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_message(&conn, &caller(), id(1), id(12))
        .await
        .expect_err("tenant column filters the child query");

    assert_not_found(&err, ResourceKind::Message);
}

#[tokio::test]
async fn a_message_of_a_foreign_chat_is_reported_as_a_missing_chat() {
    let db = seeded().await;
    db.insert("messages", message_row(id(13), id(TENANT), id(2)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_message(&conn, &caller(), id(2), id(13))
        .await
        .expect_err("chat 2 belongs to another user");

    assert_not_found(&err, ResourceKind::Chat);
}

#[tokio::test]
async fn reports_an_unknown_message_as_not_found() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let err = load_message(&conn, &caller(), id(1), id(999))
        .await
        .expect_err("unknown");

    assert_not_found(&err, ResourceKind::Message);
}

// ---- turn ----------------------------------------------------------------------

#[tokio::test]
async fn returns_a_live_turn_of_the_chat() {
    let db = seeded().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), id(TENANT), id(1), id(500), "completed"),
    )
    .await
    .unwrap();
    let conn = db.db.conn().unwrap();

    let (chat, turn) = load_turn(&conn, &caller(), id(1), id(20))
        .await
        .expect("turn");

    assert_eq!(chat.id, id(1));
    assert_eq!(turn.id, id(20));
    assert_eq!(turn.chat_id, id(1));
    assert_eq!(turn.request_id, id(500));
}

#[tokio::test]
async fn hides_a_turn_of_another_chat_of_the_caller() {
    let db = seeded().await;
    db.insert(
        "chat_turns",
        turn_row(id(21), id(TENANT), id(5), id(501), "completed"),
    )
    .await
    .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_turn(&conn, &caller(), id(1), id(21))
        .await
        .expect_err("turn belongs to chat 5");

    assert_not_found(&err, ResourceKind::Turn);
}

#[tokio::test]
async fn hides_a_soft_deleted_turn() {
    let db = seeded().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), id(TENANT), id(1), id(500), "completed"),
    )
    .await
    .unwrap();
    db.soft_delete("chat_turns", id(20)).await;
    let conn = db.db.conn().unwrap();

    let err = load_turn(&conn, &caller(), id(1), id(20))
        .await
        .expect_err("soft-deleted");

    assert_not_found(&err, ResourceKind::Turn);
}

#[tokio::test]
async fn a_turn_of_a_soft_deleted_chat_is_reported_as_a_missing_chat() {
    let db = seeded().await;
    db.insert(
        "chat_turns",
        turn_row(id(22), id(TENANT), id(4), id(502), "completed"),
    )
    .await
    .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_turn(&conn, &caller(), id(4), id(22))
        .await
        .expect_err("chat 4 is soft-deleted");

    assert_not_found(&err, ResourceKind::Chat);
}

// ---- attachment ----------------------------------------------------------------

#[tokio::test]
async fn returns_a_live_attachment_of_the_chat() {
    let db = seeded().await;
    db.insert("attachments", attachment_row(id(30), id(TENANT), id(1)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let (chat, attachment) = load_attachment(&conn, &caller(), id(1), id(30))
        .await
        .expect("attachment");

    assert_eq!(chat.id, id(1));
    assert_eq!(attachment.id, id(30));
    assert_eq!(attachment.chat_id, id(1));
}

#[tokio::test]
async fn hides_an_attachment_of_another_chat_of_the_caller() {
    let db = seeded().await;
    db.insert("attachments", attachment_row(id(31), id(TENANT), id(5)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_attachment(&conn, &caller(), id(1), id(31))
        .await
        .expect_err("attachment belongs to chat 5");

    assert_not_found(&err, ResourceKind::Attachment);
}

#[tokio::test]
async fn hides_a_soft_deleted_attachment() {
    let db = seeded().await;
    db.insert("attachments", attachment_row(id(30), id(TENANT), id(1)))
        .await
        .unwrap();
    db.soft_delete("attachments", id(30)).await;
    let conn = db.db.conn().unwrap();

    let err = load_attachment(&conn, &caller(), id(1), id(30))
        .await
        .expect_err("soft-deleted");

    assert_not_found(&err, ResourceKind::Attachment);
}

#[tokio::test]
async fn an_attachment_of_another_users_chat_is_reported_as_a_missing_chat() {
    let db = seeded().await;
    db.insert("attachments", attachment_row(id(32), id(TENANT), id(2)))
        .await
        .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_attachment(&conn, &caller(), id(2), id(32))
        .await
        .expect_err("chat 2 belongs to another user");

    assert_not_found(&err, ResourceKind::Chat);
}

// ---- vector store ---------------------------------------------------------------

fn vector_store_row(id_: Uuid, tenant: Uuid, chat: Uuid) -> crate::infra::db::test_support::Row {
    crate::infra::db::test_support::Row::default()
        .set("id", id_)
        .set("tenant_id", tenant)
        .set("chat_id", chat)
        .set("provider", "openai")
        .set("created_at", crate::infra::db::test_support::TS)
}

#[tokio::test]
async fn returns_the_vector_store_bound_to_the_callers_chat() {
    let db = seeded().await;
    db.insert(
        "chat_vector_stores",
        vector_store_row(id(50), id(TENANT), id(1)),
    )
    .await
    .unwrap();
    let conn = db.db.conn().unwrap();

    let (chat, store) = load_vector_store(&conn, &caller(), id(1))
        .await
        .expect("vector store");

    assert_eq!(chat.id, id(1));
    let store = store.expect("a row exists");
    assert_eq!(store.id, id(50));
    assert_eq!(store.chat_id, id(1));
    assert_eq!(store.vector_store_id, None, "creation still in progress");
}

#[tokio::test]
async fn a_chat_without_a_vector_store_yields_none() {
    let db = seeded().await;
    let conn = db.db.conn().unwrap();

    let (_, store) = load_vector_store(&conn, &caller(), id(1))
        .await
        .expect("chat is visible");

    assert!(store.is_none());
}

#[tokio::test]
async fn the_vector_store_of_an_invisible_chat_is_never_reached() {
    let db = seeded().await;
    db.insert(
        "chat_vector_stores",
        vector_store_row(id(51), id(TENANT), id(2)),
    )
    .await
    .unwrap();
    let conn = db.db.conn().unwrap();

    let err = load_vector_store(&conn, &caller(), id(2))
        .await
        .expect_err("chat 2 belongs to another user");

    assert_not_found(&err, ResourceKind::Chat);
}

#[tokio::test]
async fn a_vector_store_row_of_another_tenant_is_filtered_out() {
    let db = seeded().await;
    db.insert(
        "chat_vector_stores",
        vector_store_row(id(52), id(OTHER_TENANT), id(1)),
    )
    .await
    .unwrap();
    let conn = db.db.conn().unwrap();

    let (_, store) = load_vector_store(&conn, &caller(), id(1))
        .await
        .expect("chat is visible");

    assert!(store.is_none(), "tenant_id filters the vector store query");
}
