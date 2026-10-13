#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the chat vector store rows (placeholder, CAS, reclaim delete) and
//! the heartbeat of the background indexing
//! (`cpt-cf-mini-chat-algo-attachments-ensure-vector-store`,
//! `-background-indexing`, `cpt-cf-mini-chat-state-attachments-chat-vector-store`).

use uuid::Uuid;

use crate::attachment_test_env::{AttEnv, AttOptions};
use crate::domain::error::DomainError;
use crate::domain::service::attachments::model::StoreRow;
use crate::domain::service::attachments::ports::AttachmentRepository;
use crate::test_doubles::{OTHER_TENANT, OTHER_USER, USER_TENANT};

fn placeholder(env: &AttEnv, chat: Uuid, tenant: Uuid) -> StoreRow {
    StoreRow {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        chat_id: chat,
        vector_store_id: None,
        provider: "openai".to_owned(),
        created_at: env.now(),
    }
}

async fn store_rows(env: &AttEnv) -> i64 {
    env.stream.count("chat_vector_stores").await
}

// ================================================================================================
// reads
// ================================================================================================

#[tokio::test]
async fn a_chat_without_a_store_has_none() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;

    assert!(
        env.sea
            .vector_store(USER_TENANT, chat)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_placeholder_and_a_bound_row_are_read_back() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let placeholder = env.seed_store(chat, None, "openai", env.now()).await;

    let row = env
        .sea
        .vector_store(USER_TENANT, chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.id, placeholder);
    assert_eq!(row.vector_store_id, None);
    assert_eq!(row.provider, "openai");
    assert_eq!(row.created_at, env.now());

    let other = env.chat("m-oa").await;
    env.seed_store(other, Some("vs_7"), "azure", env.now())
        .await;
    let bound = env
        .sea
        .vector_store(USER_TENANT, other)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound.vector_store_id.as_deref(), Some("vs_7"));
    assert_eq!(bound.provider, "azure");
}

#[tokio::test]
async fn the_store_is_read_by_tenant_and_chat_together() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.stream
        .quota
        .db
        .insert(
            "chat_vector_stores",
            crate::infra::db::test_support::Row::default()
                .set("id", Uuid::new_v4())
                .set("tenant_id", OTHER_TENANT)
                .set("chat_id", chat)
                .set("vector_store_id", "vs_foreign")
                .set("provider", "openai")
                .set("file_count", 0_i32)
                .set("created_at", AttEnv::stamp(env.now())),
        )
        .await
        .unwrap();

    assert!(
        env.sea
            .vector_store(USER_TENANT, chat)
            .await
            .unwrap()
            .is_none(),
        "a row of another tenant is never read"
    );
    let foreign = env
        .sea
        .vector_store(OTHER_TENANT, chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(foreign.vector_store_id.as_deref(), Some("vs_foreign"));
}

// ================================================================================================
// placeholder insert
// ================================================================================================

#[tokio::test]
async fn the_placeholder_is_written_with_its_provider_label_and_no_store_id() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let row = placeholder(&env, chat, USER_TENANT);

    env.sea.insert_store_placeholder(&row).await.unwrap();

    assert_eq!(env.stores().await, vec![(None, "openai".to_owned())]);
    assert_eq!(
        env.sea
            .vector_store(USER_TENANT, chat)
            .await
            .unwrap()
            .unwrap()
            .id,
        row.id
    );
    assert_eq!(
        env.stream
            .quota
            .db
            .query_i64("SELECT file_count FROM chat_vector_stores")
            .await,
        0
    );
}

#[tokio::test]
async fn a_second_row_for_the_same_tenant_and_chat_is_a_typed_unique_violation() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.sea
        .insert_store_placeholder(&placeholder(&env, chat, USER_TENANT))
        .await
        .unwrap();

    let again = env
        .sea
        .insert_store_placeholder(&placeholder(&env, chat, USER_TENANT))
        .await;

    assert!(
        matches!(again, Err(DomainError::UniqueViolation)),
        "{again:?}"
    );
    assert_eq!(store_rows(&env).await, 1);
}

#[tokio::test]
async fn a_bound_row_also_blocks_a_new_placeholder() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed_store(chat, Some("vs_1"), "openai", env.now())
        .await;

    let result = env
        .sea
        .insert_store_placeholder(&placeholder(&env, chat, USER_TENANT))
        .await;

    assert!(matches!(result, Err(DomainError::UniqueViolation)));
}

#[tokio::test]
async fn a_placeholder_for_a_deleted_chat_is_not_found_and_leaves_no_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.delete_chat(chat).await;

    let result = env
        .sea
        .insert_store_placeholder(&placeholder(&env, chat, USER_TENANT))
        .await;

    assert!(
        matches!(
            result,
            Err(DomainError::NotFound {
                resource: crate::domain::error::ResourceKind::Chat
            })
        ),
        "{result:?}"
    );
    assert_eq!(store_rows(&env).await, 0, "the insert is rolled back");
}

#[tokio::test]
async fn other_chats_and_other_tenants_have_their_own_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let other = env.chat("m-oa").await;
    // A chat id belongs to one tenant, and the placeholder insert checks its chat
    // is live in the row's tenant: the other tenant has a chat of its own.
    let foreign = env.chat_of(OTHER_TENANT, OTHER_USER, "m-oa").await;
    env.sea
        .insert_store_placeholder(&placeholder(&env, chat, USER_TENANT))
        .await
        .unwrap();

    env.sea
        .insert_store_placeholder(&placeholder(&env, other, USER_TENANT))
        .await
        .unwrap();
    env.sea
        .insert_store_placeholder(&placeholder(&env, foreign, OTHER_TENANT))
        .await
        .unwrap();

    assert_eq!(store_rows(&env).await, 3);
}

