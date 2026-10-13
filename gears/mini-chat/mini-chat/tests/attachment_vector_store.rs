//! The creation protocol of the per-chat vector store under concurrency (DESIGN 3.7
//! `chat_vector_stores`, "Creation protocol"): exactly one creator per chat, losers wait for the
//! creator's store and never call the provider, a placeholder of a dead creator is reclaimed.
//!
//! Added with the S6 implementation: the S6 test stage covers the sequential paths only.

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

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::time::Duration;

use attachment_support::{
    AttachmentHarness, PDF, PDF_BYTES, StorageOp, assert_unavailable, id_of, ts,
};
use mini_chat::infra::db::entity::chat_vector_stores;
use mini_chat::infra::db::timestamp::to_db;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::secure::{SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

/// A placeholder row (`vector_store_id` NULL) of `chat` created at `created_at`.
async fn seed_placeholder(h: &AttachmentHarness, chat: Uuid, created_at: OffsetDateTime) {
    let conn = h.chat.db.conn().expect("db connection");
    secure_insert::<chat_vector_stores::Entity>(
        chat_vector_stores::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(common::TENANT_A),
            chat_id: Set(chat),
            vector_store_id: Set(None),
            provider: Set("openai".to_owned()),
            file_count: Set(0),
            created_at: Set(to_db(created_at)),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("seed placeholder");
}

#[tokio::test]
async fn parallel_first_uploads_create_exactly_one_vector_store() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;

    let (a, b, c) = tokio::join!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES),
        h.upload(chat, "b.pdf", PDF, PDF_BYTES),
        h.upload(chat, "c.pdf", PDF, PDF_BYTES),
    );
    for resp in [&a, &b, &c] {
        assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
        assert_eq!(resp.json()["status"], json!("ready"));
    }
    assert_eq!(
        h.storage.count(StorageOp::CreateVectorStore),
        1,
        "the unique (tenant, chat) row arbitrates: one provider store per chat"
    );
    let stores = h.stores_of(chat).await;
    assert_eq!(stores.len(), 1);
    let store = stores[0]
        .vector_store_id
        .clone()
        .expect("the store id is set");
    for add in h.storage.calls_to(StorageOp::AddVectorStoreFile) {
        assert_eq!(
            add.vector_store_id(),
            store,
            "every file goes to the one store"
        );
    }
    assert_eq!(h.storage.count(StorageOp::AddVectorStoreFile), 3);
}

#[tokio::test]
async fn a_fresh_placeholder_makes_the_upload_wait_and_then_503_without_a_provider_store() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    seed_placeholder(&h, chat, OffsetDateTime::now_utc()).await;

    let started = std::time::Instant::now();
    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_unavailable(&resp, "10");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "five reads with exponential backoff: {:?}",
        started.elapsed()
    );
    assert_eq!(
        h.storage.count(StorageOp::CreateVectorStore),
        0,
        "a loser never creates a store"
    );
    assert_eq!(h.storage.count(StorageOp::AddVectorStoreFile), 0);
    let rows = h.rows_of(chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("vector_store_failed"));
    h.storage.wait_calls(StorageOp::DeleteFile, 1).await;
    assert_eq!(
        h.stores_of(chat).await.len(),
        1,
        "the creator's placeholder is left alone"
    );
}

#[tokio::test]
async fn a_loser_uses_the_store_the_creator_publishes_while_it_waits() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    seed_placeholder(&h, chat, OffsetDateTime::now_utc()).await;

    let upload = h.upload(chat, "a.pdf", PDF, PDF_BYTES);
    let creator = async {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let conn = h.chat.db.conn().expect("db connection");
        chat_vector_stores::Entity::update_many()
            .col_expr(
                chat_vector_stores::Column::VectorStoreId,
                Expr::value("vs_published_by_creator"),
            )
            .filter(chat_vector_stores::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("the creator publishes its store");
    };
    let (resp, ()) = tokio::join!(upload, creator);
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    let id = id_of(&resp.json());
    assert_eq!(h.storage.count(StorageOp::CreateVectorStore), 0);
    let adds = h.storage.calls_to(StorageOp::AddVectorStoreFile);
    assert_eq!(adds.len(), 1);
    assert_eq!(adds[0].vector_store_id(), "vs_published_by_creator");
    assert_eq!(h.row(id).await.status, "ready");
}

#[tokio::test]
async fn the_placeholder_of_a_dead_creator_is_reclaimed() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    // far older than the 120 s after which a placeholder counts as abandoned
    seed_placeholder(&h, chat, ts(0)).await;

    let id = h.doc(chat, "a.pdf").await;
    assert_eq!(h.row(id).await.status, "ready");
    assert_eq!(
        h.storage.count(StorageOp::CreateVectorStore),
        1,
        "the upload restarted the protocol and created the store itself"
    );
    let stores = h.stores_of(chat).await;
    assert_eq!(stores.len(), 1);
    assert!(
        stores[0]
            .vector_store_id
            .as_deref()
            .is_some_and(|store| store.starts_with("vs_s6fake"))
    );
}
