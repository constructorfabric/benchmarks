#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the chat vector store and document indexing
//! (`cpt-cf-mini-chat-dod-attachments-vector-store-indexing`,
//! `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`, `-index-document`):
//! the exact requests, lazy creation with the race-safe placeholder, status
//! polling inside the request, and the failures with their durable cleanup.
//!
//! The 25 s deadline and the background continuation (paused time) are in
//! `domain/service/attachments/service_timing_tests.rs`.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::attachment_test_env::{
    ALIAS, ANTHROPIC_ALIAS, AZURE_ALIAS, AZURE_API_VERSION, AttEnv, AttOptions, parts_of,
};
use crate::chat_test_env::Reply;
use crate::domain::service::attachments::ports::UploadResult;
use crate::test_doubles::{Fault, Gate, ProxyReply, S2S_SUBJECT, USER_TENANT};

const PDF: &str = "application/pdf";
const CREATE: &str = "/api.openai.test/v1/vector_stores";
const ADD: &str = "/api.openai.test/v1/vector_stores/*/files";
const STATUS: &str = "/api.openai.test/v1/vector_stores/*/files/*";

async fn setup() -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    (env, chat)
}

fn attachment_id(reply: &Reply) -> Uuid {
    Uuid::parse_str(
        reply.json["id"]
            .as_str()
            .unwrap_or_else(|| panic!("no id in {}", reply.text)),
    )
    .unwrap()
}

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
        "indexing_failed",
        "vector_store_failed",
        "upload_failed",
        "vs_",
        "file-",
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

/// The failed row of a document whose provider file is stored, with its one
/// durable cleanup message of `event`.
async fn assert_failed_with_cleanup(env: &AttEnv, chat: Uuid, code: &str, event: &str) -> Uuid {
    let (id, row) = env.only_att().await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some(code));
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = env.cleanup_messages().await;
    assert_eq!(
        messages.len(),
        1,
        "exactly one cleanup message: {messages:?}"
    );
    assert_eq!(messages[0]["event_type"], event);
    assert_eq!(messages[0]["provider_file_id"], "file-1");
    assert_eq!(messages[0]["vector_store_id"], json!(null));
    assert_eq!(messages[0]["attachment_id"], id.to_string());
    assert_eq!(messages[0]["chat_id"], chat.to_string());
    assert_eq!(messages[0].as_object().unwrap().len(), 10);
    id
}

// ================================================================================================
// The happy path
// ================================================================================================

#[tokio::test]
async fn the_first_document_creates_the_store_adds_the_file_and_is_ready() {
    let (env, chat) = setup().await;

    let reply = env.upload(chat, "report.pdf", PDF, b"%PDF-1.7 hello").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let id = attachment_id(&reply);
    assert_eq!(
        env.sequence(),
        vec![
            format!("POST /{ALIAS}/v1/files"),
            format!("POST /{ALIAS}/v1/vector_stores"),
            format!("POST /{ALIAS}/v1/vector_stores/vs_1/files"),
        ]
    );
    assert_eq!(reply.json["status"], "ready");
    assert_eq!(reply.json["filename"], "report.pdf");
    assert_eq!(reply.json["content_type"], PDF);
    assert_eq!(reply.json["kind"], "document");
    assert_eq!(reply.json["size_bytes"], 14);
    assert_eq!(reply.json.as_object().unwrap().len(), 7, "{}", reply.text);
    let files = parts_of(&env.file_uploads()[0]);
    assert_eq!(files[0].body, b"assistants");
    assert_eq!(
        files[1].filename.as_deref(),
        Some(format!("{chat}_{id}.pdf").as_str())
    );
    let create = &env.oagw.requests_to("POST", "/vector_stores")[0];
    assert_eq!(
        create.json(),
        json!({ "name": format!("mini-chat-{chat}") })
    );
    let add = env
        .calls_ending("POST", "/vector_stores/vs_1/files")
        .remove(0);
    assert_eq!(
        add.json(),
        json!({ "file_id": "file-1", "attributes": { "attachment_id": id.to_string() } })
    );
    let (_, row) = env.only_att().await;
    assert_eq!(row.status, "ready");
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.storage_backend, "openai");
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_1".to_owned()), "openai".to_owned())]
    );
    assert_eq!(
        env.stream
            .quota
            .db
            .query_i64("SELECT file_count FROM chat_vector_stores")
            .await,
        0,
        "file_count is reserved"
    );
}

