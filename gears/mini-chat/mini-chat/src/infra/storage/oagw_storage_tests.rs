#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slices 2 to 5: the exact requests of the "Storage wire contract" of the
//! FEATURE (`cpt-cf-mini-chat-algo-attachments-build-storage-request`,
//! `-upload-provider-file`, `-upload-secondary-copy`) and the classification of
//! the answers, over the fake OAGW.

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{OagwStorageClient, RESPONSE_BODY_LIMIT};
use crate::attachment_test_env::{ParsedPart, parts_of};
use crate::config::StorageKind;
use crate::domain::service::attachments::model::StorageTarget;
use crate::domain::service::attachments::ports::{
    FileUpload, IndexStatus, StorageClient, StorageError,
};
use crate::test_doubles::{FakeOagw, Fault, ProxyReply, RecordedProxy, USER, user_ctx};

const CHAT: Uuid = Uuid::from_u128(0xC1);
const ATT: Uuid = Uuid::from_u128(0xA1);

fn openai() -> StorageTarget {
    StorageTarget {
        provider_id: "openai".to_owned(),
        kind: StorageKind::Openai,
        backend: "openai".to_owned(),
        alias: "api.openai.test".to_owned(),
        api_version: None,
    }
}

fn azure() -> StorageTarget {
    StorageTarget {
        provider_id: "azure".to_owned(),
        kind: StorageKind::Azure,
        backend: "azure".to_owned(),
        alias: "az.test".to_owned(),
        api_version: Some("2025-01-01-preview".to_owned()),
    }
}

fn pdf(bytes: &[u8]) -> FileUpload {
    FileUpload {
        chat_id: CHAT,
        attachment_id: ATT,
        content_type: "application/pdf".to_owned(),
        extension: "pdf".to_owned(),
        bytes: Bytes::copy_from_slice(bytes),
    }
}

fn setup() -> (Arc<FakeOagw>, OagwStorageClient) {
    let oagw = FakeOagw::new();
    let client = OagwStorageClient::new(oagw.clone());
    (oagw, client)
}

fn only(oagw: &FakeOagw) -> RecordedProxy {
    let calls = oagw.proxied();
    assert_eq!(calls.len(), 1, "exactly one proxied call: {calls:?}");
    calls.into_iter().next().unwrap()
}

fn header_names(call: &RecordedProxy) -> Vec<String> {
    let mut names: Vec<String> = call
        .headers
        .iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    names.sort();
    names
}

fn assert_no_credentials(call: &RecordedProxy) {
    for name in header_names(call) {
        for forbidden in [
            "authorization",
            "api-key",
            "x-api-key",
            "cookie",
            "openai-organization",
        ] {
            assert!(
                !name.contains(forbidden),
                "the client never sets a credential header: {name}"
            );
        }
    }
}

fn json_body(call: &RecordedProxy) -> Value {
    call.json()
}

// ================================================================================================
// File upload
// ================================================================================================

#[tokio::test]
async fn the_file_is_posted_as_multipart_with_purpose_then_file() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-abc" })));

    let id = client
        .upload_file(&user_ctx(), &openai(), &pdf(b"%PDF-1.7 body"))
        .await
        .unwrap();

    assert_eq!(id, "file-abc");
    let call = only(&oagw);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, "/api.openai.test/v1/files");
    assert!(
        call.header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary="),
        "{:?}",
        call.header("content-type")
    );
    let parts = parts_of(&call);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].name, "purpose");
    assert_eq!(parts[0].body, b"assistants");
    assert_eq!(parts[0].filename, None);
    assert_eq!(parts[1].name, "file");
    assert_eq!(
        parts[1].filename.as_deref(),
        Some(format!("{CHAT}_{ATT}.pdf").as_str())
    );
    assert_eq!(parts[1].content_type.as_deref(), Some("application/pdf"));
    assert_eq!(parts[1].body, b"%PDF-1.7 body");
}

