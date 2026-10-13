use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use toolkit_security::SecurityContext;

use super::*;
use crate::config::{ProviderKind, StorageKind};
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::transport::{
    HttpResponse, OutgoingBody, ProviderTransport, StreamOutcome, TransportError,
};
use crate::testing::{MockProvider, MockTransport, ctx_a1};

fn provider(kind: StorageKind) -> ResolvedProvider {
    ResolvedProvider {
        provider_id: "p".into(),
        kind: ProviderKind::OpenaiResponses,
        alias: "alias.example".into(),
        api_path: "/v1/responses".into(),
        storage_kind: Some(kind),
        api_version: (kind == StorageKind::Azure).then(|| "2025-04-01-preview".to_owned()),
        storage_backend: "p".into(),
    }
}

fn mock() -> (Arc<MockProvider>, MockTransport) {
    let p = Arc::new(MockProvider::default());
    (Arc::clone(&p), MockTransport(p))
}

#[tokio::test]
async fn upload_openai_multipart_fields() {
    let (m, t) = mock();
    let id = upload_file(
        &t,
        &ctx_a1(),
        &provider(StorageKind::Openai),
        "a.pdf",
        "application/pdf",
        Bytes::from_static(b"12345"),
    )
    .await
    .unwrap();
    assert!(id.starts_with("file-"));
    let r = &m.recorded()[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.uri, "/alias.example/v1/files");
    assert_eq!(
        r.multipart,
        vec![
            ("purpose".to_owned(), None, None, "assistants".len()),
            (
                "file".to_owned(),
                Some("a.pdf".to_owned()),
                Some("application/pdf".to_owned()),
                5
            ),
        ]
    );
}

#[tokio::test]
async fn upload_azure_uri() {
    let (m, t) = mock();
    upload_file(
        &t,
        &ctx_a1(),
        &provider(StorageKind::Azure),
        "a.pdf",
        "application/pdf",
        Bytes::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        m.recorded()[0].uri,
        "/alias.example/openai/files?api-version=2025-04-01-preview"
    );
}

