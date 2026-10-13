#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use chrono::Utc;
use sea_orm::IntoActiveModel;
use tokio::time::Instant;
use toolkit_db::secure::{AccessScope, secure_insert};
use uuid::Uuid;

use super::*;
use crate::infra::db::entities::chat_vector_store;
use crate::testing::{TestApp, TestUser};

const U: TestUser = TestUser::A1;

fn timings() -> UploadTimings {
    UploadTimings {
        vector_store_poll_initial: Duration::from_millis(10),
        ..UploadTimings::default()
    }
}

async fn app() -> TestApp {
    TestApp::builder().upload_timings(timings()).build().await
}

async fn chat(app: &TestApp) -> Uuid {
    let r = app
        .call(
            U,
            http::Method::POST,
            "/mini-chat/v1/chats",
            Some(serde_json::json!({})),
        )
        .await;
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn seed_placeholder(app: &TestApp, chat_id: Uuid, age: chrono::Duration) {
    let row = chat_vector_store::Model {
        id: Uuid::new_v4(),
        tenant_id: U.tenant_id,
        chat_id,
        vector_store_id: None,
        provider: "openai".to_owned(),
        file_count: 0,
        created_at: Utc::now() - age,
    };
    secure_insert::<chat_vector_store::Entity>(
        row.into_active_model(),
        &AccessScope::allow_all(),
        &app.db.conn().unwrap(),
    )
    .await
    .unwrap();
}

fn rag_provider(app: &TestApp) -> ResolvedProvider {
    app.services
        .providers
        .resolve_rag("openai", U.tenant_id)
        .unwrap()
}

#[tokio::test]
async fn vector_store_loser_polls_then_503() {
    let app = app().await;
    let chat_id = chat(&app).await;
    // A fresh placeholder: another upload is creating the store.
    seed_placeholder(&app, chat_id, chrono::Duration::seconds(1)).await;
    let started = Instant::now();
    let err = app
        .services
        .attachments
        .ensure_vector_store(U.tenant_id, chat_id, &rag_provider(&app))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::StorageUnavailable(_)), "{err:?}");
    // 5 polls: 10 + 20 + 40 + 80 + 160 ms.
    assert!(started.elapsed() >= Duration::from_millis(310));
    // The loser never calls the provider.
    assert!(app.provider.requests().is_empty());
    let rows = VectorStoreRepo::find(&app.db.conn().unwrap(), U.tenant_id, chat_id)
        .await
        .unwrap();
    assert!(rows.unwrap().vector_store_id.is_none());
}

#[tokio::test]
async fn vector_store_loser_gets_the_winners_store() {
    let app = app().await;
    let chat_id = chat(&app).await;
    seed_placeholder(&app, chat_id, chrono::Duration::seconds(1)).await;
    let row = VectorStoreRepo::find(&app.db.conn().unwrap(), U.tenant_id, chat_id)
        .await
        .unwrap()
        .unwrap();
    let db = app.db.clone();
    let winner = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        VectorStoreRepo::set_vector_store_id(
            &db.conn().unwrap(),
            U.tenant_id,
            chat_id,
            row.id,
            "vs_winner",
        )
        .await
        .unwrap()
    });
    let vs = app
        .services
        .attachments
        .ensure_vector_store(U.tenant_id, chat_id, &rag_provider(&app))
        .await
        .unwrap();
    assert!(winner.await.unwrap());
    assert_eq!(vs, "vs_winner");
    assert!(app.provider.vector_stores().is_empty());
}

#[tokio::test]
async fn stale_placeholder_reclaimed() {
    let app = app().await;
    let chat_id = chat(&app).await;
    // Its creator died more than 120 s ago between the insert and the CAS.
    seed_placeholder(&app, chat_id, chrono::Duration::seconds(121)).await;
    let vs = app
        .services
        .attachments
        .ensure_vector_store(U.tenant_id, chat_id, &rag_provider(&app))
        .await
        .unwrap();
    let stores = app.provider.vector_stores();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].id, vs);
    let row = VectorStoreRepo::find(&app.db.conn().unwrap(), U.tenant_id, chat_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.vector_store_id.as_deref(), Some(vs.as_str()));
    // Fast path afterwards.
    let again = app
        .services
        .attachments
        .ensure_vector_store(U.tenant_id, chat_id, &rag_provider(&app))
        .await
        .unwrap();
    assert_eq!(again, vs);
    assert_eq!(app.provider.vector_stores().len(), 1);
}

#[tokio::test]
async fn provider_mismatch_also_for_a_placeholder() {
    let app = app().await;
    let chat_id = chat(&app).await;
    let mut p = rag_provider(&app);
    seed_placeholder(&app, chat_id, chrono::Duration::seconds(1)).await;
    p.storage_backend = "azure-eu".to_owned();
    let err = app
        .services
        .attachments
        .ensure_vector_store(U.tenant_id, chat_id, &p)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::ProviderMismatch), "{err:?}");
}

#[tokio::test]
async fn validate_part_rules() {
    let app = TestApp::builder().build().await;
    let chat_id = chat(&app).await;
    let up = app
        .services
        .attachments
        .prepare_upload(&U.security_context(), chat_id)
        .await
        .unwrap();
    assert_eq!(up.document_limit_bytes, 25 * 1024 * 1024);
    assert_eq!(up.image_limit_bytes, 5120 * 1024);
    assert!(up.code_interpreter);

    let m = up
        .validate_part(
            Some("dir/sub\\Report.PDF"),
            Some("application/octet-stream"),
        )
        .unwrap();
    assert_eq!(m.filename, "Report.PDF");
    assert_eq!(m.content_type, "application/pdf");
    assert_eq!(m.kind, AttachmentKind::Document);
    assert!(m.for_file_search && !m.for_code_interpreter);
    assert_eq!(m.limit_bytes, up.document_limit_bytes);

    let m = up.validate_part(None, Some("image/png")).unwrap();
    assert_eq!(m.filename, "upload");
    assert_eq!(m.limit_bytes, up.image_limit_bytes);
    assert!(!m.for_file_search && !m.for_code_interpreter);

    let m = up
        .validate_part(Some("s.xlsx"), Some(crate::domain::mime::XLSX_MIME))
        .unwrap();
    assert!(m.for_code_interpreter && !m.for_file_search);

    assert!(matches!(
        up.validate_part(Some("a.pdf"), None),
        Err(DomainError::Multipart {
            reason: "MISSING_CONTENT_TYPE",
            field: "content_type",
            ..
        })
    ));
    assert!(matches!(
        up.validate_part(Some("a.bin"), Some("application/x-foo")),
        Err(DomainError::UnsupportedContentType(_))
    ));
}

#[tokio::test]
async fn concurrency_permit_is_held_by_the_upload_context() {
    let app = TestApp::builder()
        .config(|c| c.rag.max_concurrent_uploads = 1)
        .build()
        .await;
    let chat_id = chat(&app).await;
    let ctx = U.security_context();
    let first = app
        .services
        .attachments
        .prepare_upload(&ctx, chat_id)
        .await
        .unwrap();
    assert!(matches!(
        app.services.attachments.prepare_upload(&ctx, chat_id).await,
        Err(DomainError::UploadConcurrencyLimit)
    ));
    drop(first);
    assert!(
        app.services
            .attachments
            .prepare_upload(&ctx, chat_id)
            .await
            .is_ok()
    );
}
