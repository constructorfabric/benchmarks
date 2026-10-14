//! The `OAGW` storage adapter against the mock gateway: the URI built from the alias, the prefix
//! and the path for the `OpenAI` (`/v1`) and Azure (`/openai` with `api-version`) kinds, the multipart upload with
//! `purpose` `assistants` and the provider-side filename, the streamed body with its byte-counter
//! abort, the vector store calls with the single `attachment_id` attribute, the transient versus
//! other read failures, the Anthropic secondary upload without `purpose`, the S2S security
//! context and the absence of any credential header or idempotency key.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload`,
//! `-vector-store-provisioning`, `-indexing-wait` and `-secondary-copy`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload`,
//! `-vector-store`, `-indexing-lifecycle` and `-images`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use serde_json::json;
use uuid::Uuid;

use super::OagwStorageClient;
use crate::config::StorageKind;
use crate::domain::attachments::provider::{
    FileUploadMeta, ProviderFailure, StorageProvider, StorageTarget, UploadBody, UploadBodyError,
    UploadError,
};
use crate::infra::provider_task::S2sContext;
use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, AZURE_API_VERSION, AZURE_BACKEND, AZURE_RAG_ALIAS, OPENAI_ALIAS,
    OPENAI_BACKEND,
};
use crate::test_support::provider::GatewayFailure;
use crate::test_support::send::provider_s2s;
use crate::test_support::storage_mock::{Answer, MockStorage, Recorded, Route};

fn client(mock: &Arc<MockStorage>) -> OagwStorageClient {
    OagwStorageClient::new(mock.clone(), S2sContext::bound(provider_s2s()))
}

fn openai() -> StorageTarget {
    StorageTarget {
        provider_id: "azure_openai".to_owned(),
        upstream_alias: OPENAI_ALIAS.to_owned(),
        storage_kind: StorageKind::Openai,
        api_version: None,
        storage_backend: OPENAI_BACKEND.to_owned(),
    }
}

fn azure() -> StorageTarget {
    StorageTarget {
        provider_id: "azure_rag".to_owned(),
        upstream_alias: AZURE_RAG_ALIAS.to_owned(),
        storage_kind: StorageKind::Azure,
        api_version: Some(AZURE_API_VERSION.to_owned()),
        storage_backend: AZURE_BACKEND.to_owned(),
    }
}

fn anthropic() -> StorageTarget {
    StorageTarget {
        provider_id: "anthropic".to_owned(),
        upstream_alias: ANTHROPIC_ALIAS.to_owned(),
        storage_kind: StorageKind::Openai,
        api_version: None,
        storage_backend: "anthropic".to_owned(),
    }
}

fn meta() -> FileUploadMeta {
    FileUploadMeta {
        provider_filename: "chat_att.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
    }
}

fn body(chunks: &[&[u8]]) -> UploadBody {
    let items: Vec<Result<Bytes, UploadBodyError>> = chunks
        .iter()
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    Box::pin(stream::iter(items))
}

fn only(mock: &MockStorage, route: Route) -> Recorded {
    let calls = mock.calls_of(route);
    assert_eq!(
        calls.len(),
        1,
        "expected one {route:?} call, got {}",
        mock.call_count()
    );
    calls.into_iter().next().unwrap()
}