#[tokio::test]
async fn upload_failures() {
    let (m, t) = mock();
    m.upload_statuses.lock().unwrap().extend([500, 400, 429]);
    let p = provider(StorageKind::Openai);
    let e = upload_file(&t, &ctx_a1(), &p, "a", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    assert!(matches!(e, StorageError::Transient(_)));
    let e = upload_file(&t, &ctx_a1(), &p, "a", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    assert_eq!(
        e,
        StorageError::Rejected {
            status: 400,
            message: "upload failed".into()
        }
    );
    let e = upload_file(&t, &ctx_a1(), &p, "a", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    assert!(matches!(e, StorageError::Transient(_)));
}

#[tokio::test]
async fn delete_file_status_mapping() {
    let (mock_provider, t) = mock();
    mock_provider
        .delete_statuses
        .lock()
        .unwrap()
        .extend([200, 404, 503, 403, 429]);
    let prov = provider(StorageKind::Openai);
    let ctx = ctx_a1();
    assert_eq!(
        delete_file(&t, &ctx, &prov, "file-1").await.unwrap(),
        DeleteOutcome::Deleted
    );
    assert_eq!(
        delete_file(&t, &ctx, &prov, "file-1").await.unwrap(),
        DeleteOutcome::NotFound
    );
    assert!(matches!(
        delete_file(&t, &ctx, &prov, "file-1").await.unwrap_err(),
        StorageError::Transient(_)
    ));
    assert!(matches!(
        delete_file(&t, &ctx, &prov, "file-1").await.unwrap_err(),
        StorageError::Rejected { status: 403, .. }
    ));
    assert!(matches!(
        delete_file(&t, &ctx, &prov, "file-1").await.unwrap_err(),
        StorageError::Transient(_)
    ));
    let recorded = mock_provider.recorded();
    assert_eq!(recorded[0].method, "DELETE");
    assert_eq!(recorded[0].uri, "/alias.example/v1/files/file-1");
}

#[tokio::test]
async fn vector_store_lifecycle_openai() {
    let (mock_provider, t) = mock();
    let prov = provider(StorageKind::Openai);
    let ctx = ctx_a1();
    let vs = create_vector_store(&t, &ctx, &prov, "chat-1")
        .await
        .unwrap();
    assert!(vs.starts_with("vs_"));
    let att = uuid::Uuid::from_u128(42);
    assert_eq!(
        add_file_to_vector_store(&t, &ctx, &prov, &vs, "file-1", att)
            .await
            .unwrap(),
        IndexStatus::Completed
    );
    mock_provider.vector_file_status.lock().unwrap().extend([
        "in_progress".to_owned(),
        "failed".to_owned(),
        "cancelled".to_owned(),
    ]);
    assert_eq!(
        get_vector_store_file_status(&t, &ctx, &prov, &vs, "file-1")
            .await
            .unwrap(),
        IndexStatus::InProgress
    );
    assert_eq!(
        get_vector_store_file_status(&t, &ctx, &prov, &vs, "file-1")
            .await
            .unwrap(),
        IndexStatus::Failed("failed".into())
    );
    assert_eq!(
        get_vector_store_file_status(&t, &ctx, &prov, &vs, "file-1")
            .await
            .unwrap(),
        IndexStatus::Failed("cancelled".into())
    );
    mock_provider.delete_statuses.lock().unwrap().push_back(404);
    assert_eq!(
        delete_vector_store(&t, &ctx, &prov, &vs).await.unwrap(),
        DeleteOutcome::NotFound
    );

    let rec = mock_provider.recorded();
    assert_eq!(
        (rec[0].method.as_str(), rec[0].uri.as_str()),
        ("POST", "/alias.example/v1/vector_stores")
    );
    assert_eq!(rec[0].json, Some(serde_json::json!({"name": "chat-1"})));
    assert_eq!(
        rec[1].uri,
        format!("/alias.example/v1/vector_stores/{vs}/files")
    );
    assert_eq!(
        rec[1].json,
        Some(
            serde_json::json!({"file_id": "file-1", "attributes": {"attachment_id": att.to_string()}})
        )
    );
    assert_eq!(
        (rec[2].method.as_str(), rec[2].uri.clone()),
        (
            "GET",
            format!("/alias.example/v1/vector_stores/{vs}/files/file-1")
        )
    );
    assert_eq!(
        (rec[5].method.as_str(), rec[5].uri.clone()),
        ("DELETE", format!("/alias.example/v1/vector_stores/{vs}"))
    );
}

#[tokio::test]
async fn vector_store_azure_uris() {
    let (m, t) = mock();
    let p = provider(StorageKind::Azure);
    let c = ctx_a1();
    let vs = create_vector_store(&t, &c, &p, "n").await.unwrap();
    get_vector_store_file_status(&t, &c, &p, &vs, "f")
        .await
        .unwrap();
    delete_file(&t, &c, &p, "f").await.unwrap();
    let uris: Vec<String> = m.recorded().into_iter().map(|r| r.uri).collect();
    assert_eq!(
        uris,
        vec![
            "/alias.example/openai/vector_stores?api-version=2025-04-01-preview".to_owned(),
            format!(
                "/alias.example/openai/vector_stores/{vs}/files/f?api-version=2025-04-01-preview"
            ),
            "/alias.example/openai/files/f?api-version=2025-04-01-preview".to_owned(),
        ]
    );
}

/// Transport returning a fixed answer (or error) for every request.
struct FixedTransport(Result<HttpResponse, TransportError>);

#[async_trait]
impl ProviderTransport for FixedTransport {
    async fn request(
        &self,
        _ctx: &SecurityContext,
        _method: http::Method,
        _uri: &str,
        _body: OutgoingBody,
    ) -> Result<HttpResponse, TransportError> {
        self.0.clone()
    }

    async fn stream(
        &self,
        _ctx: &SecurityContext,
        _uri: &str,
        _body: serde_json::Value,
    ) -> Result<StreamOutcome, TransportError> {
        Err(TransportError::Other("unused".into()))
    }
}

#[tokio::test]
async fn missing_status_is_in_progress_and_transport_errors_are_transient() {
    let p = provider(StorageKind::Openai);
    let c = ctx_a1();
    let t = FixedTransport(Ok(HttpResponse {
        status: 200,
        retry_after_secs: None,
        body: Bytes::from_static(b"{\"id\":\"x\"}"),
    }));
    assert_eq!(
        get_vector_store_file_status(&t, &c, &p, "vs", "f")
            .await
            .unwrap(),
        IndexStatus::InProgress
    );

    let t = FixedTransport(Err(TransportError::Timeout("slow".into())));
    assert!(matches!(
        delete_file(&t, &c, &p, "f").await.unwrap_err(),
        StorageError::Transient(_)
    ));
    assert!(matches!(
        create_vector_store(&t, &c, &p, "n").await.unwrap_err(),
        StorageError::Transient(_)
    ));

    let t = FixedTransport(Ok(HttpResponse {
        status: 200,
        retry_after_secs: None,
        body: Bytes::from_static(b"{}"),
    }));
    assert!(matches!(
        create_vector_store(&t, &c, &p, "n").await.unwrap_err(),
        StorageError::Transient(_)
    ));
}
