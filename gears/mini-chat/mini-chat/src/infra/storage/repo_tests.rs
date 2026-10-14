//! Scoped persistence tests over in-memory SQLite: two tenants and two users in one tenant
//! prove that no read or write crosses the owner or tenant boundary.
//!
//! Slice S1, algorithm `scoped-persistence`, definition of done `scoped-persistence` (and the owner-predicate
//! part of `authz-pep`). Needs the migrations, so these tests fail until the schema exists.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm::EntityTrait;
use toolkit_db::secure::{DBRunner, ScopableEntity, SecureEntityExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{ChatRecord, ChatRepo, NewChat, NewMessage};
use crate::domain::authz::{Operation, PepService};
use crate::domain::error::{DomainError, ResourceKind};
use crate::infra::storage::entity::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, message_reaction,
    quota_usage, thread_summary,
};
use crate::infra::storage::migrations::Migrator;
use crate::test_support::{
    Pdp, SUBJECT_USER_TYPE, TENANT_A, TENANT_B, TestDb, USER_A1, USER_A2, USER_B1, ctx, ctx_with,
    enforcer, raw_exec, raw_i64, sqlite_db,
};
use sea_orm_migration::MigratorTrait;

const TS: &str = "2026-01-01 00:00:00+00:00";

fn blob(id: Uuid) -> String {
    format!("x'{}'", id.simple())
}

fn owner_scope(tenant: Uuid, user: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant).ensure_owner(user)
}

async fn setup() -> TestDb {
    sqlite_db(Migrator::migrations()).await
}

async fn new_chat(runner: &impl DBRunner, tenant: Uuid, user: Uuid, title: &str) -> ChatRecord {
    let scope = owner_scope(tenant, user);
    ChatRepo::create_chat(
        runner,
        &scope,
        &ctx(user, tenant),
        NewChat {
            id: Uuid::new_v4(),
            model: "gpt-5.2".to_owned(),
            title: Some(title.to_owned()),
        },
    )
    .await
    .expect("create chat")
}

fn assert_not_found_chat<T: std::fmt::Debug>(result: Result<T, DomainError>, what: &str) {
    match result {
        Err(DomainError::NotFound {
            resource: ResourceKind::Chat,
            ..
        }) => {}
        other => panic!("{what}: expected NotFound(Chat), got {other:?}"),
    }
}

// ── Secure ORM declarations (no database) ────────────────────────────────────

/// `(has tenant column, has resource column, has owner column, has type column)`
fn dimensions<E: ScopableEntity>() -> (bool, bool, bool, bool) {
    (
        E::tenant_col().is_some(),
        E::resource_col().is_some(),
        E::owner_col().is_some(),
        E::type_col().is_some(),
    )
}

#[test]
fn every_table_declares_the_tenant_column_and_no_table_declares_a_type_column() {
    let all = [
        dimensions::<chat::Entity>(),
        dimensions::<message::Entity>(),
        dimensions::<chat_turn::Entity>(),
        dimensions::<attachment::Entity>(),
        dimensions::<message_attachment::Entity>(),
        dimensions::<thread_summary::Entity>(),
        dimensions::<chat_vector_store::Entity>(),
        dimensions::<quota_usage::Entity>(),
        dimensions::<message_reaction::Entity>(),
    ];
    for (index, (tenant, _, _, type_col)) in all.into_iter().enumerate() {
        assert!(tenant, "table #{index} must declare tenant_id");
        assert!(!type_col, "table #{index} must not declare a type column");
    }
}

#[test]
fn the_owner_column_is_declared_on_chats_reactions_and_quota_usage_only() {
    assert!(dimensions::<chat::Entity>().2);
    assert!(dimensions::<message_reaction::Entity>().2);
    assert!(dimensions::<quota_usage::Entity>().2);
    assert!(!dimensions::<message::Entity>().2);
    assert!(!dimensions::<chat_turn::Entity>().2);
    assert!(!dimensions::<attachment::Entity>().2);
    assert!(!dimensions::<message_attachment::Entity>().2);
    assert!(!dimensions::<thread_summary::Entity>().2);
    assert!(!dimensions::<chat_vector_store::Entity>().2);
}

#[test]
fn the_resource_column_is_the_id_where_the_table_has_one() {
    assert!(dimensions::<chat::Entity>().1);
    assert!(dimensions::<message::Entity>().1);
    assert!(dimensions::<chat_turn::Entity>().1);
    assert!(dimensions::<attachment::Entity>().1);
    assert!(dimensions::<thread_summary::Entity>().1);
    assert!(dimensions::<quota_usage::Entity>().1);
    assert!(dimensions::<message_reaction::Entity>().1);
    assert!(!dimensions::<message_attachment::Entity>().1);
    assert!(!dimensions::<chat_vector_store::Entity>().1);
}