// ── files upload ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_upload_posts_a_multipart_to_the_openai_files_route() {
    // inst-pfu-build, inst-pfu-post: purpose assistants, file part with the provider filename
    let mock = MockStorage::new();
    let id = client(&mock)
        .upload_file(&openai(), &meta(), body(&[b"hello ", b"world"]))
        .await
        .unwrap();
    assert!(!id.is_empty());
    let call = only(&mock, Route::FilesUpload);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, format!("/{OPENAI_ALIAS}/v1/files"));
    assert!(
        call.header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
    assert_eq!(call.text_part("purpose").as_deref(), Some("assistants"));
    let file = call.part("file").expect("file part");
    assert_eq!(file.filename.as_deref(), Some("chat_att.pdf"));
    assert_eq!(file.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(&file.data[..], b"hello world");
    assert_eq!(call.parts.len(), 2, "only purpose and file");
}

#[tokio::test]
async fn the_upload_returns_the_provider_file_id_of_the_response() {
    let mock = MockStorage::new();
    mock.queue(
        Route::FilesUpload,
        Answer::ok(&json!({ "id": "file-XYZ", "object": "file" })),
    );
    let id = client(&mock)
        .upload_file(&openai(), &meta(), body(&[b"x"]))
        .await
        .unwrap();
    assert_eq!(id, "file-XYZ");
}

#[tokio::test]
async fn the_azure_upload_uses_the_openai_prefix_with_the_api_version() {
    let mock = MockStorage::new();
    client(&mock)
        .upload_file(&azure(), &meta(), body(&[b"x"]))
        .await
        .unwrap();
    let call = only(&mock, Route::FilesUpload);
    assert_eq!(call.alias, AZURE_RAG_ALIAS);
    assert_eq!(call.path, "/openai/files");
    assert_eq!(
        call.query.as_deref(),
        Some(&format!("api-version={AZURE_API_VERSION}")[..])
    );
}

#[tokio::test]
async fn the_upload_runs_under_the_s2s_context_without_a_credential_or_idempotency_key() {
    // constraint no-credentials; the call carries no idempotency key
    let mock = MockStorage::new();
    client(&mock)
        .upload_file(&openai(), &meta(), body(&[b"x"]))
        .await
        .unwrap();
    let call = only(&mock, Route::FilesUpload);
    let s2s = provider_s2s();
    assert_eq!(call.subject, s2s.subject_id());
    assert_eq!(call.tenant, s2s.subject_tenant_id());
    for name in [
        "authorization",
        "api-key",
        "x-api-key",
        "idempotency-key",
        "x-idempotency-key",
    ] {
        assert!(call.header(name).is_none(), "`{name}` header on the upload");
    }
}

#[tokio::test]
async fn a_failing_body_stream_aborts_the_provider_request() {
    // inst-pfu-too-large: drop the provider request at once
    let mock = MockStorage::new();
    let items: Vec<Result<Bytes, UploadBodyError>> = vec![
        Ok(Bytes::from_static(b"abc")),
        Err(UploadBodyError::LimitExceeded),
        Ok(Bytes::from_static(b"never")),
    ];
    let result = client(&mock)
        .upload_file(&openai(), &meta(), Box::pin(stream::iter(items)))
        .await;
    assert_eq!(
        result,
        Err(UploadError::Body(UploadBodyError::LimitExceeded))
    );
    assert_eq!(
        mock.completed_uploads(),
        0,
        "a truncated upload reached the provider whole"
    );
}

#[tokio::test]
async fn a_failed_provider_answer_is_an_upload_failure_and_is_not_retried() {
    // inst-pfu-failed
    for answer in [
        Answer::status(500),
        Answer::status(400),
        Answer::status(429),
        Answer::Fail(GatewayFailure::Unavailable),
        Answer::Fail(GatewayFailure::Timeout),
    ] {
        let mock = MockStorage::new();
        mock.set_default(Route::FilesUpload, answer);
        let result = client(&mock)
            .upload_file(&openai(), &meta(), body(&[b"x"]))
            .await;
        assert_eq!(result, Err(UploadError::Provider));
        assert_eq!(
            mock.count_of(Route::FilesUpload),
            1,
            "the upload was retried"
        );
    }
}

#[tokio::test]
async fn a_success_without_a_file_id_is_an_upload_failure() {
    let mock = MockStorage::new();
    mock.queue(Route::FilesUpload, Answer::ok(&json!({ "object": "file" })));
    let result = client(&mock)
        .upload_file(&openai(), &meta(), body(&[b"x"]))
        .await;
    assert_eq!(result, Err(UploadError::Provider));
}

#[tokio::test]
async fn a_large_streamed_body_arrives_whole_in_the_file_part() {
    let mock = MockStorage::new();
    let chunk = vec![7u8; 64 * 1024];
    let chunks: Vec<&[u8]> = (0..16).map(|_| &chunk[..]).collect();
    client(&mock)
        .upload_file(&openai(), &meta(), body(&chunks))
        .await
        .unwrap();
    let call = only(&mock, Route::FilesUpload);
    assert_eq!(call.part("file").unwrap().data.len(), 16 * 64 * 1024);
}

// ── files delete ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_file_delete_is_a_delete_of_the_files_route() {
    // inst-uf-delete-file
    let mock = MockStorage::new();
    client(&mock)
        .delete_file(&openai(), "file-abc")
        .await
        .unwrap();
    let call = only(&mock, Route::FilesDelete);
    assert_eq!(call.method, "DELETE");
    assert_eq!(call.uri, format!("/{OPENAI_ALIAS}/v1/files/file-abc"));
    let azure_mock = MockStorage::new();
    client(&azure_mock)
        .delete_file(&azure(), "file-abc")
        .await
        .unwrap();
    let call = only(&azure_mock, Route::FilesDelete);
    assert_eq!(call.path, "/openai/files/file-abc");
    assert_eq!(
        call.query.as_deref(),
        Some(&format!("api-version={AZURE_API_VERSION}")[..])
    );
}