#[tokio::test]
async fn a_second_document_reuses_the_store_without_a_create_call() {
    let (env, chat) = setup().await;
    let first = env.upload(chat, "a.pdf", PDF, b"%PDF a").await;

    let second = env.upload(chat, "b.pdf", PDF, b"%PDF b").await;

    assert_eq!(second.status, StatusCode::CREATED, "{}", second.text);
    assert_eq!(
        env.calls_ending("POST", "/v1/vector_stores").len(),
        1,
        "one create call in total"
    );
    let adds = env.calls_ending("POST", "/vector_stores/vs_1/files");
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[1].json()["file_id"], "file-2");
    assert_eq!(
        adds[1].json()["attributes"]["attachment_id"],
        attachment_id(&second).to_string()
    );
    assert_eq!(env.stores().await.len(), 1);
    let got = env.get_attachment(chat, attachment_id(&first)).await;
    assert_eq!(got.json, first.json, "GET returns the same detail");
}

#[tokio::test]
async fn the_vector_store_is_per_chat() {
    let (env, chat) = setup().await;
    let other = env.chat("m-oa").await;

    env.upload(chat, "a.pdf", PDF, b"%PDF").await;
    env.upload(other, "b.pdf", PDF, b"%PDF").await;

    assert_eq!(env.stores().await.len(), 2);
    assert_eq!(env.calls_ending("POST", "/v1/vector_stores").len(), 2);
}

#[tokio::test]
async fn an_azure_chat_uses_the_azure_routes_and_labels_the_store_azure() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-az").await;
    let az = format!("/{AZURE_ALIAS}/openai");
    let q = format!("?api-version={AZURE_API_VERSION}");
    env.oagw.on_path_json(
        "POST",
        &format!("{az}/files"),
        200,
        &json!({ "id": "file-az" }),
    );
    env.oagw.on_path_json(
        "POST",
        &format!("{az}/vector_stores"),
        200,
        &json!({ "id": "vs_az" }),
    );
    env.oagw.on_path_json(
        "POST",
        &format!("{az}/vector_stores/*/files"),
        200,
        &json!({ "status": "completed" }),
    );

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        env.oagw
            .proxied()
            .iter()
            .map(|c| c.uri.clone())
            .collect::<Vec<_>>(),
        vec![
            format!("{az}/files{q}"),
            format!("{az}/vector_stores{q}"),
            format!("{az}/vector_stores/vs_az/files{q}"),
        ]
    );
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_az".to_owned()), "azure".to_owned())]
    );
    assert_eq!(env.only_att().await.1.storage_backend, "azure");
}

#[tokio::test]
async fn an_anthropic_chat_indexes_documents_in_its_rag_provider_without_a_secondary_copy() {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat("m-an").await;

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert!(
        env.oagw
            .proxied()
            .iter()
            .all(|c| !c.uri.contains(ANTHROPIC_ALIAS)),
        "documents are not copied to Anthropic: {:?}",
        env.sequence()
    );
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_1".to_owned()), "openai".to_owned())]
    );
    assert_eq!(env.only_att().await.1.secondary_status, "not_attempted");
}

