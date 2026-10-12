#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unnecessary_wraps,
    reason = "test helpers build scripted gateway results"
)]

//! Storage client tests over the fake OAGW gateway: backend resolution, the
//! `OpenAI` / Azure URIs, the multipart upload, the JSON bodies and the
//! outcome classification.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, deadline_exceeded, json_response, lock,
};

const TENANT: Uuid = Uuid::from_u128(0x7e7a);
const OVERRIDE_TENANT: Uuid = Uuid::from_u128(0x0bad);

fn entry(value: serde_json::Value) -> ProviderEntry {
    serde_json::from_value(value).unwrap()
}

fn providers() -> BTreeMap<String, ProviderEntry> {
    BTreeMap::from([
        (
            "openai".to_owned(),
            entry(json!({
                "kind": "openai_responses",
                "host": "api.openai.com",
                "upstream_alias": "api.openai.com",
                "storage_kind": "openai",
                "tenant_overrides": {
                    OVERRIDE_TENANT.to_string(): { "host": "tenant.openai.example" }
                },
            })),
        ),
        (
            "azure".to_owned(),
            entry(json!({
                "kind": "openai_responses",
                "host": "res.openai.azure.com",
                "upstream_alias": "res.openai.azure.com",
                "storage_kind": "azure",
                "storage_backend": "azure-eu",
                "api_version": "2025-04-01-preview",
            })),
        ),
        (
            "claude".to_owned(),
            entry(json!({
                "kind": "anthropic_messages",
                "host": "api.anthropic.com",
                "upstream_alias": "api.anthropic.com",
                "storage_kind": "openai",
                "rag_provider": "azure",
            })),
        ),
    ])
}

fn storage(gateway: &Arc<FakeGateway>) -> ProviderStorage {
    ProviderStorage::new(
        Arc::clone(gateway) as Arc<dyn ServiceGatewayClientV1>,
        Arc::new(StaticSystemContext::default()),
        &providers(),
    )
}

fn target(backend: &str) -> StorageTarget {
    StorageTarget {
        backend: backend.to_owned(),
        tenant_id: TENANT,
    }
}

fn ok(
    body: &serde_json::Value,
) -> Result<Response<Body>, toolkit_canonical_errors::CanonicalError> {
    Ok(json_response(200, body, &[], None))
}

fn status(code: u16) -> Result<Response<Body>, toolkit_canonical_errors::CanonicalError> {
    Ok(json_response(
        code,
        &json!({ "error": { "message": "boom", "code": "x" } }),
        &[],
        None,
    ))
}

// ── Resolution ───────────────────────────────────────────────────────────

#[test]
fn backend_of_a_provider_is_its_rag_provider_label() {
    let gateway = Arc::new(FakeGateway::default());
    let storage = storage(&gateway);

    assert_eq!(storage.backend_for_provider("openai").unwrap(), "openai");
    assert_eq!(storage.backend_for_provider("azure").unwrap(), "azure-eu");
    assert_eq!(storage.backend_for_provider("claude").unwrap(), "azure-eu");
    assert!(matches!(
        storage.backend_for_provider("missing").unwrap_err(),
        DomainError::Internal(_)
    ));
}

#[test]
fn endpoints_follow_the_storage_kind_and_the_tenant_override() {
    let gateway = Arc::new(FakeGateway::default());
    let storage = storage(&gateway);

    let openai = storage.endpoint(&target("openai")).unwrap();
    assert_eq!(openai.uri("files").unwrap(), "/api.openai.com/v1/files");

    let azure = storage.endpoint(&target("azure-eu")).unwrap();
    assert_eq!(azure.provider_id, "azure");
    assert_eq!(
        azure.uri("vector_stores").unwrap(),
        "/res.openai.azure.com/openai/vector_stores?api-version=2025-04-01-preview"
    );

    let tenant = storage
        .endpoint(&StorageTarget {
            backend: "openai".to_owned(),
            tenant_id: OVERRIDE_TENANT,
        })
        .unwrap();
    assert_eq!(tenant.alias, "tenant.openai.example");

    assert!(matches!(
        storage.endpoint(&target("unknown")).unwrap_err(),
        StorageError::Rejected(_)
    ));
}