#[tokio::test]
async fn the_file_bytes_survive_binary_content_and_line_breaks() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-1" })));
    let mut bytes: Vec<u8> = (0..=255_u8).cycle().take(4000).collect();
    bytes.extend_from_slice(b"\r\n--boundary-like\r\n\r\n");

    client
        .upload_file(&user_ctx(), &openai(), &pdf(&bytes))
        .await
        .unwrap();

    assert_eq!(parts_of(&only(&oagw))[1].body, bytes);
}

#[tokio::test]
async fn an_empty_file_is_uploaded_like_any_other() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-1" })));

    client
        .upload_file(&user_ctx(), &openai(), &pdf(b""))
        .await
        .unwrap();

    assert!(parts_of(&only(&oagw))[1].body.is_empty());
}

#[tokio::test]
async fn the_provider_extension_and_type_of_the_row_name_the_file() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-1" })));
    let csv = FileUpload {
        content_type: "text/plain".to_owned(),
        extension: "txt".to_owned(),
        ..pdf(b"a,b")
    };

    client
        .upload_file(&user_ctx(), &openai(), &csv)
        .await
        .unwrap();

    let parts: Vec<ParsedPart> = parts_of(&only(&oagw));
    assert_eq!(
        parts[1].filename.as_deref(),
        Some(format!("{CHAT}_{ATT}.txt").as_str())
    );
    assert_eq!(parts[1].content_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn an_azure_upload_uses_the_openai_prefix_and_the_api_version() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-az" })));

    let id = client
        .upload_file(&user_ctx(), &azure(), &pdf(b"x"))
        .await
        .unwrap();

    assert_eq!(id, "file-az");
    assert_eq!(
        only(&oagw).uri,
        "/az.test/openai/files?api-version=2025-01-01-preview"
    );
}

#[tokio::test]
async fn the_call_uses_the_given_security_context_and_no_credential_header() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-1" })));

    client
        .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
        .await
        .unwrap();

    let call = only(&oagw);
    assert_eq!(call.subject, USER);
    assert_no_credentials(&call);
}

#[tokio::test]
async fn a_failed_upload_is_classified_by_its_answer() {
    for (status, expected) in [
        (500_u16, StorageError::Status(500)),
        (502, StorageError::Status(502)),
        (429, StorageError::Status(429)),
        (400, StorageError::Status(400)),
        (401, StorageError::Status(401)),
    ] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(
            status,
            &json!({ "error": { "message": "secret provider text" } }),
        ));

        let err = client
            .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
            .await
            .unwrap_err();

        assert_eq!(err, expected, "{status}");
    }
}

#[tokio::test]
async fn a_success_without_a_usable_id_is_malformed() {
    for body in [
        json!({}),
        json!({ "id": "" }),
        json!({ "id": null }),
        json!({ "id": 5 }),
        json!({ "id": ["file-1"] }),
        json!([]),
    ] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(200, &body));

        let err = client
            .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
            .await
            .unwrap_err();

        assert_eq!(err, StorageError::Malformed, "{body}");
    }
}

#[tokio::test]
async fn a_success_that_is_not_json_is_malformed() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
        body: crate::test_doubles::ReplyBody::Bytes(b"<html>ok</html>".to_vec()),
        source: None,
    });

    let err = client
        .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
        .await
        .unwrap_err();

    assert_eq!(err, StorageError::Malformed);
}

#[tokio::test]
async fn a_failure_before_a_response_is_a_transport_error() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-ok" })));
    assert_eq!(
        client
            .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
            .await
            .unwrap(),
        "file-ok",
        "an answered call succeeds"
    );
    for fault in [Fault::Internal, Fault::DeadlineExceeded] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::Error(fault));

        let err = client
            .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
            .await
            .unwrap_err();

        assert_eq!(err, StorageError::Transport, "{fault:?}");
    }
}