#[tokio::test]
async fn images_and_spreadsheets_never_touch_the_vector_store() {
    let (env, chat) = setup().await;

    let image = env
        .upload(
            chat,
            "pic.png",
            "image/png",
            &crate::attachment_test_env::png_bytes(8, 8),
        )
        .await;
    let sheet = env
        .upload(
            chat,
            "sheet.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            b"PK",
        )
        .await;

    assert_eq!(image.json["status"], "ready", "{}", image.text);
    assert_eq!(sheet.json["status"], "ready", "{}", sheet.text);
    assert!(
        env.oagw
            .proxied()
            .iter()
            .all(|c| !c.uri.contains("vector_stores")),
        "{:?}",
        env.sequence()
    );
    assert!(env.stores().await.is_empty());
}

#[tokio::test]
async fn the_file_search_kill_switch_does_not_stop_indexing() {
    let env = AttEnv::with(AttOptions {
        switches: mini_chat_sdk::KillSwitches {
            disable_file_search: true,
            ..crate::domain::service::quota::test_support::NO_SWITCHES
        },
        ..AttOptions::default()
    })
    .await;
    env.script_happy();
    let chat = env.chat("m-oa").await;

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(env.calls_ending("POST", "/vs_1/files").len(), 1);
}

// ================================================================================================
// Waiting for the index
// ================================================================================================

#[tokio::test]
async fn an_in_progress_file_is_polled_until_completed() {
    let (env, chat) = setup().await;
    env.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    env.oagw.on_path_sequence(
        "GET",
        STATUS,
        vec![
            (200, json!({ "status": "in_progress" })),
            (200, json!({ "status": "completed" })),
        ],
    );
    let started = std::time::Instant::now();

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["status"], "ready");
    let reads = env.calls_ending("GET", "/vector_stores/vs_1/files/file-1");
    assert_eq!(reads.len(), 2, "{:?}", env.sequence());
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "250 ms then 500 ms between the reads"
    );
    assert_eq!(env.only_att().await.1.status, "ready");
}

#[tokio::test]
async fn a_missing_status_counts_as_in_progress() {
    let (env, chat) = setup().await;
    env.oagw
        .on_path_json("POST", ADD, 200, &json!({ "id": "file-1" }));
    env.oagw.on_path_sequence(
        "GET",
        STATUS,
        vec![(200, json!({})), (200, json!({ "status": "completed" }))],
    );

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.json["status"], "ready", "{}", reply.text);
    assert_eq!(env.calls_ending("GET", "/files/file-1").len(), 2);
}

#[tokio::test]
async fn provider_5xx_and_gateway_failures_on_a_status_read_keep_polling() {
    let (env, chat) = setup().await;
    env.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    let reads = std::sync::atomic::AtomicUsize::new(0);
    env.oagw.on_path("GET", STATUS, move |_| {
        match reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => ProxyReply::json(500, &json!({})),
            1 => ProxyReply::Error(Fault::DeadlineExceeded),
            2 => ProxyReply::json(502, &json!({})).from_gateway(),
            _ => ProxyReply::json(200, &json!({ "status": "completed" })),
        }
    });

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["status"], "ready");
    assert_eq!(env.calls_ending("GET", "/files/file-1").len(), 4);
}

