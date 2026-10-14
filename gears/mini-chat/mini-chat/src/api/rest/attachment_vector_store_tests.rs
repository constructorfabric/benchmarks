//! The per-chat vector store protocol over the router with concurrent uploads: two first uploads
//! to one chat produce exactly one provider create and both attachments use the same
//! `vector_store_id`; the loser waits on the virtual timer and never calls provider create; a
//! failed create leaves the loser a `vector_store_failed` after its polls; no database
//! connection is held across a provider call (the rig has a single connection).
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; algorithm
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning`; state
//! `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`; definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-vector-store`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::json;

use crate::test_support::attachments::{
    AttachRigOptions, FilePart, PDF, assert_created, assert_storage_unavailable, pdf_bytes,
    rig_with, upload_request,
};
use crate::test_support::chats::a1;
use crate::test_support::provider::Gate;
use crate::test_support::storage_mock::{Answer, Route};

fn pdf_request(chat_id: uuid::Uuid, name: &str) -> axum::http::Request<axum::body::Body> {
    upload_request(chat_id, &a1(), &[FilePart::file(name, PDF, &pdf_bytes())])
}

#[tokio::test]
async fn two_concurrent_first_uploads_create_one_store_and_share_it() {
    // inst-vs-in-progress, inst-vs-loser-poll: the loser waits for the winner's store
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.queue(
        Route::VsCreate,
        Answer::gated(
            &gate,
            Answer::ok(&json!({ "id": "vs_shared", "object": "vector_store" })),
        ),
    );
    let winner = rig.spawn(pdf_request(chat.id, "winner.pdf"));
    rig.storage.wait_for(Route::VsCreate, 1).await;
    let loser = rig.spawn(pdf_request(chat.id, "loser.pdf"));
    // the loser found the placeholder and parks on its first 200 ms poll wait
    rig.time.wait_for_parked(1).await;
    assert_eq!(
        rig.storage.count_of(Route::VsCreate),
        1,
        "the loser called provider create"
    );
    gate.open();
    assert_created(&winner.await.unwrap(), "ready");
    rig.time.advance(Duration::from_millis(200));
    assert_created(&loser.await.unwrap(), "ready");
    assert_eq!(rig.storage.count_of(Route::VsCreate), 1);
    let stores = rig.vector_stores().await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs_shared"));
    let adds = rig.storage.calls_of(Route::VsAddFile);
    assert_eq!(adds.len(), 2);
    for add in adds {
        assert!(
            add.uri.contains("/vector_stores/vs_shared/files"),
            "{}",
            add.uri
        );
    }
}

#[tokio::test]
async fn the_loser_fails_with_503_when_the_winner_create_fails_and_never_creates() {
    // inst-vs-return-loser-failed: no store appeared within 5 polls
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage
        .queue(Route::VsCreate, Answer::gated(&gate, Answer::status(500)));
    let winner = rig.spawn(pdf_request(chat.id, "winner.pdf"));
    rig.storage.wait_for(Route::VsCreate, 1).await;
    let loser = rig.spawn(pdf_request(chat.id, "loser.pdf"));
    rig.time.wait_for_parked(1).await;
    gate.open();
    assert_storage_unavailable(&winner.await.unwrap());
    // five polls: the loser parks on 200, 400, 800, 1600 and 3200 ms in turn
    for _ in 0..5 {
        rig.time.wait_for_parked(1).await;
        rig.time.advance(Duration::from_secs(4));
    }
    assert_storage_unavailable(&loser.await.unwrap());
    assert_eq!(
        rig.storage.count_of(Route::VsCreate),
        1,
        "the loser created a store"
    );
    assert!(rig.vector_stores().await.is_empty());
    let failed = rig
        .rows(chat.id)
        .await
        .into_iter()
        .filter(|row| row.status == "failed")
        .count();
    assert_eq!(failed, 2);
}

#[tokio::test]
async fn no_database_connection_is_held_across_the_provider_create() {
    // the rig has one connection: a transaction held across the gated create would starve the
    // placeholder of the other request and the row reads of this test
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.queue(
        Route::VsCreate,
        Answer::gated(
            &gate,
            Answer::ok(&json!({ "id": "vs_x", "object": "vector_store" })),
        ),
    );
    let winner = rig.spawn(pdf_request(chat.id, "winner.pdf"));
    rig.storage.wait_for(Route::VsCreate, 1).await;
    // while the create is in flight the database is usable and shows the placeholder
    let stores = tokio::time::timeout(Duration::from_secs(5), rig.vector_stores())
        .await
        .expect("a connection is held across the provider call");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id, None);
    gate.open();
    assert_created(&winner.await.unwrap(), "ready");
}

#[tokio::test]
async fn the_placeholder_row_exists_before_the_provider_create_returns() {
    // inst-vs-insert commits the placeholder on its own before inst-vs-create
    let rig = rig_with(AttachRigOptions::new().manual_time()).await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.queue(
        Route::VsCreate,
        Answer::gated(
            &gate,
            Answer::ok(&json!({ "id": "vs_y", "object": "vector_store" })),
        ),
    );
    let winner = rig.spawn(pdf_request(chat.id, "winner.pdf"));
    rig.storage.wait_for(Route::VsCreate, 1).await;
    let stores = rig.vector_stores().await;
    assert_eq!(stores.len(), 1);
    assert_eq!(
        (stores[0].chat_id, stores[0].provider.as_str()),
        (chat.id, "azure_openai")
    );
    gate.open();
    assert_created(&winner.await.unwrap(), "ready");
    assert_eq!(
        rig.vector_stores().await[0].vector_store_id.as_deref(),
        Some("vs_y")
    );
}
