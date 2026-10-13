#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2 (and the metrics of slice 6): provider file storage and the
//! code-interpreter path (`cpt-cf-mini-chat-dod-attachments-provider-storage`,
//! `cpt-cf-mini-chat-algo-attachments-upload-provider-file`, `-mark-ready`,
//! `-fail-upload`): the exact provider request, the `ready` answer, and the
//! durable cleanup of every failure that leaves a provider file behind.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use toolkit_db::outbox::{LeasedMessageHandler as _, MessageResult};
use uuid::Uuid;

use crate::attachment_test_env::{ALIAS, ATTACHMENT_TYPE, AttEnv, AttOptions, parts_of};
use crate::chat_test_env::{CHAT_TYPE, Reply};
use crate::domain::service::attachments::model::AttachmentKind;
use crate::domain::service::attachments::ports::UploadResult;
use crate::domain::service::attachments::test_support::Metric;
use crate::test_doubles::{Fault, Gate, ProxyReply, S2S_SUBJECT, USER, USER_TENANT};

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const FILES: &str = "/api.openai.test/v1/files";

async fn setup() -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    (env, chat)
}

fn keys(json: &Value) -> BTreeSet<String> {
    json.as_object().unwrap().keys().cloned().collect()
}

fn attachment_id(reply: &Reply) -> Uuid {
    Uuid::parse_str(
        reply.json["id"]
            .as_str()
            .unwrap_or_else(|| panic!("no id in {}", reply.text)),
    )
    .unwrap()
}

/// A 503 with `Retry-After: 10`, the generic detail and no hint of the cause.
fn assert_storage_unavailable(reply: &Reply) {
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    assert_eq!(reply.header("retry-after"), Some("10"));
    assert_eq!(reply.json["context"]["retry_after_seconds"], 10);
    assert_eq!(reply.json["detail"], "Service temporarily unavailable");
    for leak in [
        "upload_failed",
        "indexing_failed",
        "vector_store_failed",
        "file-",
        "provider text",
    ] {
        assert!(
            !reply.text.contains(leak),
            "`{leak}` leaked into {}",
            reply.text
        );
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(60)).await;
}

// ================================================================================================
// The code-interpreter path: file upload, then ready
// ================================================================================================

#[tokio::test]
async fn a_spreadsheet_is_stored_with_the_provider_and_is_ready() {
    let (env, chat) = setup().await;

    let reply = env
        .upload(chat, "sheet.xlsx", XLSX, b"PK\x03\x04 cells")
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        keys(&reply.json),
        [
            "id",
            "filename",
            "content_type",
            "size_bytes",
            "status",
            "kind",
            "created_at"
        ]
        .iter()
        .map(|k| (*k).to_owned())
        .collect()
    );
    assert_eq!(reply.json["filename"], "sheet.xlsx");
    assert_eq!(reply.json["content_type"], XLSX);
    assert_eq!(reply.json["size_bytes"], 10);
    assert_eq!(reply.json["status"], "ready");
    assert_eq!(reply.json["kind"], "document");
    assert!(
        reply.headers.get("location").is_none(),
        "no Location header"
    );
    let (id, row) = env.only_att().await;
    assert_eq!(id, attachment_id(&reply));
    assert_eq!(row.status, "ready");
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert!(row.for_code_interpreter && !row.for_file_search);
    assert_eq!(row.storage_backend, "openai");
    assert_eq!(row.uploaded_by, USER);
    assert_eq!(row.tenant, USER_TENANT);
    assert_eq!(row.error_code, None);
    assert_eq!(row.cleanup_status, None);
    assert_eq!(reply.json["created_at"], row.created_at);
}

#[tokio::test]
async fn a_spreadsheet_makes_one_files_request_and_no_vector_store_call() {
    let (env, chat) = setup().await;

    env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_eq!(env.sequence(), vec![format!("POST {FILES}")]);
    assert!(env.stores().await.is_empty(), "no chat_vector_stores row");
}

