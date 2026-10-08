#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::domain::outbox_payloads::AttachmentCleanupEvent;
use crate::domain::service::attachments::test_helpers::*;
use crate::domain::service::test_support::{TENANT_A, TestEnv, TestOptions, USER_A1};

fn ago(secs: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - time::Duration::seconds(secs)
}

#[tokio::test]
async fn reaps_stale_pending_and_uploaded_rows() {
    let env = TestEnv::default_env().await;
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    // Stale pending without a provider file.
    let pending = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("pending".into());
        a.provider_file_id = Set(None);
        a.updated_at = Set(ago(600));
    })
    .await;
    // Stale uploaded with a provider file.
    let uploaded = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("uploaded".into());
        a.updated_at = Set(ago(400));
    })
    .await;
    // Not reaped: fresh, ready, deleted, owned by chat cleanup.
    let fresh = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("uploaded".into());
        a.updated_at = Set(ago(10));
    })
    .await;
    let ready = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| a.updated_at = Set(ago(900))).await;
    let deleted = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("pending".into());
        a.updated_at = Set(ago(900));
        a.deleted_at = Set(Some(ago(800)));
    })
    .await;
    let owned = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("uploaded".into());
        a.updated_at = Set(ago(900));
        a.cleanup_status = Set(Some("pending".into()));
    })
    .await;

    assert_eq!(scan_once(&env.deps).await.unwrap(), 2);

    let p = row(&env, TENANT_A, pending).await;
    assert_eq!(p.status, "failed");
    assert_eq!(p.error_code.as_deref(), Some("upload_abandoned"));
    assert!(p.cleanup_status.is_none(), "no provider file → no cleanup");
    assert!(p.updated_at > ago(5));
    assert!(p.deleted_at.is_none());

    let u = row(&env, TENANT_A, uploaded).await;
    assert_eq!(u.status, "failed");
    assert_eq!(u.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(u.cleanup_status.as_deref(), Some("pending"));
    assert!(u.cleanup_updated_at.is_some_and(|t| t > ago(5)), "cleanup_updated_at set with pending");
    assert!(p.cleanup_updated_at.is_none());

    assert_eq!(row(&env, TENANT_A, fresh).await.status, "uploaded");
    assert_eq!(row(&env, TENANT_A, ready).await.status, "ready");
    assert_eq!(row(&env, TENANT_A, deleted).await.status, "pending");
    assert_eq!(row(&env, TENANT_A, owned).await.status, "uploaded");

    let q = env.deps.cfg.outbox.cleanup_queue_name.clone();
    let events = env.delivered_to(&q, 1).await;
    assert_eq!(events.len(), 1);
    let ev: AttachmentCleanupEvent = serde_json::from_value(events[0].clone()).unwrap();
    assert_eq!(ev.event_type, "attachment_upload_abandoned");
    assert_eq!(ev.attachment_id, uploaded);
    assert_eq!(ev.provider_file_id, u.provider_file_id);
    assert!(ev.secondary_ref.is_none());

    // A second scan finds nothing.
    assert_eq!(scan_once(&env.deps).await.unwrap(), 0);
    env.shutdown().await;
}

#[tokio::test]
async fn scan_is_limited_to_100_oldest_rows() {
    let env = TestEnv::default_env().await;
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let mut ids = Vec::new();
    for i in 0..105 {
        let id = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
            a.status = Set("pending".into());
            a.provider_file_id = Set(None);
            a.updated_at = Set(ago(10_000 - i));
        })
        .await;
        ids.push(id);
    }
    assert_eq!(scan_once(&env.deps).await.unwrap(), 100);
    // The 5 newest are left for the next scan.
    for id in &ids[100..] {
        assert_eq!(row(&env, TENANT_A, *id).await.status, "pending");
    }
    assert_eq!(row(&env, TENANT_A, ids[0]).await.status, "failed");
    assert_eq!(scan_once(&env.deps).await.unwrap(), 5);
    env.shutdown().await;
}

#[tokio::test]
async fn run_loop_scans_and_stops_on_cancel() {
    let mut opts = TestOptions::default();
    opts.cfg.upload_reaper.scan_interval_secs = 1;
    let env = TestEnv::new(opts).await;
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let id = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("pending".into());
        a.provider_file_id = Set(None);
        a.updated_at = Set(ago(1000));
    })
    .await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(std::sync::Arc::clone(&env.deps), cancel.clone()));
    let r = wait_row(&env, TENANT_A, id, |r| r.status == "failed").await;
    assert_eq!(r.status, "failed");
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("reaper stops")
        .unwrap();
    env.shutdown().await;
}

#[tokio::test]
async fn disabled_reaper_does_nothing() {
    let mut opts = TestOptions::default();
    opts.cfg.upload_reaper.enabled = false;
    opts.cfg.upload_reaper.scan_interval_secs = 1;
    let env = TestEnv::new(opts).await;
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let id = insert_attachment(&env, TENANT_A, chat, USER_A1, |a| {
        a.status = Set("pending".into());
        a.updated_at = Set(ago(1000));
    })
    .await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(std::sync::Arc::clone(&env.deps), cancel.clone()));
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(row(&env, TENANT_A, id).await.status, "pending");
    cancel.cancel();
    task.await.unwrap();
    env.shutdown().await;
}
