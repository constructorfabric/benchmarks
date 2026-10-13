//! Secure ORM scoping of the P1 entities (DESIGN 3.7 "Tenant scoping"):
//! `chats` is tenant + owner scoped, the chat children are tenant scoped, and
//! an empty (deny-all) scope sees nothing. Runs the real gear migrations on an
//! in-memory SQLite database.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::infra::db::entity::{chats, messages};
use mini_chat::infra::db::migrations::gear_migrations;
use mini_chat::infra::db::timestamp::to_db;
use sea_orm::{ActiveValue::Set, EntityTrait};
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

const TENANT_A: Uuid = Uuid::from_u128(0xA);
const TENANT_B: Uuid = Uuid::from_u128(0xB);
const USER_1: Uuid = Uuid::from_u128(0x1);
const USER_2: Uuid = Uuid::from_u128(0x2);

async fn provider() -> DBProvider<DbError> {
    let db = connect_db(
        "sqlite::memory:",
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, gear_migrations())
        .await
        .expect("gear migrations apply");
    DBProvider::new(db)
}

fn chat(id: u128, tenant: Uuid, user: Uuid) -> chats::ActiveModel {
    let now = to_db(OffsetDateTime::now_utc());
    chats::ActiveModel {
        id: Set(Uuid::from_u128(id)),
        tenant_id: Set(tenant),
        user_id: Set(user),
        model: Set("gpt-4.1".to_owned()),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    }
}

fn message(id: u128, tenant: Uuid, chat: u128) -> messages::ActiveModel {
    messages::ActiveModel {
        id: Set(Uuid::from_u128(id)),
        tenant_id: Set(tenant),
        chat_id: Set(Uuid::from_u128(chat)),
        request_id: Set(None),
        role: Set("user".to_owned()),
        content: Set("hello".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(to_db(OffsetDateTime::now_utc())),
        deleted_at: Set(None),
    }
}

async fn seed(db: &DBProvider<DbError>) {
    let conn = db.conn().unwrap();
    let all = AccessScope::allow_all();
    for (id, tenant, user) in [
        (0x101, TENANT_A, USER_1),
        (0x102, TENANT_A, USER_2),
        (0x201, TENANT_B, USER_1),
    ] {
        secure_insert::<chats::Entity>(chat(id, tenant, user), &all, &conn)
            .await
            .unwrap();
    }
    for (id, tenant, chat_id) in [(0x1001, TENANT_A, 0x101), (0x2001, TENANT_B, 0x201)] {
        secure_insert::<messages::Entity>(message(id, tenant, chat_id), &all, &conn)
            .await
            .unwrap();
    }
}

fn owner_scope(tenant: Uuid, owner: Uuid) -> AccessScope {
    AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
        ScopeFilter::eq(pep_properties::OWNER_ID, owner),
    ])])
}

async fn chat_ids(db: &DBProvider<DbError>, scope: &AccessScope) -> Vec<u128> {
    let conn = db.conn().unwrap();
    let mut ids: Vec<u128> = chats::Entity::find()
        .secure()
        .scope_with(scope)
        .all(&conn)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id.as_u128())
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn chats_are_scoped_by_tenant() {
    let db = provider().await;
    seed(&db).await;
    assert_eq!(
        chat_ids(&db, &AccessScope::for_tenant(TENANT_A)).await,
        vec![0x101, 0x102]
    );
    assert_eq!(
        chat_ids(&db, &AccessScope::for_tenant(TENANT_B)).await,
        vec![0x201]
    );
}

#[tokio::test]
async fn chats_are_scoped_by_owner_within_a_tenant() {
    let db = provider().await;
    seed(&db).await;
    assert_eq!(
        chat_ids(&db, &owner_scope(TENANT_A, USER_1)).await,
        vec![0x101]
    );
    assert_eq!(
        chat_ids(&db, &owner_scope(TENANT_A, USER_2)).await,
        vec![0x102]
    );
    // The same user id in another tenant sees only that tenant's chat.
    assert_eq!(
        chat_ids(&db, &owner_scope(TENANT_B, USER_1)).await,
        vec![0x201]
    );
    assert!(
        chat_ids(&db, &owner_scope(TENANT_B, USER_2))
            .await
            .is_empty(),
        "owner and tenant must both match"
    );
}

#[tokio::test]
async fn a_deny_all_scope_sees_nothing() {
    let db = provider().await;
    seed(&db).await;
    assert!(chat_ids(&db, &AccessScope::default()).await.is_empty());
}

#[tokio::test]
async fn chat_children_are_tenant_scoped() {
    let db = provider().await;
    seed(&db).await;
    let conn = db.conn().unwrap();
    let rows = messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT_A))
        .all(&conn)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, Uuid::from_u128(0x1001));
    let none = messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::default())
        .all(&conn)
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn a_tenant_scoped_insert_rejects_a_foreign_tenant_row() {
    let db = provider().await;
    let conn = db.conn().unwrap();
    let result = secure_insert::<chats::Entity>(
        chat(0x301, TENANT_B, USER_1),
        &AccessScope::for_tenant(TENANT_A),
        &conn,
    )
    .await;
    assert!(
        result.is_err(),
        "a row of tenant B must not be insertable under a tenant A scope"
    );
}
