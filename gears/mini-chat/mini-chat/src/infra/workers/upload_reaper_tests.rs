#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Upload reaper tests on in-memory `SQLite` with a running outbox (DESIGN
//! B.9.5): selection and skip rules, the abandon CAS, the cleanup message.

use std::time::Duration;

use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::ports::{AttachmentKind, AttachmentStatus, UPLOAD_ABANDONED};
use crate::infra::db::entities::attachments;
use crate::infra::db::repo::tests::{
    AttachmentSeed, TENANT_A, TENANT_B, TestStore, USER_A1, at, attachment_row, seed_attachment,
    seed_chat,
};
use crate::infra::outbox::OutboxQueues;
use crate::infra::workers::NoopElector;
use crate::infra::workers::tests::SwitchElector;

/// Ten minutes: older than `stale_after_secs = 300`.
const STALE_MS: i64 = -600_000;

fn reaper(store: &TestStore, elector: Arc<dyn LeaderElector>, enabled: bool) -> UploadReaper {
    UploadReaper::new(
        Arc::clone(&store.repo),
        elector,
        &UploadReaperConfig {
            enabled,
            scan_interval_secs: 1,
            stale_after_secs: 300,
        },
        "mini_chat_test",
    )
}

async fn set_columns(store: &TestStore, id: Uuid, columns: Vec<(attachments::Column, SimpleExpr)>) {
    let mut update = attachments::Entity::update_many();
    for (column, value) in columns {
        update = update.col_expr(column, value);
    }
    update
        .filter(attachments::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&store.conn())
        .await
        .unwrap();
}

/// An attachment of `chat_id` in `status`, last updated `updated_ms` ago,
/// with or without a provider file.
async fn upload(
    store: &TestStore,
    chat_id: Uuid,
    tenant: Uuid,
    status: AttachmentStatus,
    updated_ms: i64,
    with_file: bool,
) -> Uuid {
    let id = seed_attachment(
        store,
        chat_id,
        tenant,
        AttachmentSeed::new(AttachmentKind::Document, status, "doc.txt"),
    )
    .await;
    let mut columns = vec![(
        attachments::Column::UpdatedAt,
        Expr::value(store.stamp(at(updated_ms))),
    )];
    if !with_file {
        columns.push((
            attachments::Column::ProviderFileId,
            Expr::value(Option::<String>::None),
        ));
    }
    set_columns(store, id, columns).await;
    id
}

#[tokio::test]
async fn stale_uploaded_row_is_abandoned_with_a_cleanup_message() {
    let store = TestStore::new().await;
    let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let id = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Uploaded,
        STALE_MS,
        true,
    )
    .await;
    let before = attachment_row(&store, id).await;

    let report = reaper(&store, Arc::new(NoopElector), true)
        .scan(&CancellationToken::new())
        .await;

    assert_eq!(
        (report.selected, report.abandoned, report.cleanup_enqueued),
        (1, 1, 1)
    );
    let row = attachment_row(&store, id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some(UPLOAD_ABANDONED));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.cleanup_updated_at.is_some());
    assert!(
        row.updated_at.at() > before.updated_at.at(),
        "updated_at refreshed"
    );
    assert_eq!(row.deleted_at, None, "a reaped row is not soft-deleted");

    let queue = OutboxQueues::default().attachment_cleanup;
    let delivered = store.wait_delivered(&queue, 1).await;
    assert_eq!(delivered.len(), 1);
    let payload = &delivered[0].payload;
    assert_eq!(payload["event_type"], "attachment_upload_abandoned");
    assert_eq!(payload["tenant_id"], serde_json::json!(TENANT_A));
    assert_eq!(payload["chat_id"], serde_json::json!(chat_id));
    assert_eq!(payload["attachment_id"], serde_json::json!(id));
    assert_eq!(
        payload["provider_file_id"],
        serde_json::json!(before.provider_file_id)
    );
    assert_eq!(payload["storage_backend"], "openai");
    assert_eq!(payload["attachment_kind"], "document");
    assert_eq!(payload["secondary_ref"], serde_json::Value::Null);
}

#[tokio::test]
async fn stale_pending_row_without_file_is_abandoned_without_cleanup() {
    let store = TestStore::new().await;
    let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let id = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Pending,
        STALE_MS,
        false,
    )
    .await;

    let report = reaper(&store, Arc::new(NoopElector), true)
        .scan(&CancellationToken::new())
        .await;

    assert_eq!((report.abandoned, report.cleanup_enqueued), (1, 0));
    let row = attachment_row(&store, id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some(UPLOAD_ABANDONED));
    assert_eq!(row.cleanup_status, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let queue = OutboxQueues::default().attachment_cleanup;
    assert!(store.delivered(&queue).is_empty());
}