#[tokio::test]
async fn a_response_body_is_read_up_to_one_mebibyte_only() {
    assert_eq!(RESPONSE_BODY_LIMIT, 1024 * 1024);
    let (oagw, client) = setup();
    let padded = json!({ "id": "file-1", "pad": "x".repeat(2 * 1024 * 1024) });
    oagw.push_proxy(ProxyReply::json(200, &padded));

    let err = client
        .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
        .await
        .unwrap_err();

    assert_eq!(
        err,
        StorageError::Malformed,
        "the body is cut at 1 MiB, which leaves no JSON document"
    );
}

// ================================================================================================
// Classification
// ================================================================================================

#[test]
fn provider_5xx_and_transport_failures_are_transient() {
    for error in [
        StorageError::Status(500),
        StorageError::Status(502),
        StorageError::Status(503),
        StorageError::Status(504),
        StorageError::Status(599),
        StorageError::Transport,
    ] {
        assert!(error.is_transient(), "{error:?}");
    }
}

#[test]
fn every_other_error_is_not_transient() {
    assert!(StorageError::Status(500).is_transient(), "the contrast");
    for error in [
        StorageError::Status(400),
        StorageError::Status(401),
        StorageError::Status(403),
        StorageError::Status(404),
        StorageError::Status(429),
        StorageError::Malformed,
    ] {
        assert!(!error.is_transient(), "{error:?}");
    }
}

#[test]
fn the_cause_is_short_and_carries_no_provider_text() {
    assert_eq!(StorageError::Status(500).cause(), "http 500");
    assert_eq!(StorageError::Status(404).cause(), "http 404");
    assert_eq!(StorageError::Transport.cause(), "gateway error");
    assert_eq!(StorageError::Malformed.cause(), "malformed response");
}

// ================================================================================================
// Vector stores
// ================================================================================================

async fn status_of(wire: Value) -> Result<IndexStatus, StorageError> {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &wire));
    client
        .indexing_status(&user_ctx(), &openai(), "vs_1", "file-1")
        .await
}

#[tokio::test]
async fn the_vector_store_is_created_with_the_chat_name() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "vs_42" })));

    let id = client
        .create_vector_store(&user_ctx(), &openai(), CHAT)
        .await
        .unwrap();

    assert_eq!(id, "vs_42");
    let call = only(&oagw);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, "/api.openai.test/v1/vector_stores");
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("accept"), Some("application/json"));
    assert_eq!(
        json_body(&call),
        json!({ "name": format!("mini-chat-{CHAT}") })
    );
    assert_no_credentials(&call);
}

#[tokio::test]
async fn an_azure_vector_store_uses_the_openai_prefix_and_the_api_version() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "vs_az" })));

    client
        .create_vector_store(&user_ctx(), &azure(), CHAT)
        .await
        .unwrap();

    assert_eq!(
        only(&oagw).uri,
        "/az.test/openai/vector_stores?api-version=2025-01-01-preview"
    );
}

#[tokio::test]
async fn a_failed_store_creation_is_classified() {
    for (status, body, expected) in [
        (500_u16, json!({}), StorageError::Status(500)),
        (400, json!({}), StorageError::Status(400)),
        (200, json!({}), StorageError::Malformed),
        (200, json!({ "id": "" }), StorageError::Malformed),
    ] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(status, &body));

        let err = client
            .create_vector_store(&user_ctx(), &openai(), CHAT)
            .await
            .unwrap_err();

        assert_eq!(err, expected, "{status} {body}");
    }
}

#[tokio::test]
async fn the_file_is_added_with_only_the_attachment_id_attribute() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "status": "in_progress" })));

    let status = client
        .add_file(&user_ctx(), &openai(), "vs_1", "file-9", ATT)
        .await
        .unwrap();

    assert_eq!(status, IndexStatus::InProgress);
    let call = only(&oagw);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, "/api.openai.test/v1/vector_stores/vs_1/files");
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("accept"), Some("application/json"));
    assert_eq!(
        json_body(&call),
        json!({ "file_id": "file-9", "attributes": { "attachment_id": ATT.to_string() } })
    );
    assert_no_credentials(&call);
}

