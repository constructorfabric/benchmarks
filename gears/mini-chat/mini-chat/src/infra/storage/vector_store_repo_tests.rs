//! `SQLite` repository tests of the `chat_vector_stores` port: access only by
//! `(tenant_id, chat_id)`, the placeholder insert with its unique violation, the guarded
//! placeholder delete and the compare-and-set bind.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning`;
//! state `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-vector-store`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::vector_store::{ChatVectorStoreRepo, PlaceholderInsert};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{seed_vector_store, vector_store_rows};
use crate::test_support::chats::at;
use crate::test_support::quota::{QuotaRig, STANDARD, quota_rig};
use crate::test_support::{TENANT_A, TENANT_B};

fn now() -> OffsetDateTime {
    at(1_000_000) + time::Duration::microseconds(456_000)
}

async fn rig() -> (QuotaRig, ChatRecord) {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    (rig, chat)
}

fn repo(rig: &QuotaRig) -> &dyn ChatVectorStoreRepo {
    &*rig.db.provider
}

fn inserted(result: PlaceholderInsert) -> Uuid {
    match result {
        PlaceholderInsert::Inserted(id) => id,
        PlaceholderInsert::AlreadyExists => panic!("the placeholder insert lost"),
    }
}

#[tokio::test]
async fn find_is_none_before_any_store_exists() {
    // inst-vs-select
    let (rig, chat) = rig().await;
    assert_eq!(repo(&rig).find(TENANT_A, chat.id).await.unwrap(), None);
}

#[tokio::test]
async fn find_returns_the_row_of_the_chat() {
    let (rig, chat) = rig().await;
    let id = seed_vector_store(&rig.db, &chat, Some("vs_1"), "azure_openai", now()).await;
    let row = repo(&rig)
        .find(TENANT_A, chat.id)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.id, id);
    assert_eq!(row.vector_store_id.as_deref(), Some("vs_1"));
    assert_eq!(row.provider, "azure_openai");
    assert_eq!(row.file_count, 0);
    assert_eq!((row.tenant_id, row.chat_id), (TENANT_A, chat.id));
    assert_eq!(
        row.created_at,
        now(),
        "sub-second precision survives the round trip"
    );
}

#[tokio::test]
async fn find_never_crosses_a_tenant_or_a_chat() {
    // tenant isolation: every query filters on tenant_id and chat_id
    let (rig, chat) = rig().await;
    let other_chat = rig.chat_a1(STANDARD).await;
    seed_vector_store(&rig.db, &chat, Some("vs_1"), "azure_openai", now()).await;
    assert_eq!(repo(&rig).find(TENANT_B, chat.id).await.unwrap(), None);
    assert_eq!(
        repo(&rig).find(TENANT_A, other_chat.id).await.unwrap(),
        None
    );
    assert_eq!(
        repo(&rig).find(TENANT_A, Uuid::new_v4()).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn the_placeholder_insert_writes_a_null_store_id_with_the_provider_label() {
    // inst-vs-insert
    let (rig, chat) = rig().await;
    let id = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    let rows = vector_store_rows(&rig.db).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].vector_store_id, None);
    assert_eq!(rows[0].provider, "azure_openai");
    assert_eq!(rows[0].file_count, 0);
    assert_eq!((rows[0].tenant_id, rows[0].chat_id), (TENANT_A, chat.id));
    assert_eq!(rows[0].created_at, now());
}

#[tokio::test]
async fn a_second_insert_for_the_chat_loses_without_an_error_and_without_a_row() {
    // inst-vs-lost: UNIQUE(tenant_id, chat_id)
    let (rig, chat) = rig().await;
    inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    let second = repo(&rig)
        .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
        .await
        .unwrap();
    assert_eq!(second, PlaceholderInsert::AlreadyExists);
    assert_eq!(vector_store_rows(&rig.db).await.len(), 1);
}

#[tokio::test]
async fn the_insert_loses_against_a_bound_store_as_well() {
    let (rig, chat) = rig().await;
    seed_vector_store(&rig.db, &chat, Some("vs_1"), "azure_openai", now()).await;
    let result = repo(&rig)
        .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
        .await
        .unwrap();
    assert_eq!(result, PlaceholderInsert::AlreadyExists);
}

#[tokio::test]
async fn two_concurrent_first_inserts_produce_exactly_one_winner() {
    let (rig, chat) = rig().await;
    let (first, second) = tokio::join!(
        repo(&rig).insert_placeholder(TENANT_A, chat.id, "azure_openai", now()),
        repo(&rig).insert_placeholder(TENANT_A, chat.id, "azure_openai", now()),
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    let winners = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, PlaceholderInsert::Inserted(_)))
        .count();
    assert_eq!(winners, 1, "{outcomes:?}");
    assert_eq!(vector_store_rows(&rig.db).await.len(), 1);
}

