//! Upload reaper tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{run, scan_once};
use crate::clock;
use crate::domain::attachments::test_support::{file_deletes, insert_row, row, row_template, stop_outbox};
use crate::testing::{TENANT_A, TestApp, USER_A1, ctx_a1};

fn minutes_ago(m: i64) -> time::OffsetDateTime {
    clock::normalize(clock::now() - time::Duration::minutes(m))
}

async fn insert(t: &TestApp, chat: Uuid, status: &str, file: Option<&str>, updated_at: time::OffsetDateTime) -> Uuid {
    let mut m = row_template(TENANT_A, chat, USER_A1, status, updated_at);
    m.provider_file_id = file.map(str::to_owned);
    insert_row(t, m).await
}

#[tokio::test]
async fn stale_rows_are_marked_abandoned() {
    let mut t = TestApp::new().await;
    stop_outbox(&mut t).await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let pending = insert(&t, chat, "pending", None, minutes_ago(10)).await;
    let uploaded = insert(&t, chat, "uploaded", Some("file-1"), minutes_ago(10)).await;
    let fresh = insert(&t, chat, "pending", None, minutes_ago(1)).await;
    let ready = insert(&t, chat, "ready", Some("file-2"), minutes_ago(60)).await;
    let mut claimed = row_template(TENANT_A, chat, USER_A1, "uploaded", minutes_ago(10));
    claimed.cleanup_status = Some("pending".to_owned());
    let claimed = insert_row(&t, claimed).await;
    let mut deleted = row_template(TENANT_A, chat, USER_A1, "pending", minutes_ago(10));
    deleted.deleted_at = Some(minutes_ago(9));
    let deleted = insert_row(&t, deleted).await;

    assert_eq!(scan_once(&t.app, clock::now()).await.unwrap(), 2);

    let r = row(&t, pending).await;
    assert_eq!((r.status.as_str(), r.error_code.as_deref(), r.cleanup_status.as_deref()), ("failed", Some("upload_abandoned"), None));
    assert!(r.deleted_at.is_none());
    let r = row(&t, uploaded).await;
    assert_eq!((r.status.as_str(), r.error_code.as_deref()), ("failed", Some("upload_abandoned")));
    assert_eq!(r.cleanup_status.as_deref(), Some("pending"));
    for (id, status) in [(fresh, "pending"), (ready, "ready"), (claimed, "uploaded"), (deleted, "pending")] {
        let r = row(&t, id).await;
        assert_eq!(r.status, status);
        assert!(r.error_code.is_none());
    }
    // Nothing left to reap.
    assert_eq!(scan_once(&t.app, clock::now()).await.unwrap(), 0);
}

#[tokio::test]
async fn abandoned_upload_file_is_deleted_by_the_cleanup() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let id = insert(&t, chat, "uploaded", Some("file-abandoned"), minutes_ago(10)).await;
    assert_eq!(scan_once(&t.app, clock::now()).await.unwrap(), 1);
    t.eventually("abandoned file cleanup", || async { row(&t, id).await.cleanup_status.as_deref() == Some("done") }).await;
    assert_eq!(file_deletes(&t), 1);
    assert!(t.provider.recorded().iter().any(|r| r.method == "DELETE" && r.uri.ends_with("/files/file-abandoned")));
}

#[tokio::test]
async fn threshold_uses_stale_after_secs() {
    let mut t = TestApp::with_config(|c| c.upload_reaper.stale_after_secs = 3600).await;
    stop_outbox(&mut t).await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let id = insert(&t, chat, "pending", None, minutes_ago(30)).await;
    assert_eq!(scan_once(&t.app, clock::now()).await.unwrap(), 0);
    assert_eq!(scan_once(&t.app, clock::now() + time::Duration::minutes(31)).await.unwrap(), 1);
    assert_eq!(row(&t, id).await.status, "failed");
}

#[tokio::test]
async fn worker_loop_scans_until_cancelled() {
    let mut t = TestApp::with_config(|c| c.upload_reaper.scan_interval_secs = 1).await;
    stop_outbox(&mut t).await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let id = insert(&t, chat, "pending", None, minutes_ago(10)).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(Arc::clone(&t.app), cancel.clone()));
    t.eventually("reaped by the worker", || async { row(&t, id).await.status == "failed" }).await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
}
