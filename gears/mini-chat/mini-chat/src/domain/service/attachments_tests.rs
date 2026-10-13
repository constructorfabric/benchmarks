#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use super::test_helpers::*;
use super::*;
use crate::domain::service::test_support::{
    TENANT_A, TENANT_B, TestEnv, TestOptions, USER_A1, USER_A2, USER_B, ctx, ctx_a1, model,
};
use crate::infra::llm::{FileStorage, StorageError};

const PDF: &str = "application/pdf";

fn cleanup_queue(env: &TestEnv) -> String {
    env.deps.cfg.outbox.cleanup_queue_name.clone()
}

fn assert_invalid(err: &DomainError, want_field: &str, want_reason: &str) {
    match err {
        DomainError::InvalidArgument { field, reason, .. } => {
            assert_eq!((field.as_str(), reason.as_str()), (want_field, want_reason), "{err:?}");
        }
        other => panic!("expected invalid_argument {want_reason}, got {other:?}"),
    }
}

fn assert_unavailable(err: &DomainError, secs: u64) {
    assert!(
        matches!(err, DomainError::ServiceUnavailable { retry_after_secs, .. } if *retry_after_secs == secs),
        "{err:?}"
    );
}

fn assert_exhausted(err: &DomainError, want: &str) {
    assert!(
        matches!(err, DomainError::ResourceExhausted { resource, subject, .. }
            if *resource == resource_types::ATTACHMENT && subject == want),
        "{err:?}"
    );
}

// ── Happy paths ────────────────────────────────────────────────────────────

#[tokio::test]
async fn pdf_upload_creates_vector_store_and_becomes_ready() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;

    let row = upload(&svc, &ctx_a1(), chat, "report.pdf", PDF, b"%PDF-1.4 hello")
        .await
        .unwrap();
    assert_eq!(row.status, status::READY);
    assert_eq!(row.attachment_kind, "document");
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.storage_backend, "openai");
    assert_eq!(row.content_type, PDF);
    assert_eq!(row.filename, "report.pdf");
    assert_eq!(row.size_bytes, 14);
    assert_eq!(row.uploaded_by_user_id, USER_A1);
    let file_id = row.provider_file_id.clone().expect("provider file id");

    let uploads = calls(&env.storage, "upload ");
    assert_eq!(uploads.len(), 1);
    assert_eq!(
        uploads[0],
        format!("upload openai {chat}_{}.pdf {PDF} 14", row.id)
    );
    assert_eq!(calls(&env.storage, "create_vs").len(), 1);
    let vs = vector_stores(&env, TENANT_A, chat).await;
    assert_eq!(vs.len(), 1);
    let vs_id = vs[0].vector_store_id.clone().expect("vs id");
    assert_eq!(vs[0].provider, "openai");
    assert_eq!(
        calls(&env.storage, "add_vs_file"),
        vec![format!("add_vs_file {vs_id} {file_id}")]
    );

    // A second document reuses the chat's store.
    upload(&svc, &ctx_a1(), chat, "b.md", "text/markdown", b"# hi")
        .await
        .unwrap();
    assert_eq!(calls(&env.storage, "create_vs").len(), 1);
    assert_eq!(calls(&env.storage, "add_vs_file").len(), 2);
    env.shutdown().await;
}