#[tokio::test]
async fn an_azure_add_file_uses_the_openai_prefix_and_the_api_version() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "status": "completed" })));

    client
        .add_file(&user_ctx(), &azure(), "vs_1", "file-9", ATT)
        .await
        .unwrap();

    assert_eq!(
        only(&oagw).uri,
        "/az.test/openai/vector_stores/vs_1/files?api-version=2025-01-01-preview"
    );
}

#[tokio::test]
async fn the_status_of_an_add_file_answer_is_mapped() {
    for (wire, expected) in [
        (json!({ "status": "completed" }), IndexStatus::Completed),
        (json!({ "status": "in_progress" }), IndexStatus::InProgress),
        (json!({}), IndexStatus::InProgress),
        (json!({ "id": "file-9" }), IndexStatus::InProgress),
        (json!({ "status": "failed" }), IndexStatus::Failed),
        (json!({ "status": "cancelled" }), IndexStatus::Failed),
        (json!({ "status": "something_new" }), IndexStatus::Failed),
    ] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(200, &wire));

        let got = client
            .add_file(&user_ctx(), &openai(), "vs_1", "file-9", ATT)
            .await
            .unwrap();

        assert_eq!(got, expected, "{wire}");
    }
}

#[tokio::test]
async fn an_add_file_answer_that_is_not_json_or_not_2xx_is_an_error() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::Response {
        status: 200,
        headers: vec![],
        body: crate::test_doubles::ReplyBody::Bytes(b"not json".to_vec()),
        source: None,
    });
    let err = client
        .add_file(&user_ctx(), &openai(), "vs_1", "file-9", ATT)
        .await
        .unwrap_err();
    assert_eq!(err, StorageError::Malformed);

    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(500, &json!({})));
    let err = client
        .add_file(&user_ctx(), &openai(), "vs_1", "file-9", ATT)
        .await
        .unwrap_err();
    assert_eq!(err, StorageError::Status(500));
}

#[tokio::test]
async fn the_indexing_status_is_read_with_a_get_without_a_body() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "status": "completed" })));

    let status = client
        .indexing_status(&user_ctx(), &openai(), "vs_1", "file-9")
        .await
        .unwrap();

    assert_eq!(status, IndexStatus::Completed);
    let call = only(&oagw);
    assert_eq!(call.method, "GET");
    assert_eq!(
        call.uri,
        "/api.openai.test/v1/vector_stores/vs_1/files/file-9"
    );
    assert!(call.body.is_empty());
    assert_eq!(call.header("accept"), Some("application/json"));
    assert_no_credentials(&call);
}

#[tokio::test]
async fn an_azure_status_read_carries_the_api_version() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "status": "completed" })));

    client
        .indexing_status(&user_ctx(), &azure(), "vs_1", "file-9")
        .await
        .unwrap();

    assert_eq!(
        only(&oagw).uri,
        "/az.test/openai/vector_stores/vs_1/files/file-9?api-version=2025-01-01-preview"
    );
}

#[tokio::test]
async fn the_status_values_of_a_status_read_are_mapped() {
    for (wire, expected) in [
        (json!({ "status": "completed" }), IndexStatus::Completed),
        (json!({ "status": "in_progress" }), IndexStatus::InProgress),
        (json!({}), IndexStatus::InProgress),
        (json!({ "status": "failed" }), IndexStatus::Failed),
        (json!({ "status": "cancelled" }), IndexStatus::Failed),
        (json!({ "status": "wat" }), IndexStatus::Failed),
    ] {
        assert_eq!(status_of(wire.clone()).await, Ok(expected), "{wire}");
    }
}

#[tokio::test]
async fn a_failed_status_read_is_classified_for_the_transient_rules() {
    for (status, expected, transient) in [
        (500_u16, StorageError::Status(500), true),
        (502, StorageError::Status(502), true),
        (503, StorageError::Status(503), true),
        (504, StorageError::Status(504), true),
        (429, StorageError::Status(429), false),
        (404, StorageError::Status(404), false),
    ] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(status, &json!({})));

        let err = client
            .indexing_status(&user_ctx(), &openai(), "vs_1", "file-9")
            .await
            .unwrap_err();

        assert_eq!(err, expected, "{status}");
        assert_eq!(err.is_transient(), transient, "{status}");
    }
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::Error(Fault::DeadlineExceeded));
    let err = client
        .indexing_status(&user_ctx(), &openai(), "vs_1", "file-9")
        .await
        .unwrap_err();
    assert!(
        err.is_transient(),
        "a gateway failure keeps polling: {err:?}"
    );
}