#[test]
fn the_declared_columns_are_tenant_id_user_id_and_id() {
    assert!(matches!(
        chat::Entity::tenant_col(),
        Some(chat::Column::TenantId)
    ));
    assert!(matches!(
        chat::Entity::owner_col(),
        Some(chat::Column::UserId)
    ));
    assert!(matches!(
        chat::Entity::resource_col(),
        Some(chat::Column::Id)
    ));
    assert!(matches!(
        quota_usage::Entity::owner_col(),
        Some(quota_usage::Column::UserId)
    ));
    assert!(matches!(
        message_reaction::Entity::owner_col(),
        Some(message_reaction::Column::UserId)
    ));
    assert!(matches!(
        message::Entity::tenant_col(),
        Some(message::Column::TenantId)
    ));
}

// ── chats ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_new_chat_takes_tenant_and_user_from_the_security_context() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "hello").await;
    assert_eq!(chat.tenant_id, TENANT_A);
    assert_eq!(chat.user_id, USER_A1);
    let loaded = ChatRepo::load_chat(&conn, &owner_scope(TENANT_A, USER_A1), chat.id)
        .await
        .unwrap();
    assert_eq!(loaded, chat);
}

#[tokio::test]
async fn an_insert_outside_the_scope_is_rejected() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let new = || NewChat {
        id: Uuid::new_v4(),
        model: "m".to_owned(),
        title: None,
    };

    // a scope for another tenant does not cover a row owned by the context tenant
    let wrong_tenant = ChatRepo::create_chat(
        &conn,
        &owner_scope(TENANT_B, USER_A1),
        &ctx(USER_A1, TENANT_A),
        new(),
    )
    .await;
    assert!(wrong_tenant.is_err());
    // a scope for another user does not cover a row owned by the context user
    let wrong_owner = ChatRepo::create_chat(
        &conn,
        &owner_scope(TENANT_A, USER_A2),
        &ctx(USER_A1, TENANT_A),
        new(),
    )
    .await;
    assert!(wrong_owner.is_err());
    assert_eq!(raw_i64(&db.raw, "SELECT COUNT(*) FROM chats").await, 0);
}

#[tokio::test]
async fn uuid_values_are_written_as_16_byte_blobs_not_text() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "blob").await;
    ChatRepo::add_message(&conn, &chat, message("hi"))
        .await
        .unwrap();
    for (table, column) in [
        ("chats", "id"),
        ("chats", "tenant_id"),
        ("chats", "user_id"),
        ("messages", "id"),
        ("messages", "tenant_id"),
        ("messages", "chat_id"),
        ("messages", "request_id"),
    ] {
        let shape = raw_i64(
            &db.raw,
            &format!("SELECT COUNT(*) FROM {table} WHERE typeof({column}) = 'blob' AND length({column}) = 16"),
        )
        .await;
        assert_eq!(
            shape, 1,
            "{table}.{column} must hold 16 raw bytes, not the text form"
        );
    }
}

#[tokio::test]
async fn a_missing_foreign_and_soft_deleted_chat_are_indistinguishable() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "mine").await;

    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_A, USER_A1), Uuid::new_v4()).await,
        "missing",
    );
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_A, USER_A2), chat.id).await,
        "foreign owner, same tenant",
    );
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_B, USER_B1), chat.id).await,
        "foreign tenant",
    );
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_B, USER_A1), chat.id).await,
        "same user id in another tenant",
    );

    ChatRepo::soft_delete_chat(&conn, &owner_scope(TENANT_A, USER_A1), chat.id)
        .await
        .unwrap();
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_A, USER_A1), chat.id).await,
        "soft-deleted",
    );
}