#[tokio::test]
async fn xlsx_is_code_interpreter_only_and_skips_vector_store() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let row = upload(&svc, &ctx_a1(), chat, "t.xlsx", mime::XLSX, b"PK..")
        .await
        .unwrap();
    assert_eq!(row.status, status::READY);
    assert!(!row.for_file_search && row.for_code_interpreter);
    assert!(calls(&env.storage, "create_vs").is_empty());
    assert!(calls(&env.storage, "add_vs_file").is_empty());
    assert!(vector_stores(&env, TENANT_A, chat).await.is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn image_gets_thumbnail_and_no_vector_store() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let row = upload(&svc, &ctx_a1(), chat, "p.png", "image/png", &png(256, 128))
        .await
        .unwrap();
    assert_eq!(row.status, status::READY);
    assert_eq!(row.attachment_kind, "image");
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert_eq!((row.img_thumbnail_width, row.img_thumbnail_height), (Some(128), Some(64)));
    let thumb = row.img_thumbnail.clone().expect("thumbnail");
    assert_eq!(&thumb[0..4], b"RIFF");
    assert!(calls(&env.storage, "create_vs").is_empty());

    // An undecodable image still becomes ready, without a thumbnail.
    let row = upload(&svc, &ctx_a1(), chat, "bad.png", "image/png", b"not a png")
        .await
        .unwrap();
    assert_eq!(row.status, status::READY);
    assert!(row.img_thumbnail.is_none() && row.error_code.is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn csv_and_octet_stream_inference() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let row = upload(&svc, &ctx_a1(), chat, "d.csv", "text/csv", b"a,b\n1,2")
        .await
        .unwrap();
    assert_eq!(row.content_type, "text/plain");
    let up = calls(&env.storage, "upload ");
    assert!(up[0].ends_with(".txt text/plain 7"), "{up:?}");

    let row = upload(&svc, &ctx_a1(), chat, "s.xlsx", "application/octet-stream", b"PK")
        .await
        .unwrap();
    assert_eq!(row.content_type, mime::XLSX);
    assert!(row.for_code_interpreter);

    let err = upload(&svc, &ctx_a1(), chat, "blob.bin", "application/octet-stream", b"x")
        .await
        .unwrap_err();
    assert_invalid(&err, "content_type", reasons::UNSUPPORTED_CONTENT_TYPE);
    let err = upload(&svc, &ctx_a1(), chat, "a.zip", "application/zip", b"x")
        .await
        .unwrap_err();
    assert_invalid(&err, "content_type", reasons::UNSUPPORTED_CONTENT_TYPE);
    env.shutdown().await;
}

#[tokio::test]
async fn csv_rejected_when_disabled() {
    let mut opts = TestOptions::default();
    opts.cfg.rag.allow_csv_upload = false;
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let err = upload(&svc, &ctx_a1(), chat, "d.csv", "text/csv", b"a")
        .await
        .unwrap_err();
    assert_invalid(&err, "content_type", reasons::UNSUPPORTED_CONTENT_TYPE);
    env.shutdown().await;
}

// ── Policy checks ──────────────────────────────────────────────────────────

#[tokio::test]
async fn images_kill_switch() {
    let mut opts = TestOptions::default();
    opts.kill_switches.disable_images = true;
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let err = upload(&svc, &ctx_a1(), chat, "p.png", "image/png", &png(4, 4))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DomainError::FailedPrecondition { subject, kind, .. }
            if subject == "images" && kind == reasons::FEATURE_DISABLED),
        "{err:?}"
    );
    // Documents are unaffected.
    upload(&svc, &ctx_a1(), chat, "a.txt", "text/plain", b"x").await.unwrap();
    env.shutdown().await;
}

#[tokio::test]
async fn xlsx_rejected_when_code_interpreter_unavailable() {
    // Kill switch.
    let mut opts = TestOptions::default();
    opts.kill_switches.disable_code_interpreter = true;
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let err = upload(&svc, &ctx_a1(), chat, "t.xlsx", mime::XLSX, b"PK")
        .await
        .unwrap_err();
    assert_invalid(&err, "file", reasons::CODE_INTERPRETER_UNAVAILABLE);
    assert!(matches!(err, DomainError::InvalidArgument { resource, .. } if resource == resource_types::ATTACHMENT));
    env.shutdown().await;

    // Model capability.
    let mut opts = TestOptions::default();
    let mut m = model("no-ci", "standard");
    m.general_config.tool_support.code_interpreter = false;
    opts.catalog.push(m);
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "no-ci").await;
    let err = upload(&svc, &ctx_a1(), chat, "t.xlsx", mime::XLSX, b"PK")
        .await
        .unwrap_err();
    assert_invalid(&err, "file", reasons::CODE_INTERPRETER_UNAVAILABLE);
    env.shutdown().await;
}