#[tokio::test]
async fn provider_ids_are_single_percent_encoded_path_segments() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "status": "completed" })));
    oagw.push_proxy(ProxyReply::json(200, &json!({})));
    oagw.push_proxy(ProxyReply::json(200, &json!({})));

    client
        .indexing_status(&user_ctx(), &openai(), "vs/../x y", "a?b#c/d")
        .await
        .unwrap();
    client
        .delete_file(&user_ctx(), &openai(), "f/../g h")
        .await
        .unwrap();
    client
        .delete_vector_store(&user_ctx(), &openai(), "v?s#1")
        .await
        .unwrap();

    let calls = oagw.proxied();
    assert_eq!(
        calls[0].uri,
        "/api.openai.test/v1/vector_stores/vs%2F..%2Fx%20y/files/a%3Fb%23c%2Fd"
    );
    assert_eq!(calls[1].uri, "/api.openai.test/v1/files/f%2F..%2Fg%20h");
    assert_eq!(calls[2].uri, "/api.openai.test/v1/vector_stores/v%3Fs%231");
}

// ================================================================================================
// Deletes
// ================================================================================================

#[tokio::test]
async fn a_file_is_deleted_with_a_delete_request() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "deleted": true })));

    client
        .delete_file(&user_ctx(), &openai(), "file-7")
        .await
        .unwrap();

    let call = only(&oagw);
    assert_eq!(call.method, "DELETE");
    assert_eq!(call.uri, "/api.openai.test/v1/files/file-7");
    assert!(call.body.is_empty());
    assert_no_credentials(&call);
}

#[tokio::test]
async fn an_azure_file_delete_carries_the_api_version() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({})));

    client
        .delete_file(&user_ctx(), &azure(), "file-7")
        .await
        .unwrap();

    assert_eq!(
        only(&oagw).uri,
        "/az.test/openai/files/file-7?api-version=2025-01-01-preview"
    );
}

#[tokio::test]
async fn a_delete_succeeds_on_2xx_and_on_404() {
    for status in [200_u16, 202, 204, 404] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(status, &json!({})));

        client
            .delete_file(&user_ctx(), &openai(), "file-7")
            .await
            .unwrap_or_else(|e| panic!("{status}: {e:?}"));
    }
}

#[tokio::test]
async fn a_delete_fails_on_every_other_status_and_on_a_transport_failure() {
    for status in [400_u16, 401, 403, 409, 429, 500, 502, 503] {
        let (oagw, client) = setup();
        oagw.push_proxy(ProxyReply::json(status, &json!({})));

        let err = client
            .delete_file(&user_ctx(), &openai(), "file-7")
            .await
            .unwrap_err();

        assert_eq!(err, StorageError::Status(status), "{status}");
    }
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::Error(Fault::Internal));
    let err = client
        .delete_file(&user_ctx(), &openai(), "file-7")
        .await
        .unwrap_err();
    assert_eq!(err, StorageError::Transport);
}

#[tokio::test]
async fn an_orphan_vector_store_is_deleted_by_id() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({})));
    oagw.push_proxy(ProxyReply::json(404, &json!({})));
    oagw.push_proxy(ProxyReply::json(500, &json!({})));

    client
        .delete_vector_store(&user_ctx(), &openai(), "vs_9")
        .await
        .unwrap();
    client
        .delete_vector_store(&user_ctx(), &azure(), "vs_9")
        .await
        .unwrap();
    let err = client
        .delete_vector_store(&user_ctx(), &openai(), "vs_9")
        .await
        .unwrap_err();

    let calls = oagw.proxied();
    assert_eq!(calls[0].method, "DELETE");
    assert_eq!(calls[0].uri, "/api.openai.test/v1/vector_stores/vs_9");
    assert_eq!(
        calls[1].uri,
        "/az.test/openai/vector_stores/vs_9?api-version=2025-01-01-preview"
    );
    assert_eq!(err, StorageError::Status(500));
}

