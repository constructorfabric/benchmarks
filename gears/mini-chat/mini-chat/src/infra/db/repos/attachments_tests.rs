#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::cast_possible_wrap, clippy::integer_division)]
//! Slice 1: the attachment store reads and the pending insert under the
//! per-chat limits (`cpt-cf-mini-chat-algo-attachments-insert-pending`).

use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::attachment_test_env::{AttEnv, AttOptions};
use crate::domain::error::DomainError;
use crate::domain::service::attachments::model::{AttachmentKind, AttachmentStatus, ChatLimits};
use crate::domain::service::attachments::ports::AttachmentRepository;
use crate::test_doubles::{OTHER_TENANT, USER, USER_TENANT};

const MIB: u64 = 1_048_576;

fn limits(documents: u32, total_mib: u64) -> ChatLimits {
    ChatLimits {
        max_documents: documents,
        max_total_bytes: total_mib * MIB,
    }
}

fn roomy() -> ChatLimits {
    limits(50, 100)
}

// ================================================================================================
// find
// ================================================================================================

#[tokio::test]
async fn find_returns_the_row_with_its_columns() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row.set("size_bytes", 77_i64)).await;

    let found = env.sea.find(&env.scope(), chat, id).await.unwrap().unwrap();

    assert_eq!(found.id, id);
    assert_eq!(found.tenant_id, USER_TENANT);
    assert_eq!(found.chat_id, chat);
    assert_eq!(found.uploaded_by_user_id, USER);
    assert_eq!(found.filename, "file1.pdf");
    assert_eq!(found.content_type, "text/plain");
    assert_eq!(found.size_bytes, 77);
    assert_eq!(found.status, AttachmentStatus::Ready);
    assert_eq!(found.kind, AttachmentKind::Document);
    assert_eq!(found.provider_file_id.as_deref(), Some("file-prov1"));
    assert!(found.for_file_search && !found.for_code_interpreter);
    assert_eq!(found.deleted_at, None);
    assert_eq!(found.secondary_status, "not_attempted");
}

#[tokio::test]
async fn find_returns_a_stored_thumbnail_with_its_size() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("attachment_kind", "image")
                .set("img_thumbnail", vec![1_u8, 2, 3])
                .set("img_thumbnail_width", 128_i32)
                .set("img_thumbnail_height", 64_i32)
        })
        .await;

    let found = env.sea.find(&env.scope(), chat, id).await.unwrap().unwrap();

    let thumbnail = found.thumbnail.expect("the thumbnail");
    assert_eq!(thumbnail.bytes, vec![1, 2, 3]);
    assert_eq!((thumbnail.width, thumbnail.height), (128, 64));
}

#[tokio::test]
async fn find_includes_soft_deleted_rows() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await;

    let found = env.sea.find(&env.scope(), chat, id).await.unwrap().unwrap();

    assert!(found.deleted_at.is_some());
}

#[tokio::test]
async fn find_is_bound_to_the_chat_and_the_tenant() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let other_chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;

    assert!(
        env.sea
            .find(&env.scope(), other_chat, id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        env.sea
            .find(&env.scope(), chat, Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        env.sea
            .find(&AccessScope::for_tenant(OTHER_TENANT), chat, id)
            .await
            .unwrap()
            .is_none(),
        "another tenant never sees the row"
    );
}

// ================================================================================================
// insert_pending
// ================================================================================================

#[tokio::test]
async fn a_pending_insert_into_a_deleted_chat_is_not_found_and_leaves_no_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.delete_chat(chat).await;
    let new = env.new_attachment(chat, AttachmentKind::Document, 10);

    let result = env.sea.insert_pending(&new, &roomy()).await;

    assert!(
        matches!(
            result,
            Err(DomainError::NotFound {
                resource: crate::domain::error::ResourceKind::Chat
            })
        ),
        "{result:?}"
    );
    assert_eq!(env.att_count().await, 0, "the insert is rolled back");
}