#[tokio::test]
async fn size_limit_is_min_of_config_and_model() {
    let mut opts = TestOptions::default();
    let mut m = model("small", "standard");
    m.general_config.max_file_size_mb = 1;
    opts.catalog.push(m);
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat_small = insert_chat(&env, USER_A1, TENANT_A, "small").await;
    let chat_big = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;

    let t = svc.prepare_upload(&ctx_a1(), chat_small).await.unwrap();
    assert_eq!(svc.validate_file(&t, PDF, Some("a.pdf")).unwrap().max_bytes, 1024 * 1024);
    assert_eq!(svc.validate_file(&t, "image/png", Some("a.png")).unwrap().max_bytes, 1024 * 1024);
    let t = svc.prepare_upload(&ctx_a1(), chat_big).await.unwrap();
    assert_eq!(svc.validate_file(&t, PDF, None).unwrap().max_bytes, 25_600 * 1024);
    let spec = svc.validate_file(&t, "image/png", None).unwrap();
    assert_eq!(spec.max_bytes, 5_120 * 1024);
    assert_eq!(spec.filename, "upload");

    // The service re-checks the size of the data it is given.
    let t = svc.prepare_upload(&ctx_a1(), chat_small).await.unwrap();
    let spec = svc.validate_file(&t, "text/plain", Some("a.txt")).unwrap();
    let err = svc
        .upload(
            &ctx_a1(),
            &t,
            spec,
            bytes::Bytes::from(vec![b'a'; 1024 * 1024 + 1]),
            std::time::Instant::now(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DomainError::OutOfRange { field, reason, .. }
            if field == "content_length" && reason == reasons::FILE_TOO_LARGE),
        "{err:?}"
    );
    env.shutdown().await;
}

#[tokio::test]
async fn chat_and_model_resolution_errors() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    // Other user in the same tenant / other tenant / unknown chat → 404 chat.
    for c in [ctx(USER_A2, TENANT_A), ctx(USER_B, TENANT_B)] {
        let err = svc.prepare_upload(&c, chat).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound { resource } if resource == resource_types::CHAT), "{err:?}");
    }
    let err = svc.prepare_upload(&ctx_a1(), Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(err, DomainError::NotFound { resource } if resource == resource_types::CHAT));
    // Model gone from the catalog.
    let gone = insert_chat(&env, USER_A1, TENANT_A, "removed-model").await;
    let err = svc.prepare_upload(&ctx_a1(), gone).await.unwrap_err();
    assert_invalid(&err, "model", reasons::INVALID_MODEL);
    // A disabled model is still accepted (resolved without the enabled filter).
    let disabled = insert_chat(&env, USER_A1, TENANT_A, "gpt-disabled").await;
    svc.prepare_upload(&ctx_a1(), disabled).await.unwrap();
    env.shutdown().await;
}

// ── Per-chat limits ────────────────────────────────────────────────────────

#[tokio::test]
async fn document_limit_counts_only_live_documents() {
    let mut opts = TestOptions::default();
    opts.cfg.rag.max_documents_per_chat = 2;
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    // A failed and a deleted document do not count.
    insert_attachment(&env, TENANT_A, chat, USER_A1, |a| a.status = Set("failed".into())).await;
    insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.deleted_at = Set(Some(OffsetDateTime::now_utc()));
    })
    .await;
    upload(&svc, &ctx_a1(), chat, "1.txt", "text/plain", b"1").await.unwrap();
    upload(&svc, &ctx_a1(), chat, "2.txt", "text/plain", b"2").await.unwrap();
    let err = upload(&svc, &ctx_a1(), chat, "3.txt", "text/plain", b"3")
        .await
        .unwrap_err();
    assert_exhausted(&err, reasons::DOCUMENT_LIMIT);
    // Images are not subject to the document limit.
    upload(&svc, &ctx_a1(), chat, "p.png", "image/png", &png(4, 4))
        .await
        .unwrap();
    // The rejected upload did not insert a row nor call the provider.
    assert_eq!(calls(&env.storage, "upload ").len(), 3);
    env.shutdown().await;
}

