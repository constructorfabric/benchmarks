#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The upload reaper (DESIGN "Upload Attachment" abandoned uploads, B.9.5): attachments left
//! `pending` or `uploaded` by a dropped request or a crashed process are failed with
//! `upload_abandoned` after `upload_reaper.stale_after_secs`, and the provider file recorded on
//! the row is handed to the attachment cleanup outbox queue.
//!
//! Each scan is driven with `TestEnv::run_upload_reaper_once(now)` (application clock, HARNESS.md
//! contract); rows are inserted with SQL at chosen `updated_at` instants.

mod attachments;
mod common;

use attachments::{
    ATTACHMENT_CLEANUP_QUEUE, RowSeed, calls, chat_rows, deletes, expect_created, file_path,
    get_attachment, instant, row, script_document, soft_delete_chat_sql, upload_pdf,
    wait_queue_handled, wait_row,
};
use common::{assert_quota_exhausted, expect_status, new_chat};
use http::StatusCode;
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// The application clock of the scans: ten minutes after the default `updated_at` of seeded rows.
const NOW: &str = "2026-01-01T00:10:00Z";

fn now() -> OffsetDateTime {
    instant(NOW)
}

/// `NOW - secs` as an RFC 3339 literal.
fn ago(secs: i64) -> String {
    (now() - Duration::seconds(secs)).format(&Rfc3339).unwrap()
}

