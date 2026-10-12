#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Document indexing of an upload (DESIGN "File Upload" sequence, ADR-0007, B.9.5): the vector
//! store file status polling inside the request, the request deadline, the background indexing
//! task that finishes an upload still `in_progress` at the deadline, and the failure paths.
//!
//! The deadline (25 s), the polling waits (250 ms doubling to 2 s / 5 s), the background round
//! (20 s) and the background limit (10 min) are fixed in production; the tests shorten them with
//! the S5 harness hook `TestEnvBuilder::indexing_timings` (HARNESS.md "S5 additions").

mod attachments;
mod common;

use std::time::Duration;

use attachments::{
    ATTACHMENT_CLEANUP_QUEUE, DOCUMENT_DETAIL_KEYS, assert_keys, assert_no_provider_ids,
    assert_unavailable, calls_to, chat_rows, delete_ok, detail_id, expect_created, file_path,
    get_attachment, ok, provider_error, row, script_file, script_store_add, script_store_create,
    script_store_status, script_store_status_error, settle, store_rows, upload_pdf,
    vector_store_file, vector_store_file_path, vector_store_path, wait_calls, wait_row,
    wait_status,
};
use common::{
    assert_invalid_argument, assert_nothing_happened, chat_request_body, delete_chat,
    expect_status, expect_stream, new_chat, send, send_text,
};
use http::StatusCode;
use mini_chat::test_support::{IndexingTimings, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};

const FILE: &str = "file-doc1";
const STORE: &str = "vs_chat1";

/// Short timings: deadline 400 ms, polls 5 ms doubling to 20 ms, background rounds of 200 ms and a
/// background limit of 1.5 s.
fn timings() -> IndexingTimings {
    IndexingTimings {
        request_deadline: Duration::from_millis(400),
        poll_initial: Duration::from_millis(5),
        poll_max: Duration::from_millis(20),
        background_round: Duration::from_millis(200),
        background_poll_max: Duration::from_millis(20),
        background_timeout: Duration::from_millis(1500),
    }
}

async fn indexing_env() -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .indexing_timings(timings())
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

/// The file and the vector store of the chat; the add call reports `status`.
fn script_indexing_document(env: &TestEnv, add_status: Option<&str>) {
    script_file(env, FILE);
    script_store_create(env, STORE);
    script_store_add(env, STORE, FILE, add_status);
}

fn status_reads(env: &TestEnv) -> usize {
    calls_to(env, "GET", &vector_store_file_path(STORE, FILE)).len()
}

/// Uploads a document that stays `in_progress` past the request deadline and returns its detail
/// (`status: uploaded`).
async fn upload_in_progress(env: &TestEnv, user: &TestUser, chat: &str) -> Value {
    script_indexing_document(env, Some("in_progress"));
    let resp = upload_pdf(env, user, chat).await;
    let detail = expect_created(&resp);
    assert_eq!(detail["status"], json!("uploaded"), "{detail}");
    detail
}

// ===========================================================================
// Indexing inside the request
// ===========================================================================

#[tokio::test]
async fn an_in_progress_document_becomes_ready_when_the_vector_store_reports_completed() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, Some("in_progress"));
    script_store_status(&env, STORE, FILE, "in_progress");
    script_store_status(&env, STORE, FILE, "completed");

    let resp = upload_pdf(&env, &user, &chat).await;

    let detail = expect_created(&resp);
    assert_keys(&detail, &DOCUMENT_DETAIL_KEYS);
    assert_eq!(detail["status"], json!("ready"));
    assert_eq!(status_reads(&env), 2, "polled until completed");
    let stored = row(&env, detail_id(&detail)).await;
    assert_eq!(stored.status, "ready");
    assert!(stored.error_code.is_none());
}

#[tokio::test]
async fn a_status_response_without_a_status_counts_as_in_progress() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, None);
    env.provider().enqueue(
        &format!("/vector_stores/{STORE}/files/{FILE}"),
        ok(vector_store_file(STORE, FILE, None)),
    );
    script_store_status(&env, STORE, FILE, "completed");

    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);

    assert_eq!(detail["status"], json!("ready"));
    assert_eq!(
        status_reads(&env),
        2,
        "the status-less read kept the poll going"
    );
}

