#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use http::Method;
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::config::{ProviderKind, StorageKind};
use crate::infra::llm::ResolvedProvider;
use crate::infra::oagw::s2s::S2sContext;
use crate::testing::{FakeProvider, TestUser};

fn provider(kind: StorageKind, api_version: Option<&str>) -> ResolvedProvider {
    ResolvedProvider {
        provider_id: "p".to_owned(),
        kind: ProviderKind::OpenaiResponses,
        alias: "rag.example.com".to_owned(),
        api_path: "/v1/responses".to_owned(),
        storage_kind: Some(kind),
        storage_backend: "p".to_owned(),
        api_version: api_version.map(str::to_owned),
        tenant_id: Uuid::nil(),
    }
}

fn client() -> (Arc<FakeProvider>, RagClient) {
    let fake = FakeProvider::new();
    let s2s = Arc::new(S2sContext::new());
    s2s.set(TestUser::S2S.security_context());
    let rag = RagClient::new(
        Arc::clone(&fake) as Arc<dyn oagw_sdk::ServiceGatewayClientV1>,
        s2s,
    );
    (fake, rag)
}

#[test]
fn openai_paths() {
    let p = provider(StorageKind::Openai, None);
    assert_eq!(storage_uri(&p, "/files"), "/rag.example.com/v1/files");
    assert_eq!(
        storage_uri(&p, "/vector_stores/vs_1/files/file-1"),
        "/rag.example.com/v1/vector_stores/vs_1/files/file-1"
    );
}

#[test]
fn azure_paths_carry_the_api_version() {
    let p = provider(StorageKind::Azure, Some("2024-05-01-preview"));
    assert_eq!(
        storage_uri(&p, "/files"),
        "/rag.example.com/openai/files?api-version=2024-05-01-preview"
    );
    assert_eq!(
        storage_uri(&p, "/vector_stores/vs_1"),
        "/rag.example.com/openai/vector_stores/vs_1?api-version=2024-05-01-preview"
    );
}

#[test]
fn file_status_parsing() {
    use VsFileStatus::{Completed, Failed, InProgress};
    for (body, expected) in [
        (json!({"status": "in_progress"}), InProgress),
        (json!({"id": "file-1"}), InProgress),
        (json!({"status": "completed"}), Completed),
        (json!({"status": "failed"}), Failed),
        (json!({"status": "cancelled"}), Failed),
        (json!({"status": "something_new"}), Failed),
    ] {
        assert_eq!(VsFileStatus::from_response(&body), expected, "{body}");
    }
}

#[tokio::test]
async fn upload_sends_multipart_with_purpose_and_filename() {
    let (fake, rag) = client();
    let p = provider(StorageKind::Openai, None);
    let id = rag
        .upload_file(
            &p,
            "c_a.pdf",
            "application/pdf",
            Bytes::from_static(b"%PDF"),
        )
        .await
        .unwrap();
    let files = fake.files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, id);
    assert_eq!(files[0].purpose, FILE_PURPOSE);
    assert_eq!(files[0].filename, "c_a.pdf");
    assert_eq!(files[0].content_type.as_deref(), Some("application/pdf"));
    let req = &fake.requests()[0];
    assert_eq!(req.method, Method::POST);
    assert_eq!(req.path, "/rag.example.com/v1/files");
    assert_eq!(req.subject_id, TestUser::S2S.user_id);
}

#[tokio::test]
async fn vector_store_calls_use_documented_bodies() {
    let (fake, rag) = client();
    let p = provider(StorageKind::Azure, Some("v1"));
    let file = rag
        .upload_file(&p, "f.txt", "text/plain", Bytes::from_static(b"x"))
        .await
        .unwrap();
    let vs = rag.create_vector_store(&p, "chat_x").await.unwrap();
    let attachment = Uuid::new_v4();
    fake.set_vector_store_file_statuses(vec!["in_progress", "completed"]);
    assert_eq!(
        rag.add_file(&p, &vs, &file, attachment).await.unwrap(),
        VsFileStatus::InProgress
    );
    assert_eq!(
        rag.file_status(&p, &vs, &file).await.unwrap(),
        VsFileStatus::Completed
    );
    let reqs = fake.requests();
    assert!(
        reqs.iter()
            .all(|r| r.query.as_deref() == Some("api-version=v1"))
    );
    assert_eq!(reqs[1].json, Some(json!({"name": "chat_x"})));
    assert_eq!(
        reqs[2].json,
        Some(json!({"file_id": file, "attributes": {"attachment_id": attachment.to_string()}}))
    );
    assert_eq!(reqs[3].method, Method::GET);
    assert_eq!(
        reqs[3].path,
        format!("/rag.example.com/openai/vector_stores/{vs}/files/{file}")
    );
}

#[tokio::test]
async fn deletes_treat_404_as_success() {
    let (fake, rag) = client();
    let p = provider(StorageKind::Openai, None);
    let file = rag
        .upload_file(&p, "f.txt", "text/plain", Bytes::from_static(b"x"))
        .await
        .unwrap();
    rag.delete_file(&p, &file).await.unwrap();
    assert!(fake.files()[0].deleted);
    // Already gone: provider 404.
    rag.delete_file(&p, &file).await.unwrap();
    rag.delete_vector_store(&p, "vs_missing").await.unwrap();
    // Any other status fails.
    fake.fail_next("/v1/files", 500);
    assert!(
        rag.delete_file(&p, "file-x")
            .await
            .unwrap_err()
            .is_transient()
    );
    fake.fail_next("/v1/vector_stores", 403);
    assert!(matches!(
        rag.delete_vector_store(&p, "vs_x").await.unwrap_err(),
        StorageError::Failed(_)
    ));
}

#[tokio::test]
async fn error_classification() {
    let (fake, rag) = client();
    let p = provider(StorageKind::Openai, None);
    for (status, transient) in [
        (500, true),
        (502, true),
        (429, true),
        (408, true),
        (400, false),
        (401, false),
    ] {
        fake.fail_next("/v1/vector_stores", status);
        let err = rag.create_vector_store(&p, "n").await.unwrap_err();
        assert_eq!(err.is_transient(), transient, "{status}: {err}");
    }
    // A provider 404 on a status read is a (non-transient) failure.
    let err = rag
        .file_status(&p, "vs_unknown", "file-unknown")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Failed(_)), "{err}");
}