#[tokio::test]
async fn concurrent_placeholder_inserts_produce_exactly_one_winner() {
    let env = AttEnv::with(AttOptions {
        file_db: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let rows: Vec<_> = (0..4)
        .map(|_| placeholder(&env, chat, USER_TENANT))
        .collect();

    let results =
        futures::future::join_all(rows.iter().map(|row| env.sea.insert_store_placeholder(row)))
            .await;

    let winners = results.iter().filter(|r| r.is_ok()).count();
    let losers = results
        .iter()
        .filter(|r| matches!(r, Err(DomainError::UniqueViolation)))
        .count();
    assert_eq!((winners, losers), (1, 3), "{results:?}");
    assert_eq!(store_rows(&env).await, 1);
}

// ================================================================================================
// CAS and reclaim
// ================================================================================================

#[tokio::test]
async fn the_store_id_is_bound_once() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let row = placeholder(&env, chat, USER_TENANT);
    env.sea.insert_store_placeholder(&row).await.unwrap();

    let first = env
        .sea
        .bind_store(USER_TENANT, row.id, "vs_first")
        .await
        .unwrap();
    let second = env
        .sea
        .bind_store(USER_TENANT, row.id, "vs_second")
        .await
        .unwrap();

    assert!(first);
    assert!(!second, "the CAS needs vector_store_id IS NULL");
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_first".to_owned()), "openai".to_owned())]
    );
}

#[tokio::test]
async fn binding_is_bound_to_the_row_and_the_tenant() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let row = placeholder(&env, chat, USER_TENANT);
    env.sea.insert_store_placeholder(&row).await.unwrap();

    assert!(
        !env.sea
            .bind_store(OTHER_TENANT, row.id, "vs_x")
            .await
            .unwrap()
    );
    assert!(
        !env.sea
            .bind_store(USER_TENANT, Uuid::new_v4(), "vs_x")
            .await
            .unwrap()
    );
    assert_eq!(env.stores().await, vec![(None, "openai".to_owned())]);
}

#[tokio::test]
async fn a_reclaimed_placeholder_cannot_be_bound() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let row = placeholder(&env, chat, USER_TENANT);
    env.sea.insert_store_placeholder(&row).await.unwrap();
    assert!(
        env.sea
            .delete_placeholder(USER_TENANT, row.id)
            .await
            .unwrap()
    );

    let bound = env
        .sea
        .bind_store(USER_TENANT, row.id, "vs_late")
        .await
        .unwrap();

    assert!(!bound, "the CAS of a slow winner loses");
    assert_eq!(store_rows(&env).await, 0);
}

#[tokio::test]
async fn only_a_placeholder_is_deleted() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let placeholder_id = env.seed_store(chat, None, "openai", env.now()).await;
    let other = env.chat("m-oa").await;
    let bound_id = env
        .seed_store(other, Some("vs_1"), "openai", env.now())
        .await;

    assert!(
        !env.sea
            .delete_placeholder(USER_TENANT, bound_id)
            .await
            .unwrap()
    );
    assert!(
        !env.sea
            .delete_placeholder(OTHER_TENANT, placeholder_id)
            .await
            .unwrap()
    );
    assert_eq!(store_rows(&env).await, 2);
    assert!(
        env.sea
            .delete_placeholder(USER_TENANT, placeholder_id)
            .await
            .unwrap()
    );
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_1".to_owned()), "openai".to_owned())]
    );
}

// ================================================================================================
// heartbeat
// ================================================================================================

#[tokio::test]
async fn the_heartbeat_refreshes_updated_at_of_an_uploaded_live_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row.set("status", "uploaded")).await;

    let beat = env.sea.heartbeat(USER_TENANT, id, env.now()).await.unwrap();

    assert!(beat);
    assert_eq!(env.att(id).await.updated_at, AttEnv::stamp(env.now()));
    assert_eq!(env.att(id).await.status, "uploaded");
}

#[tokio::test]
async fn the_heartbeat_stops_when_the_row_left_the_indexing_state() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let cases = [
        env.seed(chat, 1, |row| row.set("status", "ready")).await,
        env.seed(chat, 2, |row| row.set("status", "pending")).await,
        env.seed(chat, 3, |row| row.set("status", "failed")).await,
        env.seed(chat, 4, |row| {
            row.set("status", "uploaded")
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await,
        env.seed(chat, 5, |row| {
            row.set("status", "uploaded")
                .set("cleanup_status", "pending")
        })
        .await,
    ];
    let uploaded = env.seed(chat, 6, |row| row.set("status", "uploaded")).await;
    let before = env.attachments_dump().await;

    for id in cases {
        assert!(
            !env.sea.heartbeat(USER_TENANT, id, env.now()).await.unwrap(),
            "{id}"
        );
    }
    assert!(
        !env.sea
            .heartbeat(USER_TENANT, Uuid::new_v4(), env.now())
            .await
            .unwrap()
    );
    assert!(
        !env.sea
            .heartbeat(OTHER_TENANT, uploaded, env.now())
            .await
            .unwrap()
    );

    assert_eq!(
        env.attachments_dump().await,
        before,
        "no guarded row was written"
    );
}
