use std::sync::Arc;

use bytes::Bytes;
use serde_json::json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::config::StorageKind;
use crate::test_support::FakeOagw;
use crate::test_support::fake_oagw::{gateway_timeout_error, invalid_argument_error};

const API_VERSION: &str = "2025-03-01-preview";

fn openai() -> StorageTarget {
    StorageTarget {
        provider_id: "openai".into(),
        storage_kind: StorageKind::Openai,
        alias: "api.openai.com".into(),
        api_version: None,
        storage_backend: "openai".into(),
    }
}

fn azure() -> StorageTarget {
    StorageTarget {
        provider_id: "azure".into(),
        storage_kind: StorageKind::Azure,
        alias: "azure-main".into(),
        api_version: Some(API_VERSION.into()),
        storage_backend: "azure".into(),
    }
}

fn storage() -> (Arc<FakeOagw>, RagStorage) {
    let fake = Arc::new(FakeOagw::new());
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(10))
        .subject_tenant_id(Uuid::from_u128(11))
        .build()
        .unwrap();
    let rag = RagStorage::new(fake.clone(), Arc::new(S2sContextProvider::fixed(ctx)));
    (fake, rag)
}

#[tokio::test]
async fn upload_openai_multipart_fields() {
    let (fake, rag) = storage();
    fake.push_json(
        "/v1/files",
        200,
        json!({"id": "file-abc123", "object": "file"}),
    );

    let id = rag
        .upload_file(
            &openai(),
            "chat_att.pdf",
            "application/pdf",
            Bytes::from("%PDF"),
        )
        .await
        .unwrap();

    assert_eq!(id, "file-abc123");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].uri, "/api.openai.com/v1/files");
    assert_eq!(
        reqs[0].multipart_fields,
        vec![
            ("purpose".to_owned(), None, None),
            (
                "file".to_owned(),
                Some("chat_att.pdf".to_owned()),
                Some("application/pdf".to_owned())
            ),
        ]
    );
}

#[tokio::test]
async fn upload_sends_the_file_bytes_and_purpose_value() {
    let (fake, rag) = storage();
    fake.push_json("/v1/files", 200, json!({"id": "file-1"}));
    rag.upload_file(&openai(), "a.txt", "text/plain", Bytes::from("hello"))
        .await
        .unwrap();
    let body = fake.requests()[0].raw_body.clone();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("name=\"purpose\"\r\n\r\nassistants\r\n"),
        "{text}"
    );
    assert!(text.contains("\r\n\r\nhello\r\n"), "{text}");
}

#[tokio::test]
async fn azure_paths_have_api_version() {
    let (fake, rag) = storage();
    let t = azure();
    fake.push_json("/openai/files", 200, json!({"id": "file-1"}));
    fake.push_json("/openai/files/", 200, json!({}));
    fake.push_json("/openai/vector_stores", 200, json!({"id": "vs_1"}));
    fake.push_json(
        "/openai/vector_stores/vs_1/files",
        200,
        json!({"status": "in_progress"}),
    );
    fake.push_json(
        "/openai/vector_stores/vs_1/files/file-1",
        200,
        json!({"status": "completed"}),
    );
    fake.push_json("/openai/vector_stores/vs_1", 200, json!({}));

    rag.upload_file(&t, "a.txt", "text/plain", Bytes::from("x"))
        .await
        .unwrap();
    rag.delete_file(&t, "file-1").await.unwrap();
    rag.create_vector_store(&t, Uuid::from_u128(1))
        .await
        .unwrap();
    rag.add_file_to_vector_store(&t, "vs_1", "file-1", Uuid::from_u128(2))
        .await
        .unwrap();
    rag.vector_store_file_status(&t, "vs_1", "file-1")
        .await
        .unwrap();
    rag.delete_vector_store(&t, "vs_1").await.unwrap();

    let uris: Vec<_> = fake
        .requests()
        .into_iter()
        .map(|r| (r.method, r.uri))
        .collect();
    let q = format!("?api-version={API_VERSION}");
    let expected = [
        ("POST", format!("/azure-main/openai/files{q}")),
        ("DELETE", format!("/azure-main/openai/files/file-1{q}")),
        ("POST", format!("/azure-main/openai/vector_stores{q}")),
        (
            "POST",
            format!("/azure-main/openai/vector_stores/vs_1/files{q}"),
        ),
        (
            "GET",
            format!("/azure-main/openai/vector_stores/vs_1/files/file-1{q}"),
        ),
        (
            "DELETE",
            format!("/azure-main/openai/vector_stores/vs_1{q}"),
        ),
    ];
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(m, u)| (m.to_owned(), u))
        .collect();
    assert_eq!(uris, expected);
}

