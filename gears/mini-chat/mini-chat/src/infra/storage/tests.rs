use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use http::Method;
use oagw_sdk::body::{BodyStream, BoxError};
use serde_json::json;
use toolkit_canonical_errors::resource_error;
use uuid::Uuid;

use super::*;
use crate::config::StorageKind;
use crate::infra::llm::S2sContext;
use crate::test_support::authn::s2s_security_context;
use crate::test_support::gateway::{FakeGateway, Responder};

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct ProxyError;

fn target(kind: StorageKind, api_version: Option<&str>) -> StorageTarget {
    StorageTarget {
        provider_id: "openai".to_owned(),
        storage_kind: kind,
        alias: "rag.test".to_owned(),
        api_version: api_version.map(str::to_owned),
        storage_backend: "openai".to_owned(),
    }
}

fn openai() -> StorageTarget {
    target(StorageKind::Openai, None)
}

fn storage(gateway: &Arc<FakeGateway>) -> OpenAiStorage {
    let s2s = S2sContext::new();
    s2s.set(s2s_security_context());
    OpenAiStorage::new(Arc::clone(gateway) as _, s2s)
}

fn fixture() -> (Arc<FakeGateway>, OpenAiStorage) {
    let gateway = Arc::new(FakeGateway::new());
    let storage = storage(&gateway);
    (gateway, storage)
}

fn upload(chunks: &[&'static str]) -> FileUpload {
    let items: Vec<Result<Bytes, BoxError>> = chunks.iter().map(|c| Ok(Bytes::from(*c))).collect();
    let body: BodyStream = Box::pin(stream::iter(items));
    FileUpload {
        filename: "report.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        body,
    }
}

fn status_of(value: serde_json::Value) -> IndexStatus {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::GET,
        "/vector_stores/vs_a/files/f_a",
        Responder::json(200, value),
    );
    futures::executor::block_on(storage.file_status(&openai(), "vs_a", "f_a")).unwrap()
}

#[tokio::test]
async fn upload_sends_multipart_purpose_assistants() {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::POST,
        "/files",
        Responder::json(200, json!({"id": "file-abc123"})),
    );

    let id = storage
        .upload(&openai(), upload(&["hello ", "world"]))
        .await
        .unwrap();

    assert_eq!(id, "file-abc123");
    let reqs = gateway.requests_to(&Method::POST, "/files");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].uri, "/rag.test/v1/files");
    let content_type = reqs[0].headers[http::header::CONTENT_TYPE]
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    let body = String::from_utf8(reqs[0].body.to_vec()).unwrap();
    assert!(body.contains("name=\"purpose\""), "{body}");
    assert!(body.contains("assistants"), "{body}");
    assert!(body.contains("name=\"file\""), "{body}");
    assert!(body.contains("filename=\"report.pdf\""), "{body}");
    assert!(body.contains("Content-Type: application/pdf"), "{body}");
    assert!(body.contains("hello world"), "{body}");
}

#[tokio::test]
async fn azure_uses_openai_prefix_and_api_version() {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::POST,
        "/files",
        Responder::json(200, json!({"id": "file-1"})),
    );
    gateway.on(
        Method::POST,
        "/vector_stores",
        Responder::json(200, json!({"id": "vs-1"})),
    );
    gateway.on(
        Method::GET,
        "/files/",
        Responder::json(200, json!({"status": "completed"})),
    );
    gateway.on(
        Method::DELETE,
        "/files/",
        Responder::json(200, json!({"deleted": true})),
    );
    let azure = target(StorageKind::Azure, Some("2025-03-01-preview"));

    storage.upload(&azure, upload(&["x"])).await.unwrap();
    VectorStores::create(&storage, &azure, "chat-1")
        .await
        .unwrap();
    storage.file_status(&azure, "vs-1", "file-1").await.unwrap();
    FileStorage::delete(&storage, &azure, "file-1")
        .await
        .unwrap();

    let uris: Vec<String> = gateway.requests().into_iter().map(|r| r.uri).collect();
    assert_eq!(
        uris,
        [
            "/rag.test/openai/files?api-version=2025-03-01-preview",
            "/rag.test/openai/vector_stores?api-version=2025-03-01-preview",
            "/rag.test/openai/vector_stores/vs-1/files/file-1?api-version=2025-03-01-preview",
            "/rag.test/openai/files/file-1?api-version=2025-03-01-preview",
        ]
    );
}

#[tokio::test]
async fn vector_store_create_sends_name_and_returns_id() {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::POST,
        "/vector_stores",
        Responder::json(200, json!({"id": "vs_123"})),
    );

    let id = VectorStores::create(&storage, &openai(), "chat-42")
        .await
        .unwrap();

    assert_eq!(id, "vs_123");
    let req = &gateway.requests()[0];
    assert_eq!(req.uri, "/rag.test/v1/vector_stores");
    assert_eq!(req.json, Some(json!({"name": "chat-42"})));
    assert_eq!(req.headers[http::header::CONTENT_TYPE], "application/json");
}

#[tokio::test]
async fn vector_store_add_file_sends_attachment_attribute() {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::POST,
        "/vector_stores/vs_1/files",
        Responder::json(200, json!({"id": "file-9", "status": "in_progress"})),
    );
    let attachment = Uuid::from_u128(0xAB);

    let status = storage
        .add_file(&openai(), "vs_1", "file-9", attachment)
        .await
        .unwrap();

    assert_eq!(status, IndexStatus::InProgress);
    let req = &gateway.requests()[0];
    assert_eq!(req.uri, "/rag.test/v1/vector_stores/vs_1/files");
    assert_eq!(
        req.json,
        Some(json!({"file_id": "file-9", "attributes": {"attachment_id": attachment.to_string()}}))
    );
}