#[tokio::test]
async fn storage_limit_counts_images_too() {
    let mut opts = TestOptions::default();
    opts.cfg.rag.max_total_upload_mb_per_chat = 1;
    let env = TestEnv::new(opts).await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.attachment_kind = Set("image".into());
        a.content_type = Set("image/png".into());
        a.size_bytes = Set(600 * 1024);
    })
    .await;
    // Exactly at the limit is allowed.
    upload(&svc, &ctx_a1(), chat, "a.txt", "text/plain", &vec![b'a'; 424 * 1024])
        .await
        .unwrap();
    let err = upload(&svc, &ctx_a1(), chat, "p.png", "image/png", &png(4, 4))
        .await
        .unwrap_err();
    assert_exhausted(&err, reasons::STORAGE_LIMIT);
    env.shutdown().await;
}

// ── Failures ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn provider_upload_failure_marks_row_failed() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.upload_error.lock() = Some(StorageError::Http {
        status: 500,
        message: "boom".into(),
    });
    let err = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap_err();
    assert_unavailable(&err, 10);
    if let DomainError::ServiceUnavailable { detail, .. } = &err {
        assert_eq!(detail, STORAGE_UNAVAILABLE_DETAIL);
    }
    let rows = all_rows(&env, chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, status::FAILED);
    assert_eq!(rows[0].error_code.as_deref(), Some(error_codes::UPLOAD_FAILED));
    assert!(rows[0].provider_file_id.is_none());
    env.shutdown().await;
}

async fn all_rows(env: &TestEnv, chat: Uuid) -> Vec<attachment::Model> {
    let conn = env.deps.db.conn().unwrap();
    attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT_A))
        .all(&conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn indexing_failure_marks_row_failed_and_deletes_file() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.file_status.lock() = VectorFileStatus::Failed("failed".into());
    let err = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap_err();
    assert_unavailable(&err, 10);
    let rows = all_rows(&env, chat).await;
    assert_eq!(rows[0].status, status::FAILED);
    assert_eq!(rows[0].error_code.as_deref(), Some(error_codes::INDEXING_FAILED));
    let file_id = rows[0].provider_file_id.clone().unwrap();
    let deletes = wait_calls(&env.storage, "delete_file", 1).await;
    assert_eq!(deletes, vec![format!("delete_file {file_id}")]);
    env.shutdown().await;
}