#[test]
fn azure_without_api_version_is_rejected() {
    let endpoint = StorageEndpoint {
        provider_id: "azure".to_owned(),
        alias: "a".to_owned(),
        kind: StorageKind::Azure,
        api_version: Some("  ".to_owned()),
    };
    assert!(matches!(
        endpoint.uri("files").unwrap_err(),
        StorageError::Rejected(_)
    ));
}

// ── Files API ────────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_sends_multipart_with_purpose_and_returns_the_file_id() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(ok(&json!({ "id": "file-abc123", "object": "file" })));

    let file_id = storage(&gateway)
        .upload_file(
            &target("openai"),
            FileUpload {
                filename: "chat_att.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                data: bytes::Bytes::from_static(b"%PDF-1.4 body"),
            },
        )
        .await
        .unwrap();

    assert_eq!(file_id, "file-abc123");
    let proxied = lock(&gateway.proxied).clone();
    assert_eq!(proxied.len(), 1);
    assert_eq!(proxied[0].method, Method::POST);
    assert_eq!(proxied[0].uri, "/api.openai.com/v1/files");
    let content_type = proxied[0].headers[header::CONTENT_TYPE].to_str().unwrap();
    assert!(content_type.starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8_lossy(&proxied[0].raw).into_owned();
    assert!(
        body.contains("name=\"purpose\"\r\n\r\nassistants\r\n"),
        "{body}"
    );
    assert!(
        body.contains("name=\"file\"; filename=\"chat_att.pdf\"\r\nContent-Type: application/pdf"),
        "{body}"
    );
    assert!(body.contains("%PDF-1.4 body"));
}

#[tokio::test]
async fn upload_outcomes_are_classified() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(status(500));
    gateway.push_proxy(status(429));
    gateway.push_proxy(status(400));
    gateway.push_proxy(Err(deadline_exceeded()));
    gateway.push_proxy(ok(&json!({ "object": "file" })));
    let storage = storage(&gateway);
    let file = || FileUpload {
        filename: "a.txt".to_owned(),
        content_type: "text/plain".to_owned(),
        data: bytes::Bytes::from_static(b"a"),
    };

    let results = [
        storage.upload_file(&target("openai"), file()).await,
        storage.upload_file(&target("openai"), file()).await,
        storage.upload_file(&target("openai"), file()).await,
        storage.upload_file(&target("openai"), file()).await,
        storage.upload_file(&target("openai"), file()).await,
    ];

    let transient: Vec<bool> = results
        .iter()
        .map(|result| result.as_ref().unwrap_err().is_transient())
        .collect();
    assert_eq!(transient, [true, true, false, true, false]);
    assert!(
        results[2]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("HTTP 400: boom")
    );
}

#[tokio::test]
async fn file_delete_treats_not_found_as_success() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "deleted": true }),
        &[],
        None,
    )));
    gateway.push_proxy(status(404));
    gateway.push_proxy(status(503));
    gateway.push_proxy(status(403));
    let storage = storage(&gateway);
    let azure = target("azure-eu");

    storage.delete_file(&azure, "assistant-1").await.unwrap();
    storage.delete_file(&azure, "assistant-2").await.unwrap();
    assert!(
        storage
            .delete_file(&azure, "assistant-3")
            .await
            .unwrap_err()
            .is_transient()
    );
    assert!(
        !storage
            .delete_file(&azure, "assistant-4")
            .await
            .unwrap_err()
            .is_transient()
    );

    let proxied = lock(&gateway.proxied).clone();
    assert_eq!(proxied[0].method, Method::DELETE);
    assert_eq!(
        proxied[0].uri,
        "/res.openai.azure.com/openai/files/assistant-1?api-version=2025-04-01-preview"
    );
}

#[tokio::test]
async fn provider_ids_are_encoded_as_one_path_segment() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(status(404));

    storage(&gateway)
        .delete_file(&target("openai"), "file/../x?y")
        .await
        .unwrap();

    assert_eq!(
        lock(&gateway.proxied)[0].uri,
        "/api.openai.com/v1/files/file%2F..%2Fx%3Fy"
    );
}