#[test]
fn status_mapping() {
    assert_eq!(status_of(json!({})), IndexStatus::InProgress);
    assert_eq!(status_of(json!({"status": null})), IndexStatus::InProgress);
    assert_eq!(
        status_of(json!({"status": "in_progress"})),
        IndexStatus::InProgress
    );
    assert_eq!(
        status_of(json!({"status": "completed"})),
        IndexStatus::Completed
    );
    assert!(
        matches!(status_of(json!({"status": "failed"})), IndexStatus::Failed(m) if m.contains("failed"))
    );
    assert!(
        matches!(status_of(json!({"status": "cancelled"})), IndexStatus::Failed(m) if m.contains("cancelled"))
    );
    assert!(
        matches!(status_of(json!({"status": "weird"})), IndexStatus::Failed(m) if m.contains("weird"))
    );
}

#[tokio::test]
async fn failed_status_reason_is_sanitized() {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::GET,
        "/vector_stores/vs_1/files/file-9",
        Responder::json(
            200,
            json!({"status": "failed", "last_error": {"code": "invalid_file", "message": "cannot parse file-AbCdEf123456789"}}),
        ),
    );

    let status = storage
        .file_status(&openai(), "vs_1", "file-9")
        .await
        .unwrap();

    let IndexStatus::Failed(reason) = status else {
        panic!("{status:?}")
    };
    assert!(reason.contains("cannot parse [provider_id]"), "{reason}");
    assert!(!reason.contains("AbCdEf"), "{reason}");
}

#[tokio::test]
async fn delete_404_is_success_500_is_transient_400_is_permanent() {
    let (gateway, storage) = fixture();
    let t = openai();

    gateway.on(
        Method::DELETE,
        "/files/f404",
        Responder::json(404, json!({"error": {"message": "no such file"}})),
    );
    gateway.on(
        Method::DELETE,
        "/files/f200",
        Responder::json(200, json!({"deleted": true})),
    );
    gateway.on(
        Method::DELETE,
        "/files/f204",
        Responder::json(204, json!({})),
    );
    gateway.on(
        Method::DELETE,
        "/files/f500",
        Responder::json(500, json!({"error": {"message": "boom"}})),
    );
    gateway.on(
        Method::DELETE,
        "/files/f400",
        Responder::json(
            400,
            json!({"error": {"message": "bad file-AbCdEf123456789"}}),
        ),
    );
    gateway.on(
        Method::DELETE,
        "/vector_stores/vs404",
        Responder::json(404, json!({})),
    );
    gateway.on(
        Method::DELETE,
        "/vector_stores/vs500",
        Responder::json(503, json!({})),
    );

    assert_eq!(FileStorage::delete(&storage, &t, "f404").await, Ok(()));
    assert_eq!(FileStorage::delete(&storage, &t, "f200").await, Ok(()));
    assert_eq!(FileStorage::delete(&storage, &t, "f204").await, Ok(()));
    assert!(matches!(
        FileStorage::delete(&storage, &t, "f500").await,
        Err(StorageError::Transient(_))
    ));
    let Err(StorageError::Permanent { status, message }) =
        FileStorage::delete(&storage, &t, "f400").await
    else {
        panic!("400 must be permanent");
    };
    assert_eq!(status, 400);
    assert_eq!(message, "bad [provider_id]");
    assert_eq!(VectorStores::delete(&storage, &t, "vs404").await, Ok(()));
    assert!(matches!(
        VectorStores::delete(&storage, &t, "vs500").await,
        Err(StorageError::Transient(_))
    ));
}

#[tokio::test]
async fn gateway_failures_are_transient_even_for_404() {
    let (gateway, storage) = fixture();
    gateway.on(Method::DELETE, "/files/a", Responder::GatewayStatus(404));
    gateway.on(Method::DELETE, "/files/b", Responder::GatewayStatus(504));
    gateway.on(
        Method::DELETE,
        "/files/c",
        Responder::Err(ProxyError::deadline_exceeded("slow").create()),
    );

    for id in ["a", "b", "c"] {
        assert!(
            matches!(
                FileStorage::delete(&storage, &openai(), id).await,
                Err(StorageError::Transient(_))
            ),
            "{id}"
        );
    }
}

#[tokio::test]
async fn missing_s2s_context_is_transient_and_sends_nothing() {
    let gateway = Arc::new(FakeGateway::new());
    let storage = OpenAiStorage::new(Arc::clone(&gateway) as _, S2sContext::new());

    assert!(matches!(
        VectorStores::create(&storage, &openai(), "chat-1").await,
        Err(StorageError::Transient(_))
    ));
    assert!(matches!(
        storage.upload(&openai(), upload(&["x"])).await,
        Err(StorageError::Transient(_))
    ));
    assert!(gateway.requests().is_empty());
}

#[tokio::test]
async fn invalid_provider_ids_never_reach_the_path() {
    let (gateway, storage) = fixture();

    let err = FileStorage::delete(&storage, &openai(), "../vector_stores/x").await;

    assert!(
        matches!(err, Err(StorageError::Permanent { status: 400, .. })),
        "{err:?}"
    );
    assert!(gateway.requests().is_empty());
}

#[tokio::test]
async fn success_without_id_is_transient() {
    let (gateway, storage) = fixture();
    gateway.on(
        Method::POST,
        "/vector_stores",
        Responder::json(200, json!({})),
    );

    assert!(matches!(
        VectorStores::create(&storage, &openai(), "chat-1").await,
        Err(StorageError::Transient(_))
    ));
}