#[tokio::test]
async fn the_pending_row_has_exactly_the_documented_columns() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let new = env.new_attachment(chat, AttachmentKind::Document, 1234);

    let row = env.sea.insert_pending(&new, &roomy()).await.unwrap();

    let snap = env.att(new.id).await;
    let stamp = AttEnv::stamp(new.now);
    assert_eq!(snap.status, "pending");
    assert_eq!(snap.provider_file_id, None);
    assert_eq!(snap.error_code, None);
    assert_eq!(snap.kind, "document");
    assert!(snap.for_file_search && !snap.for_code_interpreter);
    assert_eq!(snap.content_type, "application/pdf");
    assert_eq!(snap.filename, "a.pdf");
    assert_eq!(snap.size_bytes, 1234);
    assert_eq!(snap.storage_backend, "openai");
    assert_eq!(snap.uploaded_by, USER);
    assert_eq!(snap.tenant, USER_TENANT);
    assert_eq!(snap.chat, chat);
    assert_eq!(snap.cleanup_status, None);
    assert_eq!(snap.cleanup_attempts, 0);
    assert_eq!(snap.last_cleanup_error, None);
    assert_eq!(snap.deleted_at, None);
    assert_eq!(snap.created_at, stamp);
    assert_eq!(snap.updated_at, stamp);
    assert_eq!(snap.secondary_status, "not_attempted");
    assert_eq!(snap.secondary_file_id, None);
    assert_eq!(snap.secondary_provider_kind, None);
    assert_eq!(snap.thumbnail, None);
    assert_eq!(snap.doc_summary, None);
    assert_eq!(snap.summary_model, None);
    assert_eq!(row.id, new.id);
    assert_eq!(row.status, AttachmentStatus::Pending);
    assert_eq!(
        row,
        env.sea
            .find(&env.scope(), chat, new.id)
            .await
            .unwrap()
            .unwrap()
    );
}

#[tokio::test]
async fn an_image_row_has_neither_purpose() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let new = env.new_attachment(chat, AttachmentKind::Image, 10);

    env.sea.insert_pending(&new, &roomy()).await.unwrap();

    let snap = env.att(new.id).await;
    assert_eq!(snap.kind, "image");
    assert!(!snap.for_file_search && !snap.for_code_interpreter);
}

#[tokio::test]
async fn the_new_document_counts_toward_the_document_limit() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row).await;

    let second = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(2, 100),
        )
        .await;
    let third = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(2, 100),
        )
        .await;

    assert!(second.is_ok(), "1 existing + the new one = 2 <= 2");
    assert!(
        matches!(third, Err(DomainError::DocumentLimit)),
        "{third:?}"
    );
    assert_eq!(
        env.att_count().await,
        2,
        "the rejected insert is rolled back"
    );
}

#[tokio::test]
async fn failed_and_deleted_documents_do_not_count() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row.set("status", "failed")).await;
    env.seed(chat, 2, |row| {
        row.set("deleted_at", crate::chat_test_env::ts(5))
    })
    .await;
    env.seed(chat, 3, |row| row).await;

    let inserted = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(2, 100),
        )
        .await;

    assert!(inserted.is_ok(), "only the ready one and the new one count");
}

#[tokio::test]
async fn pending_uploaded_and_ready_documents_all_count() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    for (n, status) in [(1, "pending"), (2, "uploaded"), (3, "ready")] {
        env.seed(chat, n, |row| row.set("status", status)).await;
    }

    let inserted = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(3, 100),
        )
        .await;

    assert!(matches!(inserted, Err(DomainError::DocumentLimit)));
}

#[tokio::test]
async fn images_are_not_counted_as_documents_and_are_not_blocked_by_the_document_limit() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| {
        row.set("attachment_kind", "image")
            .set("for_file_search", false)
    })
    .await;
    env.seed(chat, 2, |row| row).await;

    let image = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Image, 1),
            &limits(1, 100),
        )
        .await;
    let document = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(1, 100),
        )
        .await;

    assert!(image.is_ok(), "the document limit does not apply to images");
    assert!(matches!(document, Err(DomainError::DocumentLimit)));
}

#[tokio::test]
async fn the_document_limit_is_per_chat() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let other = env.chat("m-oa").await;
    env.seed(other, 1, |row| row).await;
    env.seed(other, 2, |row| row).await;

    let inserted = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(1, 100),
        )
        .await;

    assert!(inserted.is_ok());
}