#[tokio::test]
async fn transient_status_read_errors_keep_polling() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, Some("in_progress"));
    script_store_status_error(&env, STORE, FILE, 500);
    script_store_status_error(&env, STORE, FILE, 503);
    script_store_status(&env, STORE, FILE, "completed");

    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);

    assert_eq!(detail["status"], json!("ready"));
    assert_eq!(status_reads(&env), 3);
}

#[tokio::test]
async fn a_failed_cancelled_or_unknown_status_is_503_and_the_provider_file_is_deleted() {
    for status in ["failed", "cancelled", "expired"] {
        let (env, user, chat) = indexing_env().await;
        script_indexing_document(&env, Some("in_progress"));
        script_store_status(&env, STORE, FILE, status);

        let resp = upload_pdf(&env, &user, &chat).await;

        let problem = assert_unavailable(&resp, 10);
        assert!(
            !problem.to_string().contains("indexing_failed"),
            "`{status}`: indexing_failed is not in the response body: {problem}"
        );
        let rows = chat_rows(&env, &chat).await;
        assert_eq!(rows.len(), 1, "{status}");
        assert_eq!(rows[0].status, "failed", "{status}");
        assert_eq!(
            rows[0].error_code.as_deref(),
            Some("indexing_failed"),
            "{status}"
        );
        wait_calls(&env, "DELETE", &file_path(FILE), 1).await;
        settle(150).await;
        assert_eq!(
            calls_to(&env, "DELETE", &file_path(FILE)).len(),
            1,
            "{status}: the inline delete is best effort and not retried"
        );
        assert!(
            env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
                .await
                .is_empty(),
            "{status}: no cleanup event for an inline failure"
        );
        assert!(rows[0].cleanup_status.is_none(), "{status}");
    }
}

#[tokio::test]
async fn a_best_effort_delete_that_fails_is_not_retried() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, Some("in_progress"));
    script_store_status(&env, STORE, FILE, "failed");
    env.provider()
        .enqueue(&format!("/v1/files/{FILE}"), provider_error(500));
    env.provider()
        .enqueue(&format!("/v1/files/{FILE}"), provider_error(500));

    assert_unavailable(&upload_pdf(&env, &user, &chat).await, 10);

    wait_calls(&env, "DELETE", &file_path(FILE), 1).await;
    settle(300).await;
    assert_eq!(calls_to(&env, "DELETE", &file_path(FILE)).len(), 1);
}

#[tokio::test]
async fn a_non_transient_status_read_error_before_the_deadline_is_503_indexing_failed() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, Some("in_progress"));
    script_store_status_error(&env, STORE, FILE, 404);

    let resp = upload_pdf(&env, &user, &chat).await;

    assert_unavailable(&resp, 10);
    let rows = chat_rows(&env, &chat).await;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("indexing_failed"));
    wait_calls(&env, "DELETE", &file_path(FILE), 1).await;
}

#[tokio::test]
async fn a_failed_indexing_upload_leaves_the_failed_row_visible_via_get() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, Some("in_progress"));
    script_store_status(&env, STORE, FILE, "failed");
    assert_unavailable(&upload_pdf(&env, &user, &chat).await, 10);
    let failed = chat_rows(&env, &chat).await.remove(0);

    let resp = get_attachment(&env, &user, &chat, &failed.id.to_string()).await;

    expect_status(&resp, StatusCode::OK);
    let detail = resp.json();
    assert_eq!(detail["status"], json!("failed"));
    assert_eq!(detail["error_code"], json!("indexing_failed"));
    assert_no_provider_ids(&detail.to_string());
}

// ===========================================================================
// The request deadline and the background task
// ===========================================================================