#[tokio::test]
async fn a_terminal_or_non_transient_status_fails_the_indexing_within_the_deadline() {
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Box<dyn Fn(&AttEnv)>)> = vec![
        (
            "failed",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "failed" }));
            }),
        ),
        (
            "cancelled",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "cancelled" }));
            }),
        ),
        (
            "unknown value",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "banana" }));
            }),
        ),
        (
            "add-file 500",
            Box::new(|env| env.oagw.on_path_json("POST", ADD, 500, &json!({}))),
        ),
        (
            "add-file 400",
            Box::new(|env| env.oagw.on_path_json("POST", ADD, 400, &json!({}))),
        ),
        (
            "add-file not JSON",
            Box::new(|env| {
                env.oagw.on_path("POST", ADD, |_| ProxyReply::Response {
                    status: 200,
                    headers: vec![],
                    body: crate::test_doubles::ReplyBody::Bytes(b"not json".to_vec()),
                    source: None,
                });
            }),
        ),
        (
            "read says failed",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
                env.oagw
                    .on_path_json("GET", STATUS, 200, &json!({ "status": "failed" }));
            }),
        ),
        (
            "read says cancelled",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
                env.oagw
                    .on_path_json("GET", STATUS, 200, &json!({ "status": "cancelled" }));
            }),
        ),
        (
            "read 404",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
                env.oagw.on_path_json("GET", STATUS, 404, &json!({}));
            }),
        ),
        (
            "read 429",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
                env.oagw.on_path_json("GET", STATUS, 429, &json!({}));
            }),
        ),
        (
            "read not JSON",
            Box::new(|env| {
                env.oagw
                    .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
                env.oagw.on_path("GET", STATUS, |_| ProxyReply::Response {
                    status: 200,
                    headers: vec![],
                    body: crate::test_doubles::ReplyBody::Bytes(b"<html>".to_vec()),
                    source: None,
                });
            }),
        ),
    ];
    for (label, script) in cases {
        let (env, chat) = setup().await;
        script(&env);

        let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

        assert_storage_unavailable(&reply);
        assert_failed_with_cleanup(&env, chat, "indexing_failed", "attachment_indexing_failed")
            .await;
        settle().await;
        assert!(
            env.deletes().is_empty(),
            "{label}: the request sends no provider delete"
        );
        assert_eq!(
            env.metrics.uploads().last().map(|u| u.1),
            Some(UploadResult::Failed),
            "{label}"
        );
    }
}

#[tokio::test]
async fn the_recorded_indexing_cleanup_is_executed_by_the_handler() {
    use toolkit_db::outbox::{LeasedMessageHandler as _, MessageResult};
    let (env, chat) = setup().await;
    env.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "failed" }));
    env.upload(chat, "a.pdf", PDF, b"%PDF").await;
    let message = env.cleanup_messages().await.remove(0);

    let result = env
        .cleanup_handler()
        .handle(&AttEnv::message(&message, 0))
        .await;

    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(env.deletes().len(), 1);
    assert_eq!(env.deletes()[0].uri, format!("/{ALIAS}/v1/files/file-1"));
    assert_eq!(
        env.only_att().await.1.cleanup_status.as_deref(),
        Some("done")
    );
}

// ================================================================================================
// Store creation failures
// ================================================================================================

#[tokio::test]
async fn a_failed_store_creation_fails_the_row_and_leaves_no_placeholder() {
    for create in [
        ProxyReply::json(500, &json!({})),
        ProxyReply::json(200, &json!({})),
        ProxyReply::json(200, &json!({ "id": "" })),
        ProxyReply::Error(Fault::Internal),
    ] {
        let (env, chat) = setup().await;
        let once = std::sync::Mutex::new(Some(create));
        env.oagw.on_path("POST", CREATE, move |_| {
            once.lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| ProxyReply::json(200, &json!({ "id": "vs_2" })))
        });

        let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

        assert_storage_unavailable(&reply);
        assert_failed_with_cleanup(
            &env,
            chat,
            "vector_store_failed",
            "attachment_upload_failed",
        )
        .await;
        assert!(env.stores().await.is_empty(), "the placeholder is deleted");
        settle().await;
        assert!(env.deletes().is_empty());
        // The chat recovers: the next upload creates the store.
        env.oagw.clear_faults();
        let next = env.upload(chat, "b.pdf", PDF, b"%PDF").await;
        assert_eq!(next.status, StatusCode::CREATED, "{}", next.text);
        assert_eq!(env.stores().await.len(), 1);
    }
}

#[tokio::test]
async fn a_chat_deleted_before_the_placeholder_insert_gets_no_placeholder_and_no_provider_store() {
    let env = std::sync::Arc::new(AttEnv::new().await);
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let task_env = std::sync::Arc::clone(&env);
    env.repo.before("insert_store_placeholder", move || {
        let env = std::sync::Arc::clone(&task_env);
        Box::pin(async move { env.delete_chat(chat).await })
    });

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_ne!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert!(env.stores().await.is_empty(), "no placeholder row");
    assert!(
        env.calls_ending("POST", "/vector_stores").is_empty(),
        "no vector store is created for a deleted chat: {:?}",
        env.sequence()
    );
}