#[tokio::test]
async fn the_files_request_is_multipart_with_purpose_assistants_then_the_named_file() {
    let (env, chat) = setup().await;
    let content: Vec<u8> = (0..=255_u8).cycle().take(3000).collect();

    let reply = env.upload(chat, "sheet.xlsx", XLSX, &content).await;

    let id = attachment_id(&reply);
    let uploads = env.file_uploads();
    assert_eq!(uploads.len(), 1);
    let parts = parts_of(&uploads[0]);
    assert_eq!(parts.len(), 2);
    assert_eq!(
        (parts[0].name.as_str(), parts[0].body.as_slice()),
        ("purpose", &b"assistants"[..])
    );
    assert_eq!(parts[1].name, "file");
    assert_eq!(
        parts[1].filename.as_deref(),
        Some(format!("{chat}_{id}.xlsx").as_str())
    );
    assert_eq!(parts[1].content_type.as_deref(), Some(XLSX));
    assert_eq!(
        parts[1].body, content,
        "the provider receives exactly the uploaded bytes"
    );
}

#[tokio::test]
async fn the_provider_part_carries_the_stored_type_and_extension_not_the_declared_ones() {
    let (env, chat) = setup().await;

    let reply = env
        .upload(
            chat,
            "My Sheet (final).XLSX",
            "Application/Octet-Stream; charset=binary",
            b"PK",
        )
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let parts = parts_of(&env.file_uploads()[0]);
    let id = attachment_id(&reply);
    assert_eq!(
        parts[1].filename.as_deref(),
        Some(format!("{chat}_{id}.xlsx").as_str())
    );
    assert_eq!(parts[1].content_type.as_deref(), Some(XLSX));
    assert_eq!(
        reply.json["filename"], "My Sheet (final).XLSX",
        "the user's name is kept in the row"
    );
}

#[tokio::test]
async fn a_csv_is_sent_to_the_provider_as_plain_text() {
    let (env, chat) = setup().await;
    env.oagw.on_path_json(
        "POST",
        &format!("/{ALIAS}/v1/vector_stores/*/files"),
        200,
        &json!({ "status": "completed" }),
    );

    let reply = env.upload(chat, "data.csv", "text/csv", b"a,b\n1,2").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let parts = parts_of(&env.file_uploads()[0]);
    let id = attachment_id(&reply);
    assert_eq!(
        parts[1].filename.as_deref(),
        Some(format!("{chat}_{id}.txt").as_str())
    );
    assert_eq!(parts[1].content_type.as_deref(), Some("text/plain"));
    assert_eq!(reply.json["content_type"], "text/plain");
}