#[tokio::test]
async fn a_document_still_in_progress_at_the_deadline_is_201_uploaded() {
    let (env, user, chat) = indexing_env().await;
    script_indexing_document(&env, Some("in_progress"));
    let started = std::time::Instant::now();

    let resp = upload_pdf(&env, &user, &chat).await;

    let detail = expect_created(&resp);
    assert_keys(&detail, &DOCUMENT_DETAIL_KEYS);
    assert_eq!(detail["status"], json!("uploaded"));
    assert_eq!(detail["kind"], json!("document"));
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "the upload waited for the deadline: {:?}",
        started.elapsed()
    );
    assert!(
        status_reads(&env) >= 3,
        "the request polled until the deadline"
    );
    let stored = row(&env, detail_id(&detail)).await;
    assert_eq!(stored.status, "uploaded");
    assert_eq!(stored.provider_file_id.as_deref(), Some(FILE));
    assert!(stored.error_code.is_none() && stored.cleanup_status.is_none());
    assert_no_provider_ids(&detail.to_string());
    let polled = get_attachment(&env, &user, &chat, detail["id"].as_str().unwrap()).await;
    assert_eq!(polled.json()["status"], json!("uploaded"));
}

#[tokio::test]
async fn the_background_task_makes_the_attachment_ready() {
    let (env, user, chat) = indexing_env().await;
    let detail = upload_in_progress(&env, &user, &chat).await;
    let id = detail_id(&detail);

    script_store_status(&env, STORE, FILE, "completed");

    let ready = wait_status(&env, &user, &chat, id, "ready").await;
    assert_keys(&ready, &DOCUMENT_DETAIL_KEYS);
    assert_eq!(ready["id"], detail["id"]);
    assert_eq!(row(&env, id).await.status, "ready");
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn background_polling_survives_transient_read_errors() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    script_store_status_error(&env, STORE, FILE, 502);
    script_store_status_error(&env, STORE, FILE, 500);
    script_store_status(&env, STORE, FILE, "completed");

    wait_status(&env, &user, &chat, id, "ready").await;
}

#[tokio::test]
async fn the_background_task_refreshes_updated_at_every_round() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);
    let first = row(&env, id).await.updated_at;

    let refreshed = wait_row(&env, id, "a refreshed updated_at", |row| {
        row.updated_at != first
    })
    .await;

    assert_eq!(refreshed.status, "uploaded", "still indexing");
}

#[tokio::test]
async fn a_background_failure_fails_the_row_and_schedules_the_provider_file_cleanup() {
    for status in ["failed", "cancelled", "expired"] {
        let (env, user, chat) = indexing_env().await;
        let id = detail_id(&upload_in_progress(&env, &user, &chat).await);
        // The provider file delete keeps failing, so the row stays `pending` for the assertions.
        for _ in 0..5 {
            env.provider()
                .enqueue(&format!("/v1/files/{FILE}"), provider_error(500));
        }

        script_store_status(&env, STORE, FILE, status);

        let failed = wait_row(&env, id, "a failed row with a cleanup attempt", |row| {
            row.status == "failed" && row.cleanup_attempts >= 1
        })
        .await;
        assert_eq!(
            failed.error_code.as_deref(),
            Some("indexing_failed"),
            "{status}"
        );
        assert_eq!(
            failed.cleanup_status.as_deref(),
            Some("pending"),
            "{status}"
        );
        assert!(
            failed.deleted_at.is_none(),
            "{status}: the row is not soft-deleted"
        );
        let payloads = env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await;
        assert_eq!(payloads.len(), 1, "{status}: {payloads:?}");
        let payload = &payloads[0];
        assert_eq!(payload["event_type"], json!("attachment_indexing_failed"));
        assert_eq!(payload["attachment_id"], json!(id.to_string()));
        assert_eq!(payload["chat_id"], json!(chat));
        assert_eq!(payload["tenant_id"], json!(user.tenant_id));
        assert_eq!(payload["provider_file_id"], json!(FILE));
        assert_eq!(payload["attachment_kind"], json!("document"));
        let detail = get_attachment(&env, &user, &chat, &id.to_string())
            .await
            .json();
        assert_eq!(detail["status"], json!("failed"));
        assert_eq!(detail["error_code"], json!("indexing_failed"));
    }
}

#[tokio::test]
async fn the_outbox_cleanup_deletes_the_provider_file_of_a_background_failure() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    script_store_status(&env, STORE, FILE, "failed");

    let done = wait_row(&env, id, "a finished cleanup", |row| {
        row.cleanup_status.as_deref() == Some("done")
    })
    .await;
    assert_eq!(done.status, "failed");
    assert_eq!(done.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(
        calls_to(&env, "DELETE", &file_path(FILE)).len(),
        1,
        "the provider file is deleted by the outbox cleanup"
    );
}