// ================================================================================================
// provider_mismatch
// ================================================================================================

#[tokio::test]
async fn a_document_into_a_chat_whose_store_has_another_provider_is_409_before_anything_is_stored()
{
    let (env, chat) = setup().await;
    env.seed_store(chat, Some("vs_az"), "azure", env.now())
        .await;

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert!(
        reply.category().contains("already_exists"),
        "{}",
        reply.text
    );
    assert_eq!(reply.json["context"]["resource_name"], "provider_mismatch");
    assert_eq!(
        reply.json["context"]["resource_type"],
        crate::attachment_test_env::ATTACHMENT_TYPE
    );
    assert_eq!(env.att_count().await, 0, "nothing is stored");
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn images_and_spreadsheets_are_still_accepted_into_a_mismatched_chat() {
    let (env, chat) = setup().await;
    env.seed_store(chat, Some("vs_az"), "azure", env.now())
        .await;

    let image = env
        .upload(
            chat,
            "a.png",
            "image/png",
            &crate::attachment_test_env::png_bytes(4, 4),
        )
        .await;
    let sheet = env
        .upload(
            chat,
            "a.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            b"PK",
        )
        .await;

    assert_eq!(image.status, StatusCode::CREATED, "{}", image.text);
    assert_eq!(sheet.status, StatusCode::CREATED, "{}", sheet.text);
}

#[tokio::test]
async fn a_mismatch_that_appears_during_the_upload_fails_the_row_with_its_file_cleaned_up() {
    let env = Arc::new(AttEnv::new().await);
    let chat = env.chat("m-oa").await;
    env.script_happy();
    let gate = Gate::new();
    let held = Arc::clone(&gate);
    env.oagw
        .on_path("POST", &format!("/{ALIAS}/v1/files"), move |_| {
            ProxyReply::json(200, &json!({ "id": "file-1" })).held_by(&held)
        });
    let task_env = Arc::clone(&env);
    let task = tokio::spawn(async move { task_env.upload(chat, "a.pdf", PDF, b"%PDF").await });
    gate.arrived().await;
    env.seed_store(chat, Some("vs_az"), "azure", env.now())
        .await;

    gate.release();
    let reply = task.await.unwrap();

    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert_eq!(reply.json["context"]["resource_name"], "provider_mismatch");
    assert_failed_with_cleanup(
        &env,
        chat,
        "vector_store_failed",
        "attachment_upload_failed",
    )
    .await;
    assert_eq!(
        env.calls_ending("POST", "/v1/vector_stores").len(),
        0,
        "no store was created"
    );
}

// ================================================================================================
// The race-safe placeholder
// ================================================================================================

#[tokio::test]
async fn two_concurrent_first_uploads_create_exactly_one_store() {
    let env = Arc::new(
        AttEnv::with(AttOptions {
            file_db: true,
            ..AttOptions::default()
        })
        .await,
    );
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let gate = Gate::new();
    let held = Arc::clone(&gate);
    env.oagw.on_path("POST", CREATE, move |_| {
        ProxyReply::json(200, &json!({ "id": "vs_1" })).held_by(&held)
    });
    let first = {
        let env = Arc::clone(&env);
        tokio::spawn(async move { env.upload(chat, "a.pdf", PDF, b"%PDF a").await })
    };
    let second = {
        let env = Arc::clone(&env);
        tokio::spawn(async move { env.upload(chat, "b.pdf", PDF, b"%PDF b").await })
    };
    gate.arrived().await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    gate.release();
    let (first, second) = (first.await.unwrap(), second.await.unwrap());

    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
    assert_eq!(second.status, StatusCode::CREATED, "{}", second.text);
    assert_eq!(first.json["status"], "ready");
    assert_eq!(second.json["status"], "ready");
    assert_eq!(
        env.calls_ending("POST", "/v1/vector_stores").len(),
        1,
        "one provider create: {:?}",
        env.sequence()
    );
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_1".to_owned()), "openai".to_owned())]
    );
    let adds = env.calls_ending("POST", "/vector_stores/vs_1/files");
    assert_eq!(adds.len(), 2, "both files go into the same store");
    assert_eq!(env.att_count().await, 2);
}