#[tokio::test]
async fn the_new_row_counts_toward_the_storage_limit() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row.set("size_bytes", (MIB / 2) as i64))
        .await;

    let fits = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, (MIB / 2) as i64),
            &limits(50, 1),
        )
        .await;
    let over = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(50, 1),
        )
        .await;

    assert!(fits.is_ok(), "exactly at the limit is accepted");
    assert!(matches!(over, Err(DomainError::StorageLimit)), "{over:?}");
    assert_eq!(env.att_count().await, 2);
}

#[tokio::test]
async fn the_storage_limit_counts_images_too() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| {
        row.set("attachment_kind", "image")
            .set("for_file_search", false)
            .set("size_bytes", (MIB - 10) as i64)
    })
    .await;

    let over = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 11),
            &limits(50, 1),
        )
        .await;

    assert!(matches!(over, Err(DomainError::StorageLimit)));
}

#[tokio::test]
async fn failed_and_deleted_rows_do_not_count_toward_storage() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| {
        row.set("status", "failed").set("size_bytes", MIB as i64)
    })
    .await;
    env.seed(chat, 2, |row| {
        row.set("deleted_at", crate::chat_test_env::ts(5))
            .set("size_bytes", MIB as i64)
    })
    .await;

    let inserted = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, MIB as i64),
            &limits(50, 1),
        )
        .await;

    assert!(inserted.is_ok());
}

#[tokio::test]
async fn the_storage_limit_is_per_chat() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let other = env.chat("m-oa").await;
    env.seed(other, 1, |row| row.set("size_bytes", MIB as i64))
        .await;

    let inserted = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, MIB as i64),
            &limits(50, 1),
        )
        .await;

    assert!(inserted.is_ok());
}

#[tokio::test]
async fn the_document_limit_is_checked_before_the_storage_limit() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row.set("size_bytes", MIB as i64))
        .await;

    let result = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(1, 1),
        )
        .await;

    assert!(
        matches!(result, Err(DomainError::DocumentLimit)),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_rejected_insert_leaves_nothing_behind() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let first = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(1, 100),
        )
        .await;
    assert!(first.is_ok(), "{first:?}");
    let before = env.attachments_dump().await;

    let second = env
        .sea
        .insert_pending(
            &env.new_attachment(chat, AttachmentKind::Document, 1),
            &limits(1, 100),
        )
        .await;

    assert!(
        matches!(second, Err(DomainError::DocumentLimit)),
        "{second:?}"
    );
    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn concurrent_inserts_cannot_exceed_the_document_limit() {
    let env = AttEnv::with(AttOptions {
        file_db: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let news: Vec<_> = (0..4)
        .map(|_| env.new_attachment(chat, AttachmentKind::Document, 1))
        .collect();

    let limit = limits(1, 100);
    let results =
        futures::future::join_all(news.iter().map(|new| env.sea.insert_pending(new, &limit))).await;

    let accepted = results.iter().filter(|r| r.is_ok()).count();
    let rejected = results
        .iter()
        .filter(|r| matches!(r, Err(DomainError::DocumentLimit)))
        .count();
    assert_eq!(
        (accepted, rejected),
        (1, 3),
        "write-first inserts serialize on SQLite: {results:?}"
    );
    assert_eq!(env.att_count().await, 1);
}

#[tokio::test]
async fn concurrent_inserts_cannot_exceed_the_storage_limit() {
    let env = AttEnv::with(AttOptions {
        file_db: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let news: Vec<_> = (0..4)
        .map(|_| env.new_attachment(chat, AttachmentKind::Document, (MIB * 6 / 10) as i64))
        .collect();

    let limit = limits(50, 1);
    let results =
        futures::future::join_all(news.iter().map(|new| env.sea.insert_pending(new, &limit))).await;

    let accepted = results.iter().filter(|r| r.is_ok()).count();
    let rejected = results
        .iter()
        .filter(|r| matches!(r, Err(DomainError::StorageLimit)))
        .count();
    assert_eq!((accepted, rejected), (1, 3), "{results:?}");
}