#[tokio::test]
async fn missing_s2s_context_is_transient() {
    let gateway = Arc::new(FakeGateway::default());
    let system = StaticSystemContext::default();
    *lock(&system.fail_with) =
        Some(crate::infra::oagw_provisioning::SystemContextError::Unavailable("down".to_owned()));
    let storage = ProviderStorage::new(
        Arc::clone(&gateway) as Arc<dyn ServiceGatewayClientV1>,
        Arc::new(system),
        &providers(),
    );

    let err = storage
        .delete_file(&target("openai"), "file-1")
        .await
        .unwrap_err();

    assert!(err.is_transient());
    assert!(lock(&gateway.proxied).is_empty());
}

// ── Vector Stores API ────────────────────────────────────────────────────

#[tokio::test]
async fn vector_store_create_add_and_status() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(ok(&json!({ "id": "vs_123", "object": "vector_store" })));
    gateway.push_proxy(ok(&json!({ "id": "file-1", "status": "in_progress" })));
    gateway.push_proxy(ok(&json!({ "id": "file-1" })));
    gateway.push_proxy(ok(&json!({ "id": "file-1", "status": "completed" })));
    let storage = storage(&gateway);
    let chat_id = Uuid::from_u128(0xc4a7);
    let attachment_id = Uuid::from_u128(0xa77);
    let openai = target("openai");

    let store = storage.create_vector_store(&openai, chat_id).await.unwrap();
    let added = storage
        .add_file(&openai, &store, "file-1", attachment_id)
        .await
        .unwrap();
    let missing = storage
        .file_status(&openai, &store, "file-1")
        .await
        .unwrap();
    let completed = storage
        .file_status(&openai, &store, "file-1")
        .await
        .unwrap();

    assert_eq!(store, "vs_123");
    assert_eq!(added, VectorStoreFileStatus::InProgress);
    assert_eq!(missing, VectorStoreFileStatus::InProgress, "no status");
    assert_eq!(completed, VectorStoreFileStatus::Completed);
    let proxied = lock(&gateway.proxied).clone();
    assert_eq!(
        (proxied[0].method.clone(), proxied[0].uri.as_str()),
        (Method::POST, "/api.openai.com/v1/vector_stores")
    );
    assert_eq!(proxied[0].body["name"], format!("mini-chat {chat_id}"));
    assert_eq!(
        proxied[1].uri,
        "/api.openai.com/v1/vector_stores/vs_123/files"
    );
    assert_eq!(
        proxied[1].body,
        json!({ "file_id": "file-1", "attributes": { "attachment_id": attachment_id.to_string() } })
    );
    assert_eq!(
        (proxied[2].method.clone(), proxied[2].uri.as_str()),
        (
            Method::GET,
            "/api.openai.com/v1/vector_stores/vs_123/files/file-1"
        )
    );
}

#[test]
fn file_statuses_are_parsed() {
    let parse = |value: serde_json::Value| parse_file_status(&value);
    assert_eq!(parse(json!({})), VectorStoreFileStatus::InProgress);
    assert_eq!(
        parse(json!({ "status": "in_progress" })),
        VectorStoreFileStatus::InProgress
    );
    assert_eq!(
        parse(json!({ "status": "completed" })),
        VectorStoreFileStatus::Completed
    );
    assert_eq!(
        parse(json!({ "status": "failed" })),
        VectorStoreFileStatus::Failed
    );
    assert_eq!(
        parse(json!({ "status": "cancelled" })),
        VectorStoreFileStatus::Cancelled
    );
    assert_eq!(
        parse(json!({ "status": "paused" })),
        VectorStoreFileStatus::Other("paused".to_owned())
    );
}

#[tokio::test]
async fn vector_store_delete_treats_not_found_as_success() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(status(404));
    gateway.push_proxy(status(500));
    let storage = storage(&gateway);

    storage
        .delete_vector_store(&target("openai"), "vs_1")
        .await
        .unwrap();
    assert!(
        storage
            .delete_vector_store(&target("openai"), "vs_2")
            .await
            .unwrap_err()
            .is_transient()
    );
    assert_eq!(
        lock(&gateway.proxied)[0].uri,
        "/api.openai.com/v1/vector_stores/vs_1"
    );
}

#[test]
fn transient_statuses() {
    for code in [500_u16, 502, 503, 504, 429, 408] {
        assert!(
            is_transient_status(StatusCode::from_u16(code).unwrap()),
            "{code}"
        );
    }
    for code in [400_u16, 401, 403, 404, 409, 413, 422] {
        assert!(
            !is_transient_status(StatusCode::from_u16(code).unwrap()),
            "{code}"
        );
    }
}