#[tokio::test]
async fn a_stale_placeholder_is_reclaimed_by_the_next_upload() {
    let (env, chat) = setup().await;
    env.seed_store(
        chat,
        None,
        "openai",
        env.now() - time::Duration::seconds(121),
    )
    .await;

    let reply = env.upload(chat, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(env.calls_ending("POST", "/v1/vector_stores").len(), 1);
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_1".to_owned()), "openai".to_owned())]
    );
}

#[tokio::test]
async fn a_fresh_placeholder_is_waited_for_and_never_created_again() {
    let env = Arc::new(AttEnv::new().await);
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let placeholder = env
        .seed_store(
            chat,
            None,
            "openai",
            env.now() - time::Duration::seconds(30),
        )
        .await;
    let task_env = Arc::clone(&env);
    let task = tokio::spawn(async move { task_env.upload(chat, "a.pdf", PDF, b"%PDF").await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    env.exec(&format!(
        "UPDATE chat_vector_stores SET vector_store_id = 'vs_existing' WHERE id = {}",
        crate::chat_test_env::blob(placeholder)
    ))
    .await;
    let reply = task.await.unwrap();

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        env.calls_ending("POST", "/v1/vector_stores").len(),
        0,
        "the loser never calls the provider"
    );
    assert_eq!(
        env.calls_ending("POST", "/vector_stores/vs_existing/files")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_lost_compare_and_set_deletes_the_new_empty_store_best_effort_and_uses_the_existing_one()
{
    let env = Arc::new(AttEnv::new().await);
    env.script_happy();
    let chat = env.chat("m-oa").await;
    let gate = Gate::new();
    let held = Arc::clone(&gate);
    env.oagw.on_path("POST", CREATE, move |_| {
        ProxyReply::json(200, &json!({ "id": "vs_new" })).held_by(&held)
    });
    let task_env = Arc::clone(&env);
    let task = tokio::spawn(async move { task_env.upload(chat, "a.pdf", PDF, b"%PDF").await });
    gate.arrived().await;
    // The placeholder was reclaimed and another upload bound a store meanwhile.
    env.exec("DELETE FROM chat_vector_stores").await;
    env.seed_store(chat, Some("vs_other"), "openai", env.now())
        .await;

    gate.release();
    let reply = task.await.unwrap();

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    env.wait_until("the best-effort store delete", || {
        !env.calls_ending("DELETE", "/vector_stores/vs_new")
            .is_empty()
    })
    .await;
    let delete = &env.calls_ending("DELETE", "/vector_stores/vs_new")[0];
    assert_eq!(delete.subject, S2S_SUBJECT);
    settle().await;
    assert_eq!(
        env.calls_ending("DELETE", "/vector_stores/vs_new").len(),
        1,
        "not retried"
    );
    assert_eq!(
        env.calls_ending("POST", "/vector_stores/vs_other/files")
            .len(),
        1
    );
    assert!(
        env.calls_ending("POST", "/vector_stores/vs_new/files")
            .is_empty()
    );
    assert_eq!(
        env.cleanup_messages().await.len(),
        0,
        "no cleanup message for the orphan store"
    );
    assert_eq!(
        env.stores().await,
        vec![(Some("vs_other".to_owned()), "openai".to_owned())]
    );
    let _ = (USER_TENANT, Value::Null);
}