#[tokio::test]
async fn add_to_vector_store_failure_is_indexing_failed() {
    let env = TestEnv::default_env().await;
    let storage = Arc::new(ScriptStorage::new(Arc::clone(&env.storage)));
    *storage.add_error.lock() = Some(StorageError::Http {
        status: 400,
        message: "bad".into(),
    });
    let svc = AttachmentService::new(deps_with_storage(&env, storage)).with_timings(fast_timings());
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let err = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap_err();
    assert_unavailable(&err, 10);
    let rows = all_rows(&env, chat).await;
    assert_eq!(rows[0].error_code.as_deref(), Some(error_codes::INDEXING_FAILED));
    assert_eq!(wait_calls(&env.storage, "delete_file", 1).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn transient_status_errors_keep_polling() {
    let env = TestEnv::default_env().await;
    let storage = Arc::new(ScriptStorage::new(Arc::clone(&env.storage)));
    *env.storage.file_status.lock() = VectorFileStatus::InProgress;
    {
        let mut s = storage.statuses.lock();
        s.push_back(Err(StorageError::Transport("gw".into())));
        s.push_back(Err(StorageError::Http { status: 503, message: "x".into() }));
        s.push_back(Ok(VectorFileStatus::InProgress));
        s.push_back(Ok(VectorFileStatus::Completed));
    }
    let svc = AttachmentService::new(deps_with_storage(&env, Arc::clone(&storage) as Arc<dyn FileStorage>))
        .with_timings(fast_timings());
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    assert_eq!(row.status, status::READY);
    assert_eq!(*storage.status_reads.lock(), 4);

    // A non-transient read error fails the upload.
    storage
        .statuses
        .lock()
        .push_back(Err(StorageError::Http { status: 403, message: "x".into() }));
    let err = upload(&svc, &ctx_a1(), chat, "b.pdf", PDF, b"x").await.unwrap_err();
    assert_unavailable(&err, 10);
    env.shutdown().await;
}

// ── Background indexing ────────────────────────────────────────────────────

#[tokio::test]
async fn still_indexing_at_deadline_returns_uploaded_then_ready() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.file_status.lock() = VectorFileStatus::InProgress;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    assert_eq!(row.status, status::UPLOADED);
    assert!(row.provider_file_id.is_some());
    let before = row.updated_at;

    // Heartbeat refreshes updated_at while waiting.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mid = row_now(&env, row.id).await;
    assert_eq!(mid.status, status::UPLOADED);
    assert!(mid.updated_at > before);

    *env.storage.file_status.lock() = VectorFileStatus::Completed;
    let done = wait_row(&env, TENANT_A, row.id, |r| r.status == status::READY).await;
    assert_eq!(done.status, status::READY);
    assert!(done.cleanup_status.is_none());
    env.shutdown().await;
}

async fn row_now(env: &TestEnv, id: Uuid) -> attachment::Model {
    row(env, TENANT_A, id).await
}

#[tokio::test]
async fn background_failure_marks_failed_and_enqueues_cleanup() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.file_status.lock() = VectorFileStatus::InProgress;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    assert_eq!(row.status, status::UPLOADED);
    *env.storage.file_status.lock() = VectorFileStatus::Failed("cancelled".into());
    let failed = wait_row(&env, TENANT_A, row.id, |r| r.status == status::FAILED).await;
    assert_eq!(failed.error_code.as_deref(), Some(error_codes::INDEXING_FAILED));
    assert_eq!(failed.cleanup_status.as_deref(), Some(cleanup_status::PENDING));
    assert!(failed.cleanup_updated_at.is_some(), "cleanup_updated_at set with pending");
    let events = env.delivered_to(&cleanup_queue(&env), 1).await;
    assert_eq!(events.len(), 1);
    let ev: AttachmentCleanupEvent = serde_json::from_value(events[0].clone()).unwrap();
    assert_eq!(ev.event_type, attachment_event_types::INDEXING_FAILED);
    assert_eq!(ev.attachment_id, row.id);
    assert_eq!(ev.chat_id, chat);
    assert_eq!(ev.tenant_id, TENANT_A);
    assert_eq!(ev.provider_file_id, row.provider_file_id);
    assert_eq!(ev.storage_backend, "openai");
    assert_eq!(ev.attachment_kind, "document");
    assert!(ev.vector_store_id.is_none() && ev.secondary_ref.is_none());
    // No inline delete: the outbox cleanup owns the provider file.
    assert!(calls(&env.storage, "delete_file").is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn background_timeout_marks_failed() {
    let env = TestEnv::default_env().await;
    let mut t = fast_timings();
    t.background_total = Duration::from_millis(300);
    let svc = AttachmentService::new(Arc::clone(&env.deps)).with_timings(t);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.file_status.lock() = VectorFileStatus::InProgress;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    let failed = wait_row(&env, TENANT_A, row.id, |r| r.status == status::FAILED).await;
    assert_eq!(failed.error_code.as_deref(), Some(error_codes::INDEXING_FAILED));
    assert_eq!(env.delivered_to(&cleanup_queue(&env), 1).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn background_stops_when_chat_deleted() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.file_status.lock() = VectorFileStatus::InProgress;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    soft_delete_chat(&env, TENANT_A, chat).await;
    *env.storage.file_status.lock() = VectorFileStatus::Completed;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let r = row_now(&env, row.id).await;
    assert_eq!(r.status, status::UPLOADED, "a row of a deleted chat never becomes ready");
    assert_eq!(r.cleanup_status.as_deref(), Some(cleanup_status::PENDING));
    assert!(env.delivered_to(&cleanup_queue(&env), 0).await.is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn background_stops_on_shutdown() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.file_status.lock() = VectorFileStatus::InProgress;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    env.deps.shutdown.cancel();
    env.deps.tasks.close();
    tokio::time::timeout(Duration::from_secs(2), env.deps.tasks.wait())
        .await
        .expect("background task stops on shutdown");
    assert_eq!(row_now(&env, row.id).await.status, status::UPLOADED);
    env.shutdown().await;
}

// ── Vector store protocol ──────────────────────────────────────────────────

#[tokio::test]
async fn provider_mismatch_rejected_before_upload() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    insert_vector_store(&env, TENANT_A, chat, Some("vs_other"), "azure", OffsetDateTime::now_utc()).await;
    let err = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap_err();
    assert!(
        matches!(&err, DomainError::AlreadyExists { name, resource, .. }
            if name == reasons::PROVIDER_MISMATCH && *resource == resource_types::ATTACHMENT),
        "{err:?}"
    );
    assert!(calls(&env.storage, "upload ").is_empty());
    // Images and code-interpreter files do not use the vector store.
    upload(&svc, &ctx_a1(), chat, "t.xlsx", mime::XLSX, b"PK").await.unwrap();
    env.shutdown().await;
}

#[tokio::test]
async fn stale_placeholder_is_reclaimed() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let old = OffsetDateTime::now_utc() - time::Duration::seconds(300);
    let placeholder = insert_vector_store(&env, TENANT_A, chat, None, "openai", old).await;
    let row = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap();
    assert_eq!(row.status, status::READY);
    let vs = vector_stores(&env, TENANT_A, chat).await;
    assert_eq!(vs.len(), 1);
    assert_ne!(vs[0].id, placeholder);
    assert!(vs[0].vector_store_id.is_some());
    assert_eq!(calls(&env.storage, "create_vs").len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn loser_gives_up_with_503_on_fresh_placeholder() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    insert_vector_store(&env, TENANT_A, chat, None, "openai", OffsetDateTime::now_utc()).await;
    let err = upload(&svc, &ctx_a1(), chat, "a.pdf", PDF, b"x").await.unwrap_err();
    assert_unavailable(&err, 10);
    assert!(calls(&env.storage, "create_vs").is_empty(), "the loser never creates a store");
    let rows = all_rows(&env, chat).await;
    assert_eq!(rows[0].status, status::FAILED);
    assert_eq!(rows[0].error_code.as_deref(), Some(error_codes::VECTOR_STORE_FAILED));
    env.shutdown().await;
}

#[tokio::test]
async fn concurrent_first_uploads_create_one_store() {
    let env = TestEnv::default_env().await;
    let mut t = fast_timings();
    t.vs_loser_polls = 6;
    let svc = Arc::new(AttachmentService::new(Arc::clone(&env.deps)).with_timings(t));
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let mut handles = Vec::new();
    for i in 0..4 {
        let svc = Arc::clone(&svc);
        handles.push(tokio::spawn(async move {
            upload(&svc, &ctx_a1(), chat, &format!("{i}.txt"), "text/plain", b"x").await
        }));
    }
    for h in handles {
        let r = h.await.unwrap().unwrap();
        assert_eq!(r.status, status::READY);
    }
    assert_eq!(calls(&env.storage, "create_vs").len(), 1);
    assert_eq!(vector_stores(&env, TENANT_A, chat).await.len(), 1);
    env.shutdown().await;
}

// ── GET / DELETE ───────────────────────────────────────────────────────────

#[tokio::test]
async fn get_applies_chat_uploader_and_deletion_checks() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let other_chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let own = insert_attachment(&env, TENANT_A, chat, USER_A1, |_| {}).await;
    let foreign = insert_attachment(&env, TENANT_A, chat, USER_A2, |_| {}).await;
    let deleted = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.deleted_at = Set(Some(OffsetDateTime::now_utc()));
    })
    .await;

    assert_eq!(svc.get(&ctx_a1(), chat, own).await.unwrap().id, own);
    for (c, id) in [(chat, foreign), (chat, deleted), (other_chat, own), (chat, Uuid::new_v4())] {
        let err = svc.get(&ctx_a1(), c, id).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound { resource } if resource == resource_types::ATTACHMENT), "{err:?}");
    }
    // Another user of the tenant cannot see the chat at all.
    let err = svc.get(&ctx(USER_A2, TENANT_A), chat, own).await.unwrap_err();
    assert!(matches!(err, DomainError::NotFound { resource } if resource == resource_types::CHAT));
    env.shutdown().await;
}