#[tokio::test]
async fn a_failed_file_delete_is_an_error_and_is_not_retried() {
    let mock = MockStorage::new();
    mock.set_default(Route::FilesDelete, Answer::status(500));
    assert!(
        client(&mock)
            .delete_file(&openai(), "file-abc")
            .await
            .is_err()
    );
    assert_eq!(mock.count_of(Route::FilesDelete), 1);
}

// ── vector stores ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_vector_store_is_created_with_a_post_and_its_id_returned() {
    // inst-vs-create
    let mock = MockStorage::new();
    mock.queue(
        Route::VsCreate,
        Answer::ok(&json!({ "id": "vs_777", "object": "vector_store" })),
    );
    let id = client(&mock).create_vector_store(&openai()).await.unwrap();
    assert_eq!(id, "vs_777");
    let call = only(&mock, Route::VsCreate);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, format!("/{OPENAI_ALIAS}/v1/vector_stores"));
}

#[tokio::test]
async fn the_azure_vector_store_routes_carry_the_api_version() {
    let mock = MockStorage::new();
    client(&mock).create_vector_store(&azure()).await.unwrap();
    let call = only(&mock, Route::VsCreate);
    assert_eq!(call.path, "/openai/vector_stores");
    assert_eq!(
        call.query.as_deref(),
        Some(&format!("api-version={AZURE_API_VERSION}")[..])
    );
}

#[tokio::test]
async fn a_failed_vector_store_create_is_an_error_and_is_not_retried() {
    for answer in [
        Answer::status(500),
        Answer::status(403),
        Answer::Fail(GatewayFailure::Unavailable),
    ] {
        let mock = MockStorage::new();
        mock.set_default(Route::VsCreate, answer);
        assert!(client(&mock).create_vector_store(&openai()).await.is_err());
        assert_eq!(mock.count_of(Route::VsCreate), 1);
    }
}

#[tokio::test]
async fn a_vector_store_delete_is_a_delete_of_the_store() {
    // inst-vs-cas-lost: best-effort delete of the orphaned store
    let mock = MockStorage::new();
    client(&mock)
        .delete_vector_store(&openai(), "vs_1")
        .await
        .unwrap();
    let call = only(&mock, Route::VsDelete);
    assert_eq!(call.method, "DELETE");
    assert_eq!(call.uri, format!("/{OPENAI_ALIAS}/v1/vector_stores/vs_1"));
}