#[tokio::test]
async fn the_provider_call_carries_the_request_context_and_no_credential() {
    let (env, chat) = setup().await;

    env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    let call = &env.file_uploads()[0];
    assert_eq!(call.subject, USER);
    assert_eq!(call.tenant, USER_TENANT);
    for (name, _) in &call.headers {
        let name = name.to_ascii_lowercase();
        assert!(
            !name.contains("authorization")
                && !name.contains("api-key")
                && !name.contains("cookie"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_upload_over_the_axum_default_body_limit_is_accepted() {
    let (env, chat) = setup().await;
    let big = vec![b'x'; 3 * 1024 * 1024];

    let reply = env.upload(chat, "big.xlsx", XLSX, &big).await;

    assert_eq!(
        reply.status,
        StatusCode::CREATED,
        "the route sets no body limit"
    );
    assert_eq!(parts_of(&env.file_uploads()[0])[1].body.len(), big.len());
}

#[tokio::test]
async fn the_201_detail_is_what_a_later_get_returns() {
    let (env, chat) = setup().await;
    let created = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    let got = env.get_attachment(chat, attachment_id(&created)).await;

    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.json, created.json);
}

// ================================================================================================
// A failed provider upload
// ================================================================================================

#[tokio::test]
async fn a_provider_error_is_503_and_the_row_is_failed_without_a_cleanup() {
    for status in [500_u16, 502, 429, 400, 401] {
        let (env, chat) = setup().await;
        env.oagw.on_path_json(
            "POST",
            FILES,
            status,
            &json!({ "error": { "message": "provider text" } }),
        );

        let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

        assert_storage_unavailable(&reply);
        let (_, row) = env.only_att().await;
        assert_eq!(row.status, "failed", "{status}");
        assert_eq!(row.error_code.as_deref(), Some("upload_failed"), "{status}");
        assert_eq!(row.provider_file_id, None);
        assert_eq!(row.cleanup_status, None, "nothing to delete");
        assert_eq!(env.stream.quota.outbox_message_count().await, 0, "{status}");
        assert_eq!(
            env.sequence(),
            vec![format!("POST {FILES}")],
            "{status}: the write is never retried"
        );
    }
}

#[tokio::test]
async fn an_answer_without_a_usable_id_fails_the_upload() {
    for body in [
        json!({}),
        json!({ "id": "" }),
        json!({ "id": 5 }),
        json!({ "id": null }),
    ] {
        let (env, chat) = setup().await;
        env.oagw.on_path_json("POST", FILES, 200, &body);

        let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

        assert_storage_unavailable(&reply);
        assert_eq!(
            env.only_att().await.1.error_code.as_deref(),
            Some("upload_failed"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_gateway_failure_fails_the_upload() {
    for fault in [Fault::Internal, Fault::DeadlineExceeded] {
        let (env, chat) = setup().await;
        env.oagw
            .on_path("POST", FILES, move |_| ProxyReply::Error(fault));

        let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

        assert_storage_unavailable(&reply);
        assert_eq!(
            env.only_att().await.1.error_code.as_deref(),
            Some("upload_failed"),
            "{fault:?}"
        );
    }
}

#[tokio::test]
async fn a_failed_upload_leaves_no_pending_gauge_behind() {
    let (env, chat) = setup().await;
    env.oagw.on_path_json("POST", FILES, 500, &json!({}));

    let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_storage_unavailable(&reply);
    assert_eq!(
        env.metrics.pending_balance(),
        0,
        "{:?}",
        env.metrics.events()
    );
}

// ================================================================================================
// The provider upload is lost: deleted, reaped or chat deleted while it runs
// ================================================================================================

/// An upload whose provider call is held at the returned gate; the file the
/// provider will return is `file-held`.
async fn hold_the_upload(
    env: &Arc<AttEnv>,
    chat: Uuid,
) -> (tokio::task::JoinHandle<Reply>, Arc<Gate>, Uuid) {
    let gate = Gate::new();
    let held = Arc::clone(&gate);
    env.oagw.on_path("POST", FILES, move |_| {
        ProxyReply::json(200, &json!({ "id": "file-held" })).held_by(&held)
    });
    let task_env = Arc::clone(env);
    let task = tokio::spawn(async move { task_env.upload(chat, "sheet.xlsx", XLSX, b"PK").await });
    gate.arrived().await;
    let (id, row) = env.only_att().await;
    assert_eq!(
        row.status, "pending",
        "the row exists before the provider is called"
    );
    (task, gate, id)
}

fn orphans(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|m| m["event_type"] == "attachment_orphan_file")
        .collect()
}

#[tokio::test]
async fn an_attachment_deleted_during_the_upload_is_404_and_the_new_file_gets_an_orphan_message() {
    let env = Arc::new(AttEnv::new().await);
    let chat = env.chat("m-oa").await;
    let (task, gate, id) = hold_the_upload(&env, chat).await;

    let deleted = env.delete_attachment(chat, id).await;
    gate.release();
    let reply = task.await.unwrap();

    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);
    reply.assert_not_found(ATTACHMENT_TYPE);
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0]["event_type"], "attachment_deleted");
    assert_eq!(
        messages[0]["provider_file_id"],
        json!(null),
        "the file was not stored yet"
    );
    let orphan = orphans(&messages);
    assert_eq!(orphan.len(), 1);
    assert_eq!(orphan[0]["provider_file_id"], "file-held");
    assert_eq!(orphan[0]["attachment_id"], id.to_string());
    assert_eq!(orphan[0]["chat_id"], chat.to_string());
    assert_eq!(orphan[0]["tenant_id"], USER_TENANT.to_string());
    assert_eq!(orphan[0]["storage_backend"], "openai");
    assert_eq!(orphan[0]["secondary_ref"], json!(null));
    assert_eq!(env.deletes().len(), 0, "the request deletes nothing itself");
    let row = env.att(id).await;
    assert_eq!(row.provider_file_id, None, "no row can carry the new id");
    assert_eq!(row.status, "pending");
}

#[tokio::test]
async fn a_chat_deleted_during_the_upload_is_404_with_the_chat_type_and_an_orphan_message() {
    let env = Arc::new(AttEnv::new().await);
    let chat = env.chat("m-oa").await;
    let (task, gate, id) = hold_the_upload(&env, chat).await;

    env.delete_chat(chat).await;
    gate.release();
    let reply = task.await.unwrap();

    reply.assert_not_found(CHAT_TYPE);
    let messages = env.cleanup_messages().await;
    let orphan = orphans(&messages);
    assert_eq!(orphan.len(), 1, "{messages:?}");
    assert_eq!(orphan[0]["provider_file_id"], "file-held");
    assert_eq!(env.att(id).await.provider_file_id, None);
    assert_eq!(env.att(id).await.status, "pending");
}

/// Soft-deletes the chat through the product route right before `method` of the
/// store runs (the chat was loaded earlier in the request).
fn delete_chat_before(env: &Arc<AttEnv>, method: &'static str, chat: Uuid) {
    let task_env = Arc::clone(env);
    env.repo.before(method, move || {
        let env = Arc::clone(&task_env);
        Box::pin(async move { env.delete_chat(chat).await })
    });
}

#[tokio::test]
async fn a_chat_deleted_between_its_load_and_the_pending_insert_is_404_with_no_row_and_no_provider_call()
 {
    let env = Arc::new(AttEnv::new().await);
    env.script_happy();
    let chat = env.chat("m-oa").await;
    delete_chat_before(&env, "insert_pending", chat);

    let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    reply.assert_not_found(CHAT_TYPE);
    assert_eq!(env.att_count().await, 0, "the insert is rolled back");
    assert!(env.sequence().is_empty(), "{:?}", env.sequence());
    assert!(
        orphans(&env.cleanup_messages().await).is_empty(),
        "no file, no orphan message"
    );
}

#[tokio::test]
async fn a_row_reaped_during_the_upload_is_404_and_the_new_file_gets_an_orphan_message() {
    let env = Arc::new(AttEnv::new().await);
    let chat = env.chat("m-oa").await;
    let (task, gate, id) = hold_the_upload(&env, chat).await;
    env.exec(&format!(
        "UPDATE attachments SET status = 'failed', error_code = 'upload_abandoned' WHERE id = {}",
        crate::chat_test_env::blob(id)
    ))
    .await;

    gate.release();
    let reply = task.await.unwrap();

    reply.assert_not_found(ATTACHMENT_TYPE);
    let orphan_messages = env.cleanup_messages().await;
    assert_eq!(orphans(&orphan_messages).len(), 1);
    assert_eq!(env.att(id).await.status, "failed");
    assert_eq!(env.att(id).await.provider_file_id, None);
}

#[tokio::test]
async fn when_the_orphan_enqueue_fails_the_new_file_gets_one_last_resort_delete() {
    let env = Arc::new(AttEnv::new().await);
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let (task, gate, id) = hold_the_upload(&env, chat).await;
    env.exec(&format!(
        "UPDATE attachments SET deleted_at = '{}' WHERE id = {}",
        AttEnv::stamp(env.now()),
        crate::chat_test_env::blob(id)
    ))
    .await;
    env.fail_outbox().await;

    gate.release();
    let reply = task.await.unwrap();

    reply.assert_not_found(ATTACHMENT_TYPE);
    env.wait_until("the best-effort delete", || env.deletes().len() == 1)
        .await;
    let delete = &env.deletes()[0];
    assert_eq!(delete.uri, format!("/{ALIAS}/v1/files/file-held"));
    assert_eq!(
        delete.subject, S2S_SUBJECT,
        "outside the request the S2S context is used"
    );
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

// ================================================================================================
// Database errors around a stored provider file: durable cleanup
// ================================================================================================

async fn assert_failed_with_durable_cleanup(env: &AttEnv, chat: Uuid, file: &str) -> Uuid {
    let (id, row) = env.only_att().await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(
        row.provider_file_id.as_deref(),
        Some(file),
        "the file id is on the row"
    );
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = env.cleanup_messages().await;
    assert_eq!(
        messages.len(),
        1,
        "exactly one cleanup message: {messages:?}"
    );
    assert_eq!(messages[0]["event_type"], "attachment_upload_failed");
    assert_eq!(messages[0]["provider_file_id"], file);
    assert_eq!(messages[0]["attachment_id"], id.to_string());
    assert_eq!(messages[0]["chat_id"], chat.to_string());
    assert_eq!(messages[0]["tenant_id"], USER_TENANT.to_string());
    assert_eq!(messages[0]["vector_store_id"], json!(null));
    assert_eq!(messages[0]["storage_backend"], "openai");
    assert_eq!(messages[0]["attachment_kind"], "document");
    assert_eq!(messages[0].as_object().unwrap().len(), 10);
    id
}

#[tokio::test]
async fn a_database_error_on_the_uploaded_write_fails_the_row_with_the_file_recorded_durably() {
    let (env, chat) = setup().await;
    env.fail_updates("no_uploaded", "NEW.status = 'uploaded'")
        .await;

    let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(reply.category().contains("internal"));
    assert_failed_with_durable_cleanup(&env, chat, "file-1").await;
    settle().await;
    assert!(
        env.deletes().is_empty(),
        "no provider delete is sent by the request"
    );
}

#[tokio::test]
async fn a_database_error_on_the_ready_write_fails_the_row_with_the_file_recorded_durably() {
    let (env, chat) = setup().await;
    env.fail_updates("no_ready", "NEW.status = 'ready'").await;

    let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(!reply.text.contains("file-1"), "{}", reply.text);
    assert_failed_with_durable_cleanup(&env, chat, "file-1").await;
    settle().await;
    assert!(
        env.deletes().is_empty(),
        "the cleanup handler deletes the file, not the request"
    );
}

#[tokio::test]
async fn the_recorded_cleanup_is_executed_by_the_handler_with_retries() {
    let (env, chat) = setup().await;
    env.fail_updates("no_ready", "NEW.status = 'ready'").await;
    env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;
    let message = env.cleanup_messages().await.remove(0);
    let handler = env.cleanup_handler();
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 500, &json!({}));

    let failed = handler.handle(&AttEnv::message(&message, 0)).await;
    env.oagw
        .on_path_json("DELETE", &format!("/{ALIAS}/v1/files/*"), 200, &json!({}));
    let done = handler.handle(&AttEnv::message(&message, 1)).await;

    assert!(matches!(failed, MessageResult::Retry), "{failed:?}");
    assert!(matches!(done, MessageResult::Ok), "{done:?}");
    let (_, row) = env.only_att().await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 1);
    assert_eq!(env.deletes().len(), 2);
    assert!(
        env.deletes()
            .iter()
            .all(|d| d.uri == format!("/{ALIAS}/v1/files/file-1"))
    );
}

#[tokio::test]
async fn when_the_failure_transaction_fails_too_a_file_held_only_in_memory_is_deleted_best_effort()
{
    let (env, chat) = setup().await;
    env.fail_updates(
        "no_uploaded_or_failed",
        "NEW.status IN ('uploaded', 'failed')",
    )
    .await;

    let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    env.wait_until("the last-resort delete", || env.deletes().len() == 1)
        .await;
    let delete = &env.deletes()[0];
    assert_eq!(delete.uri, format!("/{ALIAS}/v1/files/file-1"));
    assert_eq!(delete.subject, S2S_SUBJECT);
    settle().await;
    assert_eq!(env.deletes().len(), 1, "one attempt, not retried");
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
    assert_eq!(env.only_att().await.1.status, "pending");
}

#[tokio::test]
async fn when_the_failure_transaction_fails_a_stored_file_id_is_left_to_the_reaper() {
    let (env, chat) = setup().await;
    env.fail_updates("no_ready_or_failed", "NEW.status IN ('ready', 'failed')")
        .await;

    let reply = env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    settle().await;
    assert!(
        env.deletes().is_empty(),
        "a stored file id is not at risk: no request-side delete"
    );
    let (id, row) = env.only_att().await;
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    // The reaper finishes it once the database is healthy again.
    env.heal("no_ready_or_failed").await;
    env.advance(400);
    let report = env
        .reaper(Arc::new(crate::infra::workers::leader::NoopLeaderElector))
        .scan_once()
        .await;
    assert_eq!(report.abandoned, 1);
    let row = env.att(id).await;
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["event_type"], "attachment_upload_abandoned");
    assert_eq!(messages[0]["provider_file_id"], "file-1");
}

// ================================================================================================
// Metrics of an upload (cpt-cf-mini-chat-algo-attachments-record-metrics)
// ================================================================================================

#[tokio::test]
async fn a_201_is_counted_ok_with_its_size_and_leaves_the_pending_gauge_at_zero() {
    let (env, chat) = setup().await;

    env.upload(chat, "sheet.xlsx", XLSX, b"PK\x03\x04 cells")
        .await;

    assert_eq!(
        env.metrics.uploads(),
        vec![(AttachmentKind::Document, UploadResult::Ok)]
    );
    assert_eq!(
        env.metrics
            .count(&Metric::UploadBytes(AttachmentKind::Document, 10)),
        1
    );
    assert_eq!(
        env.metrics.pending_balance(),
        0,
        "{:?}",
        env.metrics.events()
    );
    assert!(
        env.metrics.events().contains(&Metric::Pending(1)),
        "the gauge goes up once the insert committed: {:?}",
        env.metrics.events()
    );
}

#[tokio::test]
async fn a_provider_failure_after_the_insert_is_counted_failed() {
    let (env, chat) = setup().await;
    env.oagw.on_path_json("POST", FILES, 500, &json!({}));

    env.upload(chat, "sheet.xlsx", XLSX, b"PK").await;

    assert_eq!(
        env.metrics.uploads(),
        vec![(AttachmentKind::Document, UploadResult::Failed)]
    );
}

#[tokio::test]
async fn limit_and_size_rejections_are_counted_rejected_by_kind() {
    let mut options = AttOptions::default();
    options.rag.max_documents_per_chat = 1;
    options.rag.uploaded_image_max_size_kb = 1;
    let env = AttEnv::with(options).await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row).await;

    env.upload(chat, "second.pdf", "application/pdf", b"%PDF")
        .await;
    env.upload(chat, "big.png", "image/png", &vec![0_u8; 2048])
        .await;

    assert_eq!(
        env.metrics.uploads(),
        vec![
            (AttachmentKind::Document, UploadResult::Rejected),
            (AttachmentKind::Image, UploadResult::Rejected)
        ]
    );
    assert_eq!(env.metrics.pending_balance(), 0);
}

#[tokio::test]
async fn an_upload_that_fails_classification_is_not_counted() {
    let (env, chat) = setup().await;

    env.upload(chat, "a.zip", "application/zip", b"x").await;
    env.upload(chat, "x.xlsx", XLSX, b"PK").await;

    assert_eq!(
        env.metrics.uploads(),
        vec![(AttachmentKind::Document, UploadResult::Ok)],
        "only requests that passed classification are counted: {:?}",
        env.metrics.events()
    );
}
