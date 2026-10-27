#![allow(clippy::wildcard_imports)]
//! Cleanup & recovery tests: chat deletion cleanup, attachment cleanup,
//! orphan watchdog, upload reaper.

use time::OffsetDateTime;
use uuid::Uuid;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::SecureUpdateExt;

use super::*;
use crate::infra::storage::repo::attachments::NewAttachment;
use crate::infra::watchdog::{finalize_orphan, reap_row};

#[tokio::test]
async fn chat_deletion_cleans_provider_resources() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Upload a document (creates file + vector store).
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "text/plain")
        .await
        .expect("prepare");
    bed.services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            crate::domain::attachment_service::UploadInput {
                filename: "doc.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                bytes: bytes::Bytes::from_static(b"doc"),
            },
        )
        .await
        .expect("upload");
    let n_files = bed.gateway.files.lock().expect("lock").len();
    let n_stores = bed.gateway.vector_stores.lock().expect("lock").len();
    assert_eq!((n_files, n_stores), (1, 1));

    // Delete the chat.
    bed.services
        .chats
        .delete_chat(&bed.bed.security, chat.id)
        .await
        .expect("delete");

    // The cleanup handler (running via the real outbox pipeline) deletes
    // the provider file and the vector store.
    wait_until(|| !bed.gateway.deleted_files.lock().expect("lock").is_empty(), "file delete").await;
    wait_until(
        || !bed.gateway.deleted_vector_stores.lock().expect("lock").is_empty(),
        "vector store delete",
    )
    .await;

    // The vector store row is gone; attachments cleanup done.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let store = bed
        .bed
        .ctx
        .repos
        .vector_stores
        .get(&conn, bed.bed.security.subject_tenant_id(), chat.id)
        .await
        .expect("get");
    assert!(store.is_none(), "vector store row removed after cleanup");
}

#[tokio::test]
async fn attachment_deletion_cleans_provider_file() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "text/plain")
        .await
        .expect("prepare");
    let att = bed
        .services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            crate::domain::attachment_service::UploadInput {
                filename: "doc.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                bytes: bytes::Bytes::from_static(b"doc"),
            },
        )
        .await
        .expect("upload");
    let file_id = att.provider_file_id.clone().expect("file id");

    bed.services
        .attachments
        .delete(&bed.bed.security, chat.id, att.id)
        .await
        .expect("delete");
    wait_until(
        || bed.gateway.deleted_files.lock().expect("lock").contains(&file_id),
        "provider file delete",
    )
    .await;
    // The chat's vector store is NOT deleted with a single attachment.
    assert!(bed.gateway.deleted_vector_stores.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn orphan_watchdog_finalizes_stale_turn() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Insert a stale running turn with a reserve.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .insert(
            &conn,
            &toolkit_db::secure::AccessScope::allow_all(),
            crate::infra::storage::repo::turns::NewTurn {
                id: Uuid::new_v4(),
                tenant_id: bed.bed.security.subject_tenant_id(),
                chat_id: chat.id,
                request_id: Uuid::new_v4(),
                requester_user_id: bed.bed.security.subject_id(),
                web_search_enabled: false,
                reserve_tokens: 1100,
                max_output_tokens_applied: 100,
                reserved_credits_micro: 1100,
                policy_version_applied: 1,
                effective_model: "gpt-premium".to_owned(),
                minimal_generation_floor_applied: 50,
            },
        )
        .await
        .expect("insert");
    // Seed the reserve rows (the reserve transaction creates them).
    let now = OffsetDateTime::now_utc();
    for (ptype, pstart) in [
        ("daily", crate::domain::services::periods::daily_start(now)),
        ("monthly", crate::domain::services::periods::monthly_start(now)),
    ] {
        for b in ["total", "tier:premium"] {
            bed.bed
                .ctx
                .repos
                .quota
                .add_reserve(
                    &conn,
                    bed.bed.security.subject_tenant_id(),
                    bed.bed.security.subject_id(),
                    crate::infra::storage::repo::quota::BucketKey {
                        period_type: ptype,
                        period_start: pstart,
                        bucket: b,
                    },
                    1100,
                )
                .await
                .expect("reserve");
        }
    }
    // Age it past the cutoff.
    let cutoff = OffsetDateTime::now_utc() + time::Duration::seconds(1);
    let won = finalize_orphan(&bed.bed.ctx, &turn, cutoff)
        .await
        .expect("finalize");
    assert!(won, "CAS won");
    // Turn is failed with orphan_timeout.
    let after = bed
        .bed
        .ctx
        .repos
        .turns
        .get_by_id(&conn, bed.bed.security.subject_tenant_id(), turn.id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(after.state, "failed");
    assert_eq!(after.error_code.as_deref(), Some("orphan_timeout"));
    // Exactly one usage event with aborted billing.
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 1, "usage event").await;
    let usage = bed.bed.policy.usage_events.lock().expect("lock");
    assert_eq!(usage[0].billing_outcome, "aborted");
    assert_eq!(usage[0].settlement_method, "estimated");
    // est: reserve 1100 - max_out 100 = 1000 est input; floor 50 -> 1050.
    assert_eq!(usage[0].actual_credits_micro, 1050);
    // Re-running is a no-op (already finalized).
    let won = finalize_orphan(&bed.bed.ctx, &turn, cutoff).await.expect("again");
    assert!(!won, "second finalization is a no-op");
}

#[tokio::test]
async fn upload_reaper_fails_abandoned_rows() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Insert a stale `uploaded` row with a provider file id.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let att = bed
        .bed
        .ctx
        .repos
        .attachments
        .insert(
            &conn,
            &toolkit_db::secure::AccessScope::allow_all(),
            NewAttachment {
                id: Uuid::new_v4(),
                tenant_id: bed.bed.security.subject_tenant_id(),
                chat_id: chat.id,
                uploaded_by_user_id: bed.bed.security.subject_id(),
                filename: "stuck.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                size_bytes: 10,
                storage_backend: "mock".to_owned(),
                attachment_kind: "document".to_owned(),
                for_file_search: true,
                for_code_interpreter: false,
            },
        )
        .await
        .expect("insert");
    bed.bed
        .ctx
        .repos
        .attachments
        .mark_uploaded(&conn, att.id, "file-stuck123456")
        .await
        .expect("uploaded");
    let att = bed
        .bed
        .ctx
        .repos
        .attachments
        .get_any_raw(&conn, att.id)
        .await
        .expect("reload");
    // Force the row stale.
    crate::infra::storage::entity::attachments::Entity::update_many()
        .col_expr(
            crate::infra::storage::entity::attachments::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(OffsetDateTime::now_utc() - time::Duration::hours(1)),
        )
        .filter(
            sea_orm::Condition::all()
                .add(crate::infra::storage::entity::attachments::Column::Id.eq(att.id)),
        )
        .secure()
        .scope_with(&toolkit_db::secure::AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("age");

    let cutoff = OffsetDateTime::now_utc() - time::Duration::seconds(300);
    let won = reap_row(&bed.bed.ctx, &att, cutoff).await.expect("reap");
    assert!(won);
    let after = bed
        .bed
        .ctx
        .repos
        .attachments
        .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, att.id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(after.status, "failed");
    assert_eq!(after.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(after.cleanup_status.as_deref(), Some("pending"));
    // The provider file delete flows through the outbox pipeline.
    wait_until(
        || {
            bed.gateway
                .deleted_files
                .lock()
                .expect("lock")
                .contains(&"file-stuck123456".to_owned())
        },
        "provider file delete",
    )
    .await;
}
