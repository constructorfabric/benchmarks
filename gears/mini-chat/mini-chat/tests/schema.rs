#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Schema-level guarantees of the initial migration (D§3.7): tables, unique
//! and partial unique indexes, CHECK constraints, UUID storage.

mod common;

use common::{
    chat_row, message_row, quota_row, raw_strings, reaction_row, tenant_scope, test_db,
    test_db_with_raw, turn_row, vector_store_row,
};
use mini_chat::infra::db::repos::{
    ChatRepo, MessageRepo, QuotaUsageRepo, ReactionRepo, TurnRepo, VectorStoreRepo,
};
use uuid::Uuid;

#[tokio::test]
async fn all_tables_created() {
    let (_db, raw) = test_db_with_raw().await;
    let tables = raw_strings(&raw, "SELECT name FROM sqlite_master WHERE type = 'table'").await;
    for expected in [
        "chats",
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
        "message_reactions",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "table {expected} missing; have {tables:?}"
        );
    }
    assert!(
        tables.iter().any(|t| t.starts_with("toolkit_outbox")),
        "outbox tables missing; have {tables:?}"
    );
}

#[tokio::test]
async fn one_running_turn_per_chat() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();

    TurnRepo
        .insert(&conn, &scope, turn_row(&chat, Uuid::new_v4(), "running"))
        .await
        .unwrap();
    let err = TurnRepo
        .insert(&conn, &scope, turn_row(&chat, Uuid::new_v4(), "running"))
        .await
        .expect_err("second running turn must be rejected");
    assert!(err.is_unique_violation(), "unexpected error: {err:?}");

    TurnRepo
        .insert(&conn, &scope, turn_row(&chat, Uuid::new_v4(), "completed"))
        .await
        .expect("a completed turn next to the running one is allowed");
}

#[tokio::test]
async fn turn_request_id_unique() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();
    let request_id = Uuid::new_v4();

    TurnRepo
        .insert(&conn, &scope, turn_row(&chat, request_id, "completed"))
        .await
        .unwrap();
    let err = TurnRepo
        .insert(&conn, &scope, turn_row(&chat, request_id, "failed"))
        .await
        .expect_err("duplicate (chat_id, request_id) must be rejected");
    assert!(err.is_unique_violation(), "unexpected error: {err:?}");
}

#[tokio::test]
async fn message_request_role_unique_ignores_deleted() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();
    let request_id = Uuid::new_v4();

    let mut deleted = message_row(&chat, Some(request_id), "user");
    deleted.deleted_at = Some(common::ts(1_700_000_100));
    MessageRepo.insert(&conn, &scope, deleted).await.unwrap();
    MessageRepo
        .insert(&conn, &scope, message_row(&chat, Some(request_id), "user"))
        .await
        .expect("a soft-deleted duplicate does not count");

    let err = MessageRepo
        .insert(&conn, &scope, message_row(&chat, Some(request_id), "user"))
        .await
        .expect_err("two live (chat, request, user) rows must be rejected");
    assert!(err.is_unique_violation(), "unexpected error: {err:?}");

    MessageRepo
        .insert(
            &conn,
            &scope,
            message_row(&chat, Some(request_id), "assistant"),
        )
        .await
        .expect("the assistant row of the same request is allowed");
    MessageRepo
        .insert(&conn, &scope, message_row(&chat, None, "user"))
        .await
        .unwrap();
    MessageRepo
        .insert(&conn, &scope, message_row(&chat, None, "user"))
        .await
        .expect("rows without request_id are not constrained");
}

#[tokio::test]
async fn quota_usage_unique_bucket() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);

    QuotaUsageRepo
        .insert(&conn, &scope, quota_row(tenant, user, "total"))
        .await
        .unwrap();
    QuotaUsageRepo
        .insert(&conn, &scope, quota_row(tenant, user, "tier:premium"))
        .await
        .expect("a different bucket of the same period is allowed");
    let err = QuotaUsageRepo
        .insert(&conn, &scope, quota_row(tenant, user, "total"))
        .await
        .expect_err("duplicate bucket row must be rejected");
    assert!(err.is_unique_violation(), "unexpected error: {err:?}");
}

#[tokio::test]
async fn reaction_unique_per_user() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();
    let msg = MessageRepo
        .insert(&conn, &scope, message_row(&chat, None, "assistant"))
        .await
        .unwrap();

    ReactionRepo
        .insert(&conn, &scope, reaction_row(&msg, user, "like"))
        .await
        .unwrap();
    let err = ReactionRepo
        .insert(&conn, &scope, reaction_row(&msg, user, "dislike"))
        .await
        .expect_err("second reaction of the same user must be rejected");
    assert!(err.is_unique_violation(), "unexpected error: {err:?}");

    let bogus = ReactionRepo
        .insert(&conn, &scope, reaction_row(&msg, user, "love"))
        .await;
    assert!(bogus.is_err(), "reaction CHECK must reject unknown values");
}

#[tokio::test]
async fn vector_store_unique_per_chat() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();

    VectorStoreRepo
        .insert(&conn, &scope, vector_store_row(&chat))
        .await
        .unwrap();
    let err = VectorStoreRepo
        .insert(&conn, &scope, vector_store_row(&chat))
        .await
        .expect_err("second vector store row for a chat must be rejected");
    assert!(err.is_unique_violation(), "unexpected error: {err:?}");

    let mut negative = vector_store_row(&chat_row(tenant, user));
    negative.file_count = -1;
    assert!(
        VectorStoreRepo
            .insert(&conn, &scope, negative)
            .await
            .is_err(),
        "file_count >= 0 CHECK must hold"
    );
}

#[tokio::test]
async fn state_check_constraint() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();

    let err = TurnRepo
        .insert(&conn, &scope, turn_row(&chat, Uuid::new_v4(), "bogus"))
        .await
        .expect_err("state CHECK must reject 'bogus'");
    assert!(
        !err.is_unique_violation(),
        "expected a CHECK failure: {err:?}"
    );

    let mut bad_requester = turn_row(&chat, Uuid::new_v4(), "completed");
    bad_requester.requester_type = "robot".to_owned();
    assert!(
        TurnRepo.insert(&conn, &scope, bad_requester).await.is_err(),
        "requester_type CHECK must hold"
    );
}

#[tokio::test]
async fn uuid_stored_as_blob() {
    let (db, raw) = test_db_with_raw().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();

    let types = raw_strings(&raw, "SELECT typeof(id) FROM chats").await;
    assert_eq!(types, vec!["blob".to_owned()]);
    let tenant_types = raw_strings(&raw, "SELECT typeof(tenant_id) FROM chats").await;
    assert_eq!(tenant_types, vec!["blob".to_owned()]);
}

#[test]
fn gear_migrations_are_mini_chat_then_outbox() {
    use toolkit::contracts::DatabaseCapability;
    let names: Vec<String> = mini_chat::MiniChatGear::default()
        .migrations()
        .iter()
        .map(|m| m.name().to_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "m20261004_000001_initial".to_owned(),
            "m20261004_000002_sqlite_sortable_timestamps".to_owned(),
            "m20261005_000003_sqlite_worker_timestamps".to_owned(),
            "m001_create_toolkit_outbox_schema".to_owned(),
        ]
    );
}