#[tokio::test]
async fn chats_have_independent_stores() {
    let (rig, chat) = rig().await;
    let other = rig.chat_a1(STANDARD).await;
    inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, other.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    assert_eq!(vector_store_rows(&rig.db).await.len(), 2);
}

#[tokio::test]
async fn the_placeholder_delete_is_guarded_by_id_chat_tenant_and_a_null_store_id() {
    // inst-vs-reclaim: guarded by id, tenant_id, chat_id and vector_store_id IS NULL
    let (rig, chat) = rig().await;
    let id = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    assert!(
        !repo(&rig)
            .delete_placeholder(TENANT_B, chat.id, id)
            .await
            .unwrap(),
        "tenant"
    );
    assert!(
        !repo(&rig)
            .delete_placeholder(TENANT_A, Uuid::new_v4(), id)
            .await
            .unwrap(),
        "chat"
    );
    assert!(
        !repo(&rig)
            .delete_placeholder(TENANT_A, chat.id, Uuid::new_v4())
            .await
            .unwrap(),
        "id"
    );
    assert_eq!(vector_store_rows(&rig.db).await.len(), 1);
    assert!(
        repo(&rig)
            .delete_placeholder(TENANT_A, chat.id, id)
            .await
            .unwrap()
    );
    assert!(vector_store_rows(&rig.db).await.is_empty());
    assert!(
        !repo(&rig)
            .delete_placeholder(TENANT_A, chat.id, id)
            .await
            .unwrap(),
        "already gone"
    );
}

#[tokio::test]
async fn a_bound_store_is_never_deleted_as_a_placeholder() {
    let (rig, chat) = rig().await;
    let id = seed_vector_store(&rig.db, &chat, Some("vs_1"), "azure_openai", now()).await;
    assert!(
        !repo(&rig)
            .delete_placeholder(TENANT_A, chat.id, id)
            .await
            .unwrap()
    );
    assert_eq!(vector_store_rows(&rig.db).await.len(), 1);
}

#[tokio::test]
async fn the_compare_and_set_binds_the_store_once() {
    // inst-vs-cas
    let (rig, chat) = rig().await;
    let id = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    assert!(
        repo(&rig)
            .set_vector_store_id(TENANT_A, chat.id, id, "vs_first")
            .await
            .unwrap()
    );
    assert!(
        !repo(&rig)
            .set_vector_store_id(TENANT_A, chat.id, id, "vs_second")
            .await
            .unwrap()
    );
    let rows = vector_store_rows(&rig.db).await;
    assert_eq!(rows[0].vector_store_id.as_deref(), Some("vs_first"));
}

#[tokio::test]
async fn the_compare_and_set_affects_no_row_after_the_placeholder_was_reclaimed() {
    // inst-vs-cas-lost
    let (rig, chat) = rig().await;
    let id = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    assert!(
        repo(&rig)
            .delete_placeholder(TENANT_A, chat.id, id)
            .await
            .unwrap()
    );
    assert!(
        !repo(&rig)
            .set_vector_store_id(TENANT_A, chat.id, id, "vs_late")
            .await
            .unwrap()
    );
    assert!(vector_store_rows(&rig.db).await.is_empty());
}

#[tokio::test]
async fn the_compare_and_set_is_guarded_by_tenant_chat_and_row_id() {
    let (rig, chat) = rig().await;
    let id = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    assert!(
        !repo(&rig)
            .set_vector_store_id(TENANT_B, chat.id, id, "vs_x")
            .await
            .unwrap()
    );
    assert!(
        !repo(&rig)
            .set_vector_store_id(TENANT_A, Uuid::new_v4(), id, "vs_x")
            .await
            .unwrap()
    );
    assert!(
        !repo(&rig)
            .set_vector_store_id(TENANT_A, chat.id, Uuid::new_v4(), "vs_x")
            .await
            .unwrap()
    );
    assert_eq!(vector_store_rows(&rig.db).await[0].vector_store_id, None);
}

#[tokio::test]
async fn the_placeholder_is_reclaimed_and_the_chat_gets_a_new_one() {
    // state placeholder -> absent -> placeholder
    let (rig, chat) = rig().await;
    let old = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", at(1))
            .await
            .unwrap(),
    );
    assert!(
        repo(&rig)
            .delete_placeholder(TENANT_A, chat.id, old)
            .await
            .unwrap()
    );
    let fresh = inserted(
        repo(&rig)
            .insert_placeholder(TENANT_A, chat.id, "azure_openai", now())
            .await
            .unwrap(),
    );
    assert_ne!(old, fresh);
    assert_eq!(vector_store_rows(&rig.db).await.len(), 1);
}