async fn setup() -> (TestEnv, TestUser, String) {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

// ===========================================================================
// What the reaper takes
// ===========================================================================

#[tokio::test]
async fn a_stale_pending_row_without_a_provider_file_is_failed_as_abandoned() {
    let (env, user, chat) = setup().await;
    let id = RowSeed::new(&user, &chat)
        .status("pending")
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    let reaped = row(&env, id).await;
    assert_eq!(reaped.status, "failed");
    assert_eq!(reaped.error_code.as_deref(), Some("upload_abandoned"));
    assert!(reaped.deleted_at.is_none(), "the row is not soft-deleted");
    assert!(
        reaped.cleanup_status.is_none(),
        "there is no provider file to clean up"
    );
    assert_eq!(
        instant(&reaped.updated_at),
        now(),
        "updated_at is the application clock of the scan"
    );
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn a_reaped_row_stays_visible_via_get_as_failed() {
    let (env, user, chat) = setup().await;
    let id = RowSeed::new(&user, &chat)
        .status("pending")
        .insert(&env)
        .await;
    env.run_upload_reaper_once(now()).await;

    let resp = get_attachment(&env, &user, &chat, &id.to_string()).await;

    expect_status(&resp, StatusCode::OK);
    let detail = resp.json();
    assert_eq!(detail["status"], json!("failed"));
    assert_eq!(detail["error_code"], json!("upload_abandoned"));
    assert_eq!(detail["id"], json!(id.to_string()));
}

#[tokio::test]
async fn a_stale_uploaded_row_with_a_provider_file_schedules_the_file_cleanup() {
    let (env, user, chat) = setup().await;
    let id = RowSeed::new(&user, &chat)
        .status("uploaded")
        .provider_file("file-stale")
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    let reaped = row(&env, id).await;
    assert_eq!(reaped.status, "failed");
    assert_eq!(reaped.error_code.as_deref(), Some("upload_abandoned"));
    assert!(reaped.deleted_at.is_none());
    assert!(
        matches!(reaped.cleanup_status.as_deref(), Some("pending" | "done")),
        "scheduled in the same transaction (the handler may already have finished): {reaped:?}"
    );
    let payloads = env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let payload = &payloads[0];
    assert_eq!(payload["event_type"], json!("attachment_upload_abandoned"));
    assert_eq!(payload["tenant_id"], json!(user.tenant_id));
    assert_eq!(payload["chat_id"], json!(chat));
    assert_eq!(payload["attachment_id"], json!(id.to_string()));
    assert_eq!(payload["provider_file_id"], json!("file-stale"));
    assert!(payload["vector_store_id"].is_null());
    assert_eq!(payload["storage_backend"], json!("openai"));
    assert_eq!(payload["attachment_kind"], json!("document"));
    assert!(payload["secondary_ref"].is_null());
}

#[tokio::test]
async fn the_outbox_cleanup_deletes_the_provider_file_of_a_reaped_row() {
    let (env, user, chat) = setup().await;
    let id = RowSeed::new(&user, &chat)
        .status("uploaded")
        .provider_file("file-stale")
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    wait_queue_handled(&env, ATTACHMENT_CLEANUP_QUEUE).await;
    let done = wait_row(&env, id, "the finished cleanup", |row| {
        row.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(done.status, "failed");
    assert_eq!(done.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(deletes(&env), vec![file_path("file-stale")]);
}

#[tokio::test]
async fn the_cleanup_event_of_an_image_carries_no_secondary_reference() {
    let (env, user, chat) = setup().await;
    RowSeed::new(&user, &chat)
        .image()
        .status("uploaded")
        .provider_file("file-stale")
        .secondary("file_011sec")
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    let payloads = env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["attachment_kind"], json!("image"));
    assert!(
        payloads[0]["secondary_ref"].is_null(),
        "an Anthropic secondary copy is not deleted by the reaper: {}",
        payloads[0]
    );
}

#[tokio::test]
async fn rows_of_every_tenant_and_chat_are_reaped() {
    let env = TestEnv::new().await;
    let first = TestUser::new();
    let second = TestUser::new();
    let first_chat = new_chat(&env, &first).await;
    let second_chat = new_chat(&env, &second).await;
    let a = RowSeed::new(&first, &first_chat)
        .status("pending")
        .insert(&env)
        .await;
    let b = RowSeed::new(&second, &second_chat)
        .status("uploaded")
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    assert_eq!(
        row(&env, a).await.error_code.as_deref(),
        Some("upload_abandoned")
    );
    assert_eq!(
        row(&env, b).await.error_code.as_deref(),
        Some("upload_abandoned")
    );
}

#[tokio::test]
async fn a_second_scan_changes_nothing_and_enqueues_nothing_again() {
    let (env, user, chat) = setup().await;
    let id = RowSeed::new(&user, &chat)
        .status("uploaded")
        .provider_file("file-stale")
        .insert(&env)
        .await;
    env.run_upload_reaper_once(now()).await;
    wait_queue_handled(&env, ATTACHMENT_CLEANUP_QUEUE).await;
    let after_first = row(&env, id).await;

    env.run_upload_reaper_once(now() + Duration::seconds(600))
        .await;

    let after_second = row(&env, id).await;
    assert_eq!(after_second.updated_at, after_first.updated_at);
    assert_eq!(after_second.status, "failed");
    assert_eq!(env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await.len(), 1);
    assert_eq!(deletes(&env).len(), 1);
}

// ===========================================================================
// What the reaper leaves alone
// ===========================================================================

#[tokio::test]
async fn recent_rows_are_untouched_and_the_cutoff_is_exclusive() {
    let (env, user, chat) = setup().await;
    // cutoff = NOW - 300 s
    let fresh = RowSeed::new(&user, &chat)
        .status("pending")
        .updated_at(&ago(60))
        .insert(&env)
        .await;
    let at_cutoff = RowSeed::new(&user, &chat)
        .status("pending")
        .updated_at(&ago(300))
        .insert(&env)
        .await;
    let just_stale = RowSeed::new(&user, &chat)
        .status("pending")
        .updated_at(&ago(301))
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    assert_eq!(row(&env, fresh).await.status, "pending");
    assert_eq!(
        row(&env, at_cutoff).await.status,
        "pending",
        "updated_at < cutoff is strict"
    );
    assert_eq!(row(&env, just_stale).await.status, "failed");
}

#[tokio::test]
async fn only_pending_and_uploaded_rows_are_candidates() {
    let (env, user, chat) = setup().await;
    let ready = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-1")
        .insert(&env)
        .await;
    let failed = RowSeed::new(&user, &chat)
        .status("failed")
        .error_code("indexing_failed")
        .provider_file("file-2")
        .insert(&env)
        .await;
    let deleted_pending = RowSeed::new(&user, &chat)
        .status("pending")
        .deleted()
        .insert(&env)
        .await;
    let deleted_uploaded = RowSeed::new(&user, &chat)
        .status("uploaded")
        .provider_file("file-3")
        .deleted()
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    for (id, status) in [
        (ready, "ready"),
        (failed, "failed"),
        (deleted_pending, "pending"),
        (deleted_uploaded, "uploaded"),
    ] {
        let untouched = row(&env, id).await;
        assert_eq!(untouched.status, status);
        assert_ne!(untouched.error_code.as_deref(), Some("upload_abandoned"));
    }
    assert_eq!(
        row(&env, failed).await.error_code.as_deref(),
        Some("indexing_failed")
    );
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn rows_that_chat_cleanup_owns_are_skipped() {
    let (env, user, chat) = setup().await;
    let deleted_chat = new_chat(&env, &user).await;
    let pending_cleanup = RowSeed::new(&user, &deleted_chat)
        .status("uploaded")
        .provider_file("file-1")
        .cleanup("pending")
        .insert(&env)
        .await;
    let done_cleanup = RowSeed::new(&user, &deleted_chat)
        .status("pending")
        .cleanup("done")
        .insert(&env)
        .await;
    let failed_cleanup = RowSeed::new(&user, &chat)
        .status("uploaded")
        .provider_file("file-2")
        .cleanup("failed")
        .insert(&env)
        .await;
    soft_delete_chat_sql(&env, &deleted_chat).await;

    env.run_upload_reaper_once(now()).await;

    assert_eq!(row(&env, pending_cleanup).await.status, "uploaded");
    assert_eq!(row(&env, done_cleanup).await.status, "pending");
    assert_eq!(
        row(&env, failed_cleanup).await.status,
        "uploaded",
        "any cleanup_status is skipped"
    );
    for id in [pending_cleanup, done_cleanup, failed_cleanup] {
        assert!(row(&env, id).await.error_code.is_none());
    }
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
    assert!(calls(&env).is_empty());
}

// ===========================================================================
// Configuration and batching
// ===========================================================================

#[tokio::test]
async fn the_stale_threshold_comes_from_the_configuration() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "upload_reaper": { "stale_after_secs": 60 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let stale = RowSeed::new(&user, &chat)
        .status("pending")
        .updated_at(&ago(90))
        .insert(&env)
        .await;
    let fresh = RowSeed::new(&user, &chat)
        .status("pending")
        .updated_at(&ago(30))
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    assert_eq!(row(&env, stale).await.status, "failed");
    assert_eq!(row(&env, fresh).await.status, "pending");
}

#[tokio::test]
async fn one_scan_takes_at_most_100_rows_oldest_first() {
    let (env, user, chat) = setup().await;
    let mut ids: Vec<Uuid> = Vec::new();
    for index in 0..101 {
        // 500 s .. 400 s before NOW: all stale, increasingly recent.
        let id = RowSeed::new(&user, &chat)
            .status("pending")
            .updated_at(&ago(500 - index))
            .insert(&env)
            .await;
        ids.push(id);
    }

    env.run_upload_reaper_once(now()).await;

    let rows = chat_rows(&env, &chat).await;
    let failed = rows.iter().filter(|row| row.status == "failed").count();
    assert_eq!(failed, 100);
    assert_eq!(
        row(&env, *ids.last().unwrap()).await.status,
        "pending",
        "the most recently updated row waits for the next scan"
    );

    env.run_upload_reaper_once(now()).await;

    assert_eq!(row(&env, *ids.last().unwrap()).await.status, "failed");
}

#[tokio::test]
async fn a_reaped_upload_no_longer_counts_toward_the_document_limit() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "rag": { "max_documents_per_chat": 1 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    RowSeed::new(&user, &chat)
        .status("pending")
        .insert(&env)
        .await;
    assert_quota_exhausted(&upload_pdf(&env, &user, &chat).await, "document_limit");

    env.run_upload_reaper_once(now()).await;

    script_document(&env, "file-new", "vs_chat1", true);
    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);
    assert_eq!(detail["status"], json!("ready"));
}

#[tokio::test]
async fn a_row_that_is_being_indexed_by_a_live_upload_is_not_reaped() {
    // `updated_at` is refreshed by the upload / the background task; a row updated "now" is not stale.
    let (env, user, chat) = setup().await;
    let live = RowSeed::new(&user, &chat)
        .status("uploaded")
        .provider_file("file-live")
        .updated_at(&ago(0))
        .insert(&env)
        .await;

    env.run_upload_reaper_once(now()).await;

    assert_eq!(row(&env, live).await.status, "uploaded");
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
}