// ================================================================================================
// Anthropic secondary copy
// ================================================================================================

#[tokio::test]
async fn the_secondary_copy_posts_only_the_file_part_with_the_anthropic_headers() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file_ant_1" })));
    let png = FileUpload {
        content_type: "image/png".to_owned(),
        extension: "png".to_owned(),
        ..pdf(b"\x89PNG bytes")
    };

    let id = client
        .upload_secondary(&user_ctx(), "api.anthropic.test", &png)
        .await
        .unwrap();

    assert_eq!(id, "file_ant_1");
    let call = only(&oagw);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, "/api.anthropic.test/v1/files");
    assert_eq!(call.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(call.header("anthropic-beta"), Some("files-api-2025-04-14"));
    assert!(
        call.header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
    let parts = parts_of(&call);
    assert_eq!(parts.len(), 1, "no purpose part");
    assert_eq!(parts[0].name, "file");
    assert_eq!(
        parts[0].filename.as_deref(),
        Some(format!("{CHAT}_{ATT}.png").as_str())
    );
    assert_eq!(parts[0].content_type.as_deref(), Some("image/png"));
    assert_eq!(parts[0].body, b"\x89PNG bytes");
    assert_no_credentials(&call);
}

#[tokio::test]
async fn a_failed_secondary_upload_is_classified() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(500, &json!({})));
    oagw.push_proxy(ProxyReply::json(200, &json!({})));
    oagw.push_proxy(ProxyReply::Error(Fault::DeadlineExceeded));

    let a = client
        .upload_secondary(&user_ctx(), "api.anthropic.test", &pdf(b"x"))
        .await;
    let b = client
        .upload_secondary(&user_ctx(), "api.anthropic.test", &pdf(b"x"))
        .await;
    let c = client
        .upload_secondary(&user_ctx(), "api.anthropic.test", &pdf(b"x"))
        .await;

    assert_eq!(a, Err(StorageError::Status(500)));
    assert_eq!(b, Err(StorageError::Malformed));
    assert_eq!(c, Err(StorageError::Transport));
}

#[tokio::test]
async fn the_secondary_copy_is_deleted_with_the_anthropic_headers() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({})));
    oagw.push_proxy(ProxyReply::json(404, &json!({})));
    oagw.push_proxy(ProxyReply::json(500, &json!({})));

    client
        .delete_secondary(&user_ctx(), "api.anthropic.test", "file_ant_1")
        .await
        .unwrap();
    client
        .delete_secondary(&user_ctx(), "api.anthropic.test", "file_ant_1")
        .await
        .unwrap();
    let err = client
        .delete_secondary(&user_ctx(), "api.anthropic.test", "file_ant_1")
        .await
        .unwrap_err();

    let calls = oagw.proxied();
    for call in &calls {
        assert_eq!(call.method, "DELETE");
        assert_eq!(call.uri, "/api.anthropic.test/v1/files/file_ant_1");
        assert_eq!(call.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(call.header("anthropic-beta"), Some("files-api-2025-04-14"));
        assert_no_credentials(call);
    }
    assert_eq!(err, StorageError::Status(500));
}

#[tokio::test]
async fn the_openai_calls_never_carry_the_anthropic_headers() {
    let (oagw, client) = setup();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "file-1" })));
    oagw.push_proxy(ProxyReply::json(200, &json!({})));

    client
        .upload_file(&user_ctx(), &openai(), &pdf(b"x"))
        .await
        .unwrap();
    client
        .delete_file(&user_ctx(), &openai(), "file-1")
        .await
        .unwrap();

    for call in oagw.proxied() {
        assert!(call.header("anthropic-version").is_none());
        assert!(call.header("anthropic-beta").is_none());
    }
}