#[tokio::test]
async fn azure_without_api_version_is_permanent_and_sends_nothing() {
    let (fake, rag) = storage();
    let mut t = azure();
    t.api_version = None;
    let err = rag.delete_file(&t, "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Permanent(_)), "{err:?}");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn delete_file_uses_the_openai_path() {
    let (fake, rag) = storage();
    fake.push_json("/v1/files/", 200, json!({"id": "file-1", "deleted": true}));
    rag.delete_file(&openai(), "file-1").await.unwrap();
    let r = &fake.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.uri.as_str()),
        ("DELETE", "/api.openai.com/v1/files/file-1")
    );
}

#[tokio::test]
async fn delete_404_is_ok() {
    let (fake, rag) = storage();
    fake.push_json(
        "/v1/files/",
        404,
        json!({"error": {"message": "No such file"}}),
    );
    fake.push_json(
        "/v1/vector_stores/",
        404,
        json!({"error": {"message": "No such store"}}),
    );
    rag.delete_file(&openai(), "file-1").await.unwrap();
    rag.delete_vector_store(&openai(), "vs_1").await.unwrap();
}

#[tokio::test]
async fn delete_500_is_transient() {
    let (fake, rag) = storage();
    fake.push_json("/v1/files/", 500, json!({"error": {"message": "boom"}}));
    fake.push_json("/v1/vector_stores/", 503, json!({}));
    let e1 = rag.delete_file(&openai(), "file-1").await.unwrap_err();
    let e2 = rag
        .delete_vector_store(&openai(), "vs_1")
        .await
        .unwrap_err();
    assert!(matches!(e1, StorageError::Transient(_)), "{e1:?}");
    assert!(matches!(e2, StorageError::Transient(_)), "{e2:?}");
}

#[tokio::test]
async fn delete_other_4xx_is_permanent() {
    let (fake, rag) = storage();
    fake.push_json("/v1/files/", 403, json!({"error": {"message": "nope"}}));
    let err = rag.delete_file(&openai(), "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn gateway_404_on_delete_is_not_success() {
    let (fake, rag) = storage();
    fake.push_gateway_response(
        "/v1/files/",
        404,
        json!({"type": "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1~", "status": 404}),
    );
    let err = rag.delete_file(&openai(), "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn proxy_failures_map_to_transient_or_permanent() {
    let (fake, rag) = storage();
    fake.push_error("/v1/files", gateway_timeout_error());
    fake.push_error("/v1/files", invalid_argument_error("bad request"));
    let timeout = rag
        .upload_file(&openai(), "a.txt", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    let invalid = rag
        .upload_file(&openai(), "a.txt", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    assert!(matches!(timeout, StorageError::Transient(_)), "{timeout:?}");
    assert!(matches!(invalid, StorageError::Permanent(_)), "{invalid:?}");
}

#[tokio::test]
async fn upload_errors_by_status_and_missing_id() {
    let (fake, rag) = storage();
    fake.push_json("/v1/files", 400, json!({"error": {"message": "bad file"}}));
    fake.push_json("/v1/files", 502, json!({}));
    fake.push_json("/v1/files", 429, json!({}));
    fake.push_json("/v1/files", 200, json!({"object": "file"}));
    let mut out = Vec::new();
    for _ in 0..4 {
        out.push(
            rag.upload_file(&openai(), "a.txt", "text/plain", Bytes::new())
                .await
                .unwrap_err(),
        );
    }
    assert!(
        matches!(&out[0], StorageError::Permanent(m) if m.contains("bad file")),
        "{:?}",
        out[0]
    );
    assert!(matches!(out[1], StorageError::Transient(_)), "{:?}", out[1]);
    // D "File Upload": transient = provider 5xx or gateway failure; a
    // provider 429 is not.
    assert!(matches!(out[2], StorageError::Permanent(_)), "{:?}", out[2]);
    assert!(matches!(out[3], StorageError::Permanent(_)), "{:?}", out[3]);
}

#[tokio::test]
async fn gateway_throttling_429_is_transient() {
    let (fake, rag) = storage();
    fake.push_gateway_response(
        "/v1/files",
        429,
        json!({"type": "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1~", "status": 429}),
    );
    let err = rag
        .upload_file(&openai(), "a.txt", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Transient(_)), "{err:?}");
}

#[tokio::test]
async fn error_messages_never_carry_provider_ids() {
    let (fake, rag) = storage();
    fake.push_json(
        "/v1/files",
        400,
        json!({"error": {"message": "file-abc123def456ghi is invalid, see https://x.example/y"}}),
    );
    let err = rag
        .upload_file(&openai(), "a.txt", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    let StorageError::Permanent(msg) = err else {
        panic!("{err:?}")
    };
    assert!(
        !msg.contains("file-abc123def456ghi") && !msg.contains("https://"),
        "{msg}"
    );
}

#[tokio::test]
async fn create_vector_store_sends_chat_name() {
    let (fake, rag) = storage();
    fake.push_json("/v1/vector_stores", 200, json!({"id": "vs_abc"}));
    let chat = Uuid::from_u128(0xC4A7);
    let id = rag.create_vector_store(&openai(), chat).await.unwrap();
    assert_eq!(id, "vs_abc");
    let r = &fake.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.uri.as_str()),
        ("POST", "/api.openai.com/v1/vector_stores")
    );
    assert_eq!(r.json_body, Some(json!({"name": format!("chat-{chat}")})));
}

#[tokio::test]
async fn vector_store_add_sends_attachment_attribute() {
    let (fake, rag) = storage();
    fake.push_json(
        "/v1/vector_stores/vs_1/files",
        200,
        json!({"id": "file-1", "status": "in_progress"}),
    );
    let att = Uuid::from_u128(0xA77);
    let status = rag
        .add_file_to_vector_store(&openai(), "vs_1", "file-1", att)
        .await
        .unwrap();
    assert_eq!(status, IndexStatus::InProgress);
    let r = &fake.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.uri.as_str()),
        ("POST", "/api.openai.com/v1/vector_stores/vs_1/files")
    );
    assert_eq!(
        r.json_body,
        Some(json!({"file_id": "file-1", "attributes": {"attachment_id": att.to_string()}}))
    );
}