#[tokio::test]
async fn a_deny_all_scope_sees_nothing() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "mine").await;
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &AccessScope::deny_all(), chat.id).await,
        "deny-all",
    );
    assert!(
        ChatRepo::list_chats(&conn, &AccessScope::deny_all())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn listing_returns_only_the_callers_live_chats() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let a1_first = new_chat(&conn, TENANT_A, USER_A1, "a1-first").await;
    let a1_second = new_chat(&conn, TENANT_A, USER_A1, "a1-second").await;
    let a1_deleted = new_chat(&conn, TENANT_A, USER_A1, "a1-deleted").await;
    let _a2 = new_chat(&conn, TENANT_A, USER_A2, "a2").await;
    let _b1 = new_chat(&conn, TENANT_B, USER_B1, "b1").await;
    ChatRepo::soft_delete_chat(&conn, &owner_scope(TENANT_A, USER_A1), a1_deleted.id)
        .await
        .unwrap();

    let listed = ChatRepo::list_chats(&conn, &owner_scope(TENANT_A, USER_A1))
        .await
        .unwrap();
    let mut ids: Vec<Uuid> = listed.iter().map(|c| c.id).collect();
    ids.sort();
    let mut expected = vec![a1_first.id, a1_second.id];
    expected.sort();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn a_tenant_only_scope_spans_the_tenant_which_is_why_the_owner_predicate_is_added() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    new_chat(&conn, TENANT_A, USER_A1, "a1").await;
    new_chat(&conn, TENANT_A, USER_A2, "a2").await;
    new_chat(&conn, TENANT_B, USER_B1, "b1").await;
    assert_eq!(
        ChatRepo::list_chats(&conn, &AccessScope::for_tenant(TENANT_A))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        ChatRepo::list_chats(&conn, &owner_scope(TENANT_A, USER_A1))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_pdp_that_returns_only_a_tenant_predicate_still_lists_only_the_callers_chats() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let mine = new_chat(&conn, TENANT_A, USER_A1, "mine").await;
    new_chat(&conn, TENANT_A, USER_A2, "colleague").await;
    new_chat(&conn, TENANT_B, USER_B1, "other tenant").await;

    let (enforcer, _pdp) = enforcer(Pdp::TenantOnly);
    let pep = PepService::new(enforcer);
    let caller = ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"]);
    let outcome = pep
        .authorize(&caller, Operation::ListChats, None)
        .await
        .unwrap();
    let listed = ChatRepo::list_chats(&conn, outcome.scope().unwrap())
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![mine.id]
    );
}

#[tokio::test]
async fn update_and_delete_cannot_cross_the_owner_or_tenant_boundary() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "original").await;

    for (tenant, user) in [
        (TENANT_A, USER_A2),
        (TENANT_B, USER_B1),
        (TENANT_B, USER_A1),
    ] {
        let scope = owner_scope(tenant, user);
        assert_not_found_chat(
            ChatRepo::rename_chat(&conn, &scope, chat.id, "hijacked").await,
            "rename",
        );
        assert_not_found_chat(
            ChatRepo::soft_delete_chat(&conn, &scope, chat.id).await,
            "delete",
        );
    }
    let intact = ChatRepo::load_chat(&conn, &owner_scope(TENANT_A, USER_A1), chat.id)
        .await
        .unwrap();
    assert_eq!(intact.title.as_deref(), Some("original"));
}

#[tokio::test]
async fn the_owner_can_rename_and_soft_delete_and_the_row_is_kept() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "old").await;
    let scope = owner_scope(TENANT_A, USER_A1);
    ChatRepo::rename_chat(&conn, &scope, chat.id, "new")
        .await
        .unwrap();
    assert_eq!(
        ChatRepo::load_chat(&conn, &scope, chat.id)
            .await
            .unwrap()
            .title
            .as_deref(),
        Some("new")
    );

    ChatRepo::soft_delete_chat(&conn, &scope, chat.id)
        .await
        .unwrap();
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &scope, chat.id).await,
        "after delete",
    );
    assert_eq!(
        raw_i64(
            &db.raw,
            "SELECT COUNT(*) FROM chats WHERE deleted_at IS NOT NULL"
        )
        .await,
        1
    );
}

// ── child rows ───────────────────────────────────────────────────────────────

fn message(content: &str) -> NewMessage {
    NewMessage {
        id: Uuid::new_v4(),
        request_id: Some(Uuid::new_v4()),
        role: "user".to_owned(),
        content: content.to_owned(),
    }
}

#[tokio::test]
async fn messages_are_written_and_read_through_the_resolved_chat() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "c1").await;
    let other = new_chat(&conn, TENANT_A, USER_A1, "c2").await;
    let first = ChatRepo::add_message(&conn, &chat, message("one"))
        .await
        .unwrap();
    ChatRepo::add_message(&conn, &chat, message("two"))
        .await
        .unwrap();
    ChatRepo::add_message(&conn, &other, message("elsewhere"))
        .await
        .unwrap();

    assert_eq!(first.tenant_id, TENANT_A, "tenant_id comes from the chat");
    assert_eq!(first.chat_id, chat.id, "chat_id comes from the chat");
    let listed = ChatRepo::list_messages(&conn, &chat).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
}

#[tokio::test]
async fn a_user_cannot_obtain_the_chat_needed_to_reach_foreign_messages() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "private").await;
    ChatRepo::add_message(&conn, &chat, message("secret"))
        .await
        .unwrap();
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_A, USER_A2), chat.id).await,
        "colleague",
    );
    assert_not_found_chat(
        ChatRepo::load_chat(&conn, &owner_scope(TENANT_B, USER_B1), chat.id).await,
        "other tenant",
    );
}