#[tokio::test]
async fn a_non_transient_background_read_error_fails_the_attachment() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    script_store_status_error(&env, STORE, FILE, 404);

    let failed = wait_row(&env, id, "a failed row", |row| row.status == "failed").await;
    assert_eq!(failed.error_code.as_deref(), Some("indexing_failed"));
    assert!(failed.cleanup_status.is_some());
    let payloads = env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(
        payloads[0]["event_type"],
        json!("attachment_indexing_failed")
    );
}

#[tokio::test]
async fn the_background_task_gives_up_at_its_time_limit() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    // Nothing is scripted: every read says `in_progress` (an empty answer) until the 1.5 s limit.
    let failed = wait_row(&env, id, "the timeout failure", |row| {
        row.status == "failed"
    })
    .await;

    assert_eq!(failed.error_code.as_deref(), Some("indexing_failed"));
    assert!(failed.cleanup_status.is_some());
    assert!(status_reads(&env) >= 5, "it kept polling until the limit");
    let payloads = env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0]["event_type"],
        json!("attachment_indexing_failed")
    );
}

#[tokio::test]
async fn a_chat_deletion_stops_the_background_task_without_changes() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    delete_chat(&env, &user, &chat).await;
    script_store_status(&env, STORE, FILE, "completed");
    settle(1000).await;

    let stored = row(&env, id).await;
    assert_eq!(stored.status, "uploaded", "such a row never becomes ready");
    assert!(stored.error_code.is_none());
    assert!(
        env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty(),
        "no attachment_indexing_failed event: chat cleanup owns the provider file"
    );
    wait_calls(&env, "DELETE", &file_path(FILE), 1).await;
    wait_calls(&env, "DELETE", &vector_store_path(STORE), 1).await;
    wait_row(&env, id, "the chat cleanup of the file", |row| {
        row.cleanup_status.as_deref() == Some("done")
    })
    .await;
}

#[tokio::test]
async fn deleting_the_attachment_stops_the_background_task_without_changes() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    delete_ok(&env, &user, &chat, id).await;
    script_store_status(&env, STORE, FILE, "completed");
    settle(1000).await;

    let stored = row(&env, id).await;
    assert_eq!(
        stored.status, "uploaded",
        "a deleted row never becomes ready"
    );
    assert!(stored.deleted_at.is_some() && stored.error_code.is_none());
    let payloads = env.outbox_payloads(ATTACHMENT_CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1, "only the deletion event: {payloads:?}");
    assert_eq!(payloads[0]["event_type"], json!("attachment_deleted"));
}

// ===========================================================================
// A message and the tools while the document is still indexing
// ===========================================================================

#[tokio::test]
async fn a_message_cannot_reference_an_attachment_that_is_still_uploaded() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [id.to_string()] }),
    )
    .await;

    assert_invalid_argument(&resp, "attachment", "invalid_attachment");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn file_search_is_offered_only_once_the_document_is_ready() {
    let (env, user, chat) = indexing_env().await;
    let id = detail_id(&upload_in_progress(&env, &user, &chat).await);

    expect_stream(&send_text(&env, &user, &chat, "while indexing").await);
    let while_indexing = chat_request_body(&env, 0);
    script_store_status(&env, STORE, FILE, "completed");
    wait_status(&env, &user, &chat, id, "ready").await;
    expect_stream(&send_text(&env, &user, &chat, "after indexing").await);
    let after = chat_request_body(&env, 1);

    assert!(
        while_indexing.get("tools").is_none(),
        "no ready document yet: {while_indexing}"
    );
    assert_eq!(
        after["tools"][0],
        json!({ "type": "file_search", "vector_store_ids": [STORE], "max_num_results": 5 })
    );
    assert_eq!(
        store_rows(&env, &chat).await[0].vector_store_id.as_deref(),
        Some(STORE)
    );
    // And the attachment can be referenced now.
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "use it", "attachment_ids": [id.to_string()] }),
        )
        .await,
    );
}