#[tokio::test]
async fn add_file_failure_statuses() {
    let (fake, rag) = storage();
    fake.push_json(
        "/v1/vector_stores/vs_1/files",
        400,
        json!({"error": {"message": "bad"}}),
    );
    fake.push_json("/v1/vector_stores/vs_1/files", 500, json!({}));
    let e1 = rag
        .add_file_to_vector_store(&openai(), "vs_1", "f", Uuid::nil())
        .await
        .unwrap_err();
    let e2 = rag
        .add_file_to_vector_store(&openai(), "vs_1", "f", Uuid::nil())
        .await
        .unwrap_err();
    assert!(matches!(e1, StorageError::Permanent(_)), "{e1:?}");
    assert!(matches!(e2, StorageError::Transient(_)), "{e2:?}");
}

#[tokio::test]
async fn missing_status_is_in_progress() {
    let (fake, rag) = storage();
    fake.push_json("/v1/vector_stores/vs_1/files", 200, json!({"id": "file-1"}));
    fake.push_json(
        "/v1/vector_stores/vs_1/files/file-1",
        200,
        json!({"id": "file-1"}),
    );
    let added = rag
        .add_file_to_vector_store(&openai(), "vs_1", "file-1", Uuid::nil())
        .await
        .unwrap();
    let polled = rag
        .vector_store_file_status(&openai(), "vs_1", "file-1")
        .await
        .unwrap();
    assert_eq!(added, IndexStatus::InProgress);
    assert_eq!(polled, IndexStatus::InProgress);
    let r = &fake.requests()[1];
    assert_eq!(
        (r.method.as_str(), r.uri.as_str()),
        ("GET", "/api.openai.com/v1/vector_stores/vs_1/files/file-1")
    );
}

#[tokio::test]
async fn completed_status_is_completed() {
    let (fake, rag) = storage();
    fake.push_json(
        "/v1/vector_stores/vs_1/files/file-1",
        200,
        json!({"status": "completed"}),
    );
    let s = rag
        .vector_store_file_status(&openai(), "vs_1", "file-1")
        .await
        .unwrap();
    assert_eq!(s, IndexStatus::Completed);
}

#[tokio::test]
async fn cancelled_status_is_failed() {
    let (fake, rag) = storage();
    for status in ["cancelled", "failed", "something_new"] {
        fake.push_json(
            "/v1/vector_stores/vs_1/files/file-1",
            200,
            json!({"status": status}),
        );
        let s = rag
            .vector_store_file_status(&openai(), "vs_1", "file-1")
            .await
            .unwrap();
        assert_eq!(s, IndexStatus::Failed, "{status}");
    }
    fake.push_json(
        "/v1/vector_stores/vs_1/files",
        200,
        json!({"status": "cancelled"}),
    );
    let added = rag
        .add_file_to_vector_store(&openai(), "vs_1", "file-1", Uuid::nil())
        .await
        .unwrap();
    assert_eq!(added, IndexStatus::Failed);
}

#[tokio::test]
async fn status_read_errors_keep_their_class() {
    let (fake, rag) = storage();
    fake.push_json("/v1/vector_stores/vs_1/files/file-1", 500, json!({}));
    fake.push_json("/v1/vector_stores/vs_1/files/file-1", 404, json!({}));
    let e1 = rag
        .vector_store_file_status(&openai(), "vs_1", "file-1")
        .await
        .unwrap_err();
    let e2 = rag
        .vector_store_file_status(&openai(), "vs_1", "file-1")
        .await
        .unwrap_err();
    assert!(matches!(e1, StorageError::Transient(_)), "{e1:?}");
    assert!(matches!(e2, StorageError::Permanent(_)), "{e2:?}");
}