#[tokio::test]
async fn delete_soft_deletes_and_enqueues_once() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let row = upload(&svc, &ctx_a1(), chat, "p.png", "image/png", &png(8, 8)).await.unwrap();

    svc.delete(&ctx_a1(), chat, row.id).await.unwrap();
    let r = row_now(&env, row.id).await;
    assert!(r.deleted_at.is_some());
    assert_eq!(r.cleanup_status.as_deref(), Some(cleanup_status::PENDING));
    assert_eq!(r.cleanup_updated_at, r.deleted_at, "cleanup_updated_at set with pending");
    let events = env.delivered_to(&cleanup_queue(&env), 1).await;
    assert_eq!(events.len(), 1);
    let ev: AttachmentCleanupEvent = serde_json::from_value(events[0].clone()).unwrap();
    assert_eq!(ev.event_type, attachment_event_types::DELETED);
    assert_eq!(ev.provider_file_id, row.provider_file_id);
    assert_eq!(ev.attachment_kind, "image");
    assert_eq!(ev.storage_backend, "openai");
    assert!(ev.vector_store_id.is_none() && ev.secondary_ref.is_none());

    // Idempotent: 204 again, no second event.
    svc.delete(&ctx_a1(), chat, row.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.delivered_to(&cleanup_queue(&env), 1).await.len(), 1);
    // GET now returns 404.
    assert!(matches!(svc.get(&ctx_a1(), chat, row.id).await, Err(DomainError::NotFound { .. })));
    env.shutdown().await;
}