#[tokio::test]
async fn a_file_is_added_with_only_the_attachment_id_attribute() {
    // inst-iw-add: the provider file id and exactly one attribute
    let mock = MockStorage::new();
    mock.queue(Route::VsAddFile, Answer::indexing("in_progress"));
    let attachment_id = Uuid::new_v4();
    let status = client(&mock)
        .add_vector_store_file(&openai(), "vs_1", "file-9", attachment_id)
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("in_progress"));
    let call = only(&mock, Route::VsAddFile);
    assert_eq!(call.method, "POST");
    assert_eq!(
        call.uri,
        format!("/{OPENAI_ALIAS}/v1/vector_stores/vs_1/files")
    );
    assert_eq!(
        call.json(),
        json!({ "file_id": "file-9", "attributes": { "attachment_id": attachment_id.to_string() } })
    );
}

#[tokio::test]
async fn an_add_response_without_a_status_is_none() {
    let mock = MockStorage::new();
    mock.queue(Route::VsAddFile, Answer::no_status());
    let status = client(&mock)
        .add_vector_store_file(&openai(), "vs_1", "file-9", Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(status, None);
}

#[tokio::test]
async fn a_failed_add_is_an_error_and_is_not_retried() {
    for answer in [
        Answer::status(500),
        Answer::status(400),
        Answer::Fail(GatewayFailure::Internal),
    ] {
        let mock = MockStorage::new();
        mock.set_default(Route::VsAddFile, answer);
        let result = client(&mock)
            .add_vector_store_file(&openai(), "vs_1", "file-9", Uuid::new_v4())
            .await;
        assert!(result.is_err());
        assert_eq!(mock.count_of(Route::VsAddFile), 1);
    }
}

// ── indexing status reads ────────────────────────────────────────────────────

#[tokio::test]
async fn a_read_gets_the_vector_store_file_and_returns_its_status_text() {
    // inst-iw-read
    let mock = MockStorage::new();
    for status in ["in_progress", "completed", "failed", "cancelled"] {
        mock.queue(Route::VsReadFile, Answer::indexing(status));
    }
    for status in ["in_progress", "completed", "failed", "cancelled"] {
        let read = client(&mock)
            .read_vector_store_file(&openai(), "vs_1", "file-9", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.as_deref(), Some(status));
    }
    let calls = mock.calls_of(Route::VsReadFile);
    assert_eq!(calls[0].method, "GET");
    assert_eq!(
        calls[0].uri,
        format!("/{OPENAI_ALIAS}/v1/vector_stores/vs_1/files/file-9")
    );
}

#[tokio::test]
async fn a_read_without_a_status_is_none() {
    let mock = MockStorage::new();
    mock.queue(Route::VsReadFile, Answer::no_status());
    let read = client(&mock)
        .read_vector_store_file(&openai(), "vs_1", "file-9", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(read, None);
}

#[tokio::test]
async fn provider_5xx_and_gateway_failures_are_transient_read_errors() {
    // inst-iw-transient
    for answer in [
        Answer::status(500),
        Answer::status(502),
        Answer::status(503),
        Answer::Fail(GatewayFailure::Unavailable),
        Answer::Fail(GatewayFailure::Timeout),
        Answer::Fail(GatewayFailure::Internal),
    ] {
        let mock = MockStorage::new();
        mock.set_default(Route::VsReadFile, answer);
        let read = client(&mock)
            .read_vector_store_file(&openai(), "vs_1", "file-9", Duration::from_secs(5))
            .await;
        assert_eq!(read, Err(ProviderFailure::Transient));
    }
}

#[tokio::test]
async fn other_client_errors_are_non_transient_read_errors() {
    // inst-iw-read-failed
    for status in [400, 401, 403, 404, 422] {
        let mock = MockStorage::new();
        mock.set_default(Route::VsReadFile, Answer::status(status));
        let read = client(&mock)
            .read_vector_store_file(&openai(), "vs_1", "file-9", Duration::from_secs(5))
            .await;
        assert_eq!(read, Err(ProviderFailure::Other), "{status}");
    }
}

#[tokio::test]
async fn a_read_that_exceeds_its_timeout_is_a_transient_error() {
    // inst-iw-read: "or the read timed out"
    let mock = MockStorage::new();
    mock.set_default(Route::VsReadFile, Answer::Pending);
    let read = client(&mock)
        .read_vector_store_file(&openai(), "vs_1", "file-9", Duration::from_millis(50))
        .await;
    assert_eq!(read, Err(ProviderFailure::Transient));
}

// ── Anthropic secondary copy ─────────────────────────────────────────────────

#[tokio::test]
async fn the_secondary_upload_sends_only_the_file_part_without_a_purpose() {
    // inst-sec-upload: only the file part and no purpose, on the Anthropic upstream alias
    let mock = MockStorage::new();
    mock.queue_for(
        ANTHROPIC_ALIAS,
        Route::FilesUpload,
        Answer::ok(&json!({ "id": "file_ant_5", "type": "file" })),
    );
    let id = client(&mock)
        .upload_secondary_image(
            &anthropic(),
            "cat.png",
            "image/png",
            Bytes::from_static(b"PNGDATA"),
        )
        .await
        .unwrap();
    assert_eq!(id, "file_ant_5");
    let call = only(&mock, Route::FilesUpload);
    assert_eq!(call.alias, ANTHROPIC_ALIAS);
    assert_eq!(call.parts.len(), 1, "only the file part");
    assert!(call.part("purpose").is_none());
    let file = call.part("file").unwrap();
    assert_eq!(file.filename.as_deref(), Some("cat.png"));
    assert_eq!(file.content_type.as_deref(), Some("image/png"));
    assert_eq!(&file.data[..], b"PNGDATA");
    let s2s = provider_s2s();
    assert_eq!(
        (call.subject, call.tenant),
        (s2s.subject_id(), s2s.subject_tenant_id())
    );
}

#[tokio::test]
async fn a_failed_secondary_upload_is_an_error() {
    let mock = MockStorage::new();
    mock.set_default(Route::FilesUpload, Answer::status(500));
    let result = client(&mock)
        .upload_secondary_image(
            &anthropic(),
            "cat.png",
            "image/png",
            Bytes::from_static(b"x"),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(mock.count_of(Route::FilesUpload), 1);
}

#[tokio::test]
async fn a_secondary_file_delete_targets_the_anthropic_upstream() {
    // inst-uf-delete-secondary
    let mock = MockStorage::new();
    client(&mock)
        .delete_secondary_file(&anthropic(), "file_ant_5")
        .await
        .unwrap();
    let call = only(&mock, Route::FilesDelete);
    assert_eq!(call.uri, format!("/{ANTHROPIC_ALIAS}/v1/files/file_ant_5"));
}

// ── cross-cutting ────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_call_runs_under_the_s2s_context_with_no_credential_header() {
    let mock = MockStorage::new();
    let c = client(&mock);
    c.upload_file(&openai(), &meta(), body(&[b"x"]))
        .await
        .unwrap();
    c.delete_file(&openai(), "file-1").await.unwrap();
    c.create_vector_store(&openai()).await.unwrap();
    c.delete_vector_store(&openai(), "vs_1").await.unwrap();
    c.add_vector_store_file(&openai(), "vs_1", "file-1", Uuid::new_v4())
        .await
        .unwrap();
    c.read_vector_store_file(&openai(), "vs_1", "file-1", Duration::from_secs(1))
        .await
        .unwrap();
    c.upload_secondary_image(&anthropic(), "a.png", "image/png", Bytes::from_static(b"x"))
        .await
        .unwrap();
    c.delete_secondary_file(&anthropic(), "file_ant_1")
        .await
        .unwrap();
    let s2s = provider_s2s();
    let calls = mock.calls();
    assert_eq!(calls.len(), 8);
    for call in calls {
        assert_eq!(
            (call.subject, call.tenant),
            (s2s.subject_id(), s2s.subject_tenant_id()),
            "{}",
            call.uri
        );
        for name in ["authorization", "api-key", "x-api-key"] {
            assert!(call.header(name).is_none(), "{} carries `{name}`", call.uri);
        }
        assert!(call.uri.starts_with('/'), "{}", call.uri);
    }
}

#[tokio::test]
async fn an_unbound_s2s_context_sends_nothing() {
    // the gear context is bound at start: before it nothing is sent
    let mock = MockStorage::new();
    let unbound = OagwStorageClient::new(mock.clone(), S2sContext::unbound());
    assert!(unbound.create_vector_store(&openai()).await.is_err());
    assert!(
        unbound
            .upload_file(&openai(), &meta(), body(&[b"x"]))
            .await
            .is_err()
    );
    assert_eq!(mock.call_count(), 0);
}

// ── slice S2-deletion-cleanup-tools: the delete classification of the cleanup handler ──────────
//
// Algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler` (`inst-ch-delete-file`,
// `inst-ch-delete-secondary`: 2xx and 404 are success, any other status or a transport failure is a
// failed attempt); definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler`.

#[tokio::test]
async fn a_file_delete_of_a_missing_file_is_reported_as_not_found() {
    // the handler counts a 404 as success, so the adapter must tell it from another failure
    let mock = MockStorage::new();
    mock.set_default(Route::FilesDelete, Answer::status(404));
    assert_eq!(
        client(&mock).delete_file(&openai(), "file-abc").await,
        Err(ProviderFailure::NotFound)
    );
    assert_eq!(mock.count_of(Route::FilesDelete), 1, "not retried");
}

#[tokio::test]
async fn a_secondary_delete_of_a_missing_file_is_reported_as_not_found() {
    let mock = MockStorage::new();
    mock.set_default(Route::FilesDelete, Answer::status(404));
    assert_eq!(
        client(&mock)
            .delete_secondary_file(&anthropic(), "file_ant_1")
            .await,
        Err(ProviderFailure::NotFound)
    );
}

#[tokio::test]
async fn the_other_delete_failures_keep_their_classification() {
    for (status, expected) in [
        (400, ProviderFailure::Other),
        (401, ProviderFailure::Other),
        (403, ProviderFailure::Other),
        (409, ProviderFailure::Other),
        (429, ProviderFailure::Other),
        (500, ProviderFailure::Transient),
        (502, ProviderFailure::Transient),
        (503, ProviderFailure::Transient),
    ] {
        let mock = MockStorage::new();
        mock.set_default(Route::FilesDelete, Answer::status(status));
        assert_eq!(
            client(&mock).delete_file(&openai(), "file-abc").await,
            Err(expected),
            "{status}"
        );
    }
}

#[tokio::test]
async fn a_gateway_failure_on_a_file_delete_is_transient() {
    let mock = MockStorage::new();
    mock.set_default(
        Route::FilesDelete,
        Answer::Fail(GatewayFailure::Unavailable),
    );
    assert_eq!(
        client(&mock).delete_file(&openai(), "file-abc").await,
        Err(ProviderFailure::Transient)
    );
}

#[tokio::test]
async fn only_the_delete_calls_return_not_found() {
    // a 404 on a status read is still `Other` (the indexing wait fails the row on it)
    let mock = MockStorage::new();
    mock.set_default(Route::VsReadFile, Answer::status(404));
    let read = client(&mock)
        .read_vector_store_file(&openai(), "vs_1", "file-1", Duration::from_secs(1))
        .await;
    assert_eq!(read, Err(ProviderFailure::Other));
}

#[tokio::test]
async fn a_2xx_delete_is_success_whatever_the_body() {
    for body in [json!({ "deleted": true }), json!({}), json!(null)] {
        let mock = MockStorage::new();
        mock.set_default(Route::FilesDelete, Answer::ok(&body));
        client(&mock)
            .delete_file(&openai(), "file-abc")
            .await
            .unwrap();
    }
}