#[tokio::test]
async fn fresh_deleted_claimed_and_ready_rows_are_skipped() {
    let store = TestStore::new().await;
    let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let fresh = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Pending,
        -1_000,
        false,
    )
    .await;
    let deleted = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Uploaded,
        STALE_MS,
        true,
    )
    .await;
    set_columns(
        &store,
        deleted,
        vec![(
            attachments::Column::DeletedAt,
            Expr::value(Some(store.stamp(at(STALE_MS)))),
        )],
    )
    .await;
    let claimed = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Uploaded,
        STALE_MS,
        true,
    )
    .await;
    set_columns(
        &store,
        claimed,
        vec![(
            attachments::Column::CleanupStatus,
            Expr::value(Some("done".to_owned())),
        )],
    )
    .await;
    let ready = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Ready,
        STALE_MS,
        true,
    )
    .await;
    let before: Vec<_> = futures::future::join_all(
        [fresh, deleted, claimed, ready].map(|id| attachment_row(&store, id)),
    )
    .await;

    let report = reaper(&store, Arc::new(NoopElector), true)
        .scan(&CancellationToken::new())
        .await;

    assert_eq!(report.selected, 0);
    for old in before {
        assert_eq!(attachment_row(&store, old.id).await, old);
    }
}

#[tokio::test]
async fn abandon_cas_rechecks_the_scanned_status_and_staleness() {
    let store = TestStore::new().await;
    let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let id = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Pending,
        STALE_MS,
        false,
    )
    .await;
    let worker = reaper(&store, Arc::new(NoopElector), true);
    let cutoff = worker.cutoff(storage_now());
    let scanned = attachment_repo::stale_uploads(&store.conn(), store.ts(), cutoff, SCAN_LIMIT)
        .await
        .unwrap();
    assert_eq!(scanned.len(), 1);

    // The upload finished meanwhile: pending -> uploaded with a fresh updated_at.
    set_columns(
        &store,
        id,
        vec![(
            attachments::Column::Status,
            Expr::value("uploaded".to_owned()),
        )],
    )
    .await;
    assert_eq!(worker.abandon(scanned[0], cutoff).await.unwrap(), None);
    set_columns(
        &store,
        id,
        vec![
            (
                attachments::Column::Status,
                Expr::value("pending".to_owned()),
            ),
            (
                attachments::Column::UpdatedAt,
                Expr::value(store.stamp(at(0))),
            ),
        ],
    )
    .await;
    assert_eq!(worker.abandon(scanned[0], cutoff).await.unwrap(), None);
    assert_eq!(attachment_row(&store, id).await.status, "pending");
}

#[tokio::test]
async fn scan_crosses_tenants_oldest_first_and_takes_at_most_100() {
    let store = TestStore::new().await;
    let chat_a = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let chat_b = seed_chat(&store, TENANT_B, USER_A1, None).await;
    let mut seeded = Vec::new();
    for index in 0..101_i64 {
        let (chat, tenant) = if index % 2 == 0 {
            (chat_a, TENANT_A)
        } else {
            (chat_b, TENANT_B)
        };
        let updated = STALE_MS - 1_000 * (101 - index);
        seeded.push(
            upload(
                &store,
                chat,
                tenant,
                AttachmentStatus::Pending,
                updated,
                false,
            )
            .await,
        );
    }
    let worker = reaper(&store, Arc::new(NoopElector), true);

    let scanned = worker.select(worker.cutoff(storage_now())).await.unwrap();

    let ids: Vec<Uuid> = scanned.iter().map(|row| row.id).collect();
    assert_eq!(ids, seeded[..100]);
    assert!(scanned.iter().any(|row| row.tenant_id == TENANT_B));
    assert!(
        scanned
            .iter()
            .all(|row| row.status == AttachmentStatus::Pending)
    );
}

#[tokio::test]
async fn secondary_copy_is_not_referenced_by_the_cleanup_message() {
    let store = TestStore::new().await;
    let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let id = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Uploaded,
        STALE_MS,
        true,
    )
    .await;
    set_columns(
        &store,
        id,
        vec![(
            attachments::Column::SecondaryFileId,
            Expr::value(Some("anthropic-file-1".to_owned())),
        )],
    )
    .await;
    let worker = reaper(&store, Arc::new(NoopElector), true);
    let cutoff = worker.cutoff(storage_now());
    let scanned = worker.select(cutoff).await.unwrap();

    let abandoned = worker.abandon(scanned[0], cutoff).await.unwrap().unwrap();

    assert_eq!(
        abandoned.secondary_file_id.as_deref(),
        Some("anthropic-file-1")
    );
    assert!(abandoned.cleanup_enqueued);
    let queue = OutboxQueues::default().attachment_cleanup;
    let delivered = store.wait_delivered(&queue, 1).await;
    assert_eq!(
        delivered[0].payload["secondary_ref"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn disabled_or_follower_reaper_changes_nothing() {
    let store = TestStore::new().await;
    let chat_id = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let id = upload(
        &store,
        chat_id,
        TENANT_A,
        AttachmentStatus::Pending,
        STALE_MS,
        false,
    )
    .await;

    tokio::time::timeout(
        Duration::from_secs(1),
        reaper(&store, Arc::new(NoopElector), false).run(&CancellationToken::new()),
    )
    .await
    .expect("a disabled reaper does not loop");
    let follower = reaper(&store, SwitchElector::new(false), true);
    let report = follower.scan(&CancellationToken::new()).await;

    assert!(report.interrupted);
    assert_eq!(attachment_row(&store, id).await.status, "pending");
}