#[tokio::test]
async fn delete_rejects_locked_and_foreign_attachments() {
    let env = TestEnv::default_env().await;
    let svc = fast_service(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let locked = insert_attachment(&env, TENANT_A, chat, USER_A1, |_| {}).await;
    link_to_message(&env, TENANT_A, chat, locked).await;
    let err = svc.delete(&ctx_a1(), chat, locked).await.unwrap_err();
    assert!(
        matches!(&err, DomainError::AlreadyExists { name, resource, .. }
            if name == reasons::ATTACHMENT_LOCKED && *resource == resource_types::ATTACHMENT),
        "{err:?}"
    );
    assert!(row_now(&env, locked).await.deleted_at.is_none());

    // Uploaded by another user (even if already deleted) → 404.
    let foreign = insert_attachment(&env, TENANT_A, chat, USER_A2, |a| {
        a.deleted_at = Set(Some(OffsetDateTime::now_utc()));
    })
    .await;
    let err = svc.delete(&ctx_a1(), chat, foreign).await.unwrap_err();
    assert!(matches!(err, DomainError::NotFound { resource } if resource == resource_types::ATTACHMENT));
    let err = svc.delete(&ctx_a1(), chat, Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(err, DomainError::NotFound { resource } if resource == resource_types::ATTACHMENT));

    // An attachment without provider file is deleted with a null provider_file_id.
    let pending = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.provider_file_id = Set(None);
        a.status = Set("pending".into());
    })
    .await;
    svc.delete(&ctx_a1(), chat, pending).await.unwrap();
    let events = env.delivered_to(&cleanup_queue(&env), 1).await;
    assert_eq!(events.len(), 1);
    assert!(events[0]["provider_file_id"].is_null());
    env.shutdown().await;
}