#[tokio::test]
async fn child_reads_always_filter_by_tenant_even_for_a_known_chat_id() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "private").await;
    ChatRepo::add_message(&conn, &chat, message("secret"))
        .await
        .unwrap();

    let forged = ChatRecord {
        tenant_id: TENANT_B,
        ..chat.clone()
    };
    assert!(
        ChatRepo::list_messages(&conn, &forged)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        ChatRepo::vector_store(&conn, &forged)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_vector_store_is_read_through_the_chat_and_its_tenant() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "with docs").await;
    for (tenant, store) in [(TENANT_A, "vs_tenant_a"), (TENANT_B, "vs_tenant_b")] {
        raw_exec(
            &db.raw,
            &format!(
                "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, file_count, created_at) \
                 VALUES ({}, {}, {}, '{store}', 'openai', 0, '{TS}')",
                blob(Uuid::new_v4()),
                blob(tenant),
                blob(chat.id)
            ),
        )
        .await
        .expect("seed vector store");
    }
    let found = ChatRepo::vector_store(&conn, &chat)
        .await
        .unwrap()
        .expect("row of the chat's tenant");
    assert_eq!(found.tenant_id, TENANT_A);
    assert_eq!(found.vector_store_id.as_deref(), Some("vs_tenant_a"));
}

#[tokio::test]
async fn a_chat_without_a_vector_store_has_none() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "no docs").await;
    assert!(
        ChatRepo::vector_store(&conn, &chat)
            .await
            .unwrap()
            .is_none()
    );
}

// ── owner-scoped quota usage and reactions ───────────────────────────────────

async fn seed_quota(db: &TestDb, tenant: Uuid, user: Uuid) {
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, \
             spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens, \
             file_search_calls, web_search_calls, code_interpreter_calls, rag_retrieval_calls, \
             image_inputs, image_upload_bytes, updated_at) \
             VALUES ({}, {}, {}, 'daily', '2026-01-01', 'total', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, '{TS}')",
            blob(Uuid::new_v4()),
            blob(tenant),
            blob(user)
        ),
    )
    .await
    .expect("seed quota_usage");
}

async fn quota_users(db: &TestDb, scope: &AccessScope) -> Vec<Uuid> {
    let conn = db.provider.conn().unwrap();
    let mut users: Vec<Uuid> = quota_usage::Entity::find()
        .secure()
        .scope_with(scope)
        .all(&conn)
        .await
        .expect("scoped read")
        .into_iter()
        .map(|row| row.user_id)
        .collect();
    users.sort();
    users
}

#[tokio::test]
async fn quota_usage_reads_are_owner_scoped() {
    let db = setup().await;
    seed_quota(&db, TENANT_A, USER_A1).await;
    seed_quota(&db, TENANT_A, USER_A2).await;
    seed_quota(&db, TENANT_B, USER_B1).await;

    assert_eq!(
        quota_users(&db, &owner_scope(TENANT_A, USER_A1)).await,
        vec![USER_A1]
    );
    assert_eq!(
        quota_users(&db, &owner_scope(TENANT_B, USER_B1)).await,
        vec![USER_B1]
    );
    let mut tenant_wide = vec![USER_A1, USER_A2];
    tenant_wide.sort();
    assert_eq!(
        quota_users(&db, &AccessScope::for_tenant(TENANT_A)).await,
        tenant_wide
    );
}

#[tokio::test]
async fn a_pdp_that_returns_only_a_tenant_predicate_yields_only_the_callers_quota_rows() {
    let db = setup().await;
    seed_quota(&db, TENANT_A, USER_A1).await;
    seed_quota(&db, TENANT_A, USER_A2).await;

    let (enforcer, _pdp) = enforcer(Pdp::TenantOnly);
    let pep = PepService::new(enforcer);
    let caller = ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"]);
    let outcome = pep
        .authorize(&caller, Operation::GetQuotaStatus, None)
        .await
        .unwrap();
    assert_eq!(
        quota_users(&db, outcome.scope().unwrap()).await,
        vec![USER_A1]
    );
}

#[tokio::test]
async fn message_reactions_are_owner_scoped() {
    let db = setup().await;
    let conn = db.provider.conn().unwrap();
    let chat = new_chat(&conn, TENANT_A, USER_A1, "c").await;
    let msg = ChatRepo::add_message(&conn, &chat, message("hi"))
        .await
        .unwrap();
    for user in [USER_A1, USER_A2] {
        raw_exec(
            &db.raw,
            &format!(
                "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
                 VALUES ({}, {}, {}, {}, 'like', '{TS}')",
                blob(Uuid::new_v4()),
                blob(msg.id),
                blob(user),
                blob(TENANT_A)
            ),
        )
        .await
        .expect("seed reaction");
    }
    let conn = db.provider.conn().unwrap();
    let mine = message_reaction::Entity::find()
        .secure()
        .scope_with(&owner_scope(TENANT_A, USER_A1))
        .all(&conn)
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].user_id, USER_A1);
}