/// Storage whose provider upload runs while the chat is deleted (cleanup claims the row).
struct DeleteChatDuringUpload {
    inner: Arc<crate::domain::service::test_support::FakeStorage>,
    env_deps: Arc<crate::domain::service::Deps>,
    chat: parking_lot::Mutex<Option<Uuid>>,
}

#[async_trait::async_trait]
impl FileStorage for DeleteChatDuringUpload {
    async fn upload_file(
        &self,
        p: &str,
        t: Uuid,
        filename: &str,
        ct: &str,
        data: bytes::Bytes,
    ) -> Result<String, StorageError> {
        let id = self.inner.upload_file(p, t, filename, ct, data).await?;
        let chat = *self.chat.lock();
        if let Some(chat) = chat {
            use sea_orm::sea_query::Expr;
            use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
            use toolkit_db::secure::SecureUpdateExt;
            let conn = self.env_deps.db.conn().unwrap();
            crate::infra::db::entity::attachment::Entity::update_many()
                .col_expr(crate::infra::db::entity::attachment::Column::CleanupStatus, Expr::value("pending"))
                .filter(crate::infra::db::entity::attachment::Column::ChatId.eq(chat))
                .secure()
                .scope_with(&toolkit_security::AccessScope::for_tenant(t))
                .exec(&conn)
                .await
                .unwrap();
        }
        Ok(id)
    }
    async fn delete_file(&self, p: &str, t: Uuid, f: &str) -> Result<(), StorageError> {
        self.inner.delete_file(p, t, f).await
    }
    async fn create_vector_store(&self, p: &str, t: Uuid, n: &str) -> Result<String, StorageError> {
        self.inner.create_vector_store(p, t, n).await
    }
    async fn add_file_to_vector_store(
        &self,
        p: &str,
        t: Uuid,
        vs: &str,
        f: &str,
        a: std::collections::BTreeMap<String, String>,
    ) -> Result<crate::infra::llm::VectorFileStatus, StorageError> {
        self.inner.add_file_to_vector_store(p, t, vs, f, a).await
    }
    async fn get_vector_store_file_status(
        &self,
        p: &str,
        t: Uuid,
        vs: &str,
        f: &str,
    ) -> Result<crate::infra::llm::VectorFileStatus, StorageError> {
        self.inner.get_vector_store_file_status(p, t, vs, f).await
    }
    async fn delete_vector_store(&self, p: &str, t: Uuid, vs: &str) -> Result<(), StorageError> {
        self.inner.delete_vector_store(p, t, vs).await
    }
}

#[tokio::test]
async fn upload_does_not_touch_a_row_claimed_by_cleanup() {
    let env = TestEnv::default_env().await;
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let storage = Arc::new(DeleteChatDuringUpload {
        inner: Arc::clone(&env.storage),
        env_deps: Arc::clone(&env.deps),
        chat: parking_lot::Mutex::new(Some(chat)),
    });
    let deps = deps_with_storage(&env, storage);
    let svc = AttachmentService::new(deps).with_timings(fast_timings());
    let _ = upload(&svc, &ctx_a1(), chat, "p.png", "image/png", &png(8, 8)).await;
    let rows = all_rows(&env, chat).await;
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    // The guarded update skipped the row: still pending, no provider file id recorded.
    assert_eq!(r.status, status::PENDING);
    assert!(r.provider_file_id.is_none());
    assert_eq!(r.cleanup_status.as_deref(), Some(cleanup_status::PENDING));
    // The provider file is deleted best effort instead.
    let deleted = wait_calls(&env.storage, "delete_file", 1).await;
    assert_eq!(deleted.len(), 1);
    env.shutdown().await;
}
