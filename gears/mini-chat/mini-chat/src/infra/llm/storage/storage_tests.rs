#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, ServiceGatewayClientV1,
    UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::StorageKind;
use crate::domain::ports::{IndexStatus, RagStorage, StorageError};
use crate::infra::llm::ServiceIdentity;
use crate::infra::llm::storage::OagwRagStorage;
use crate::infra::llm::types::ResolvedStorage;
use crate::test_support::test_ctx;

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct ProxyErr;

// -- Fake gateway ---------------------------------------------------------

enum Reply {
    Response {
        status: u16,
        source: Option<ErrorSource>,
        body: Bytes,
    },
    Error(CanonicalError),
}

#[derive(Debug, Clone)]
struct Captured {
    method: http::Method,
    uri: String,
    content_type: Option<String>,
    body: Bytes,
    subject_id: Uuid,
}

impl Captured {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn text(&self) -> String {
        String::from_utf8(self.body.to_vec()).unwrap()
    }
}

#[derive(Default)]
struct FakeGw {
    replies: Mutex<Vec<Reply>>,
    captured: Mutex<Vec<Captured>>,
}

impl FakeGw {
    fn with(reply: Reply) -> Arc<Self> {
        let gw = Arc::new(Self::default());
        gw.replies.lock().unwrap().push(reply);
        gw
    }

    fn last(&self) -> Captured {
        self.captured.lock().unwrap().last().cloned().unwrap()
    }
}

fn unused() -> CanonicalError {
    CanonicalError::internal("not used by the storage client").create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGw {
    async fn create_upstream(
        &self,
        _: SecurityContext,
        _: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }
    async fn get_upstream(&self, _: SecurityContext, _: Uuid) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }
    async fn list_upstreams(
        &self,
        _: SecurityContext,
        _: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Err(unused())
    }
    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }
    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Err(unused())
    }
    async fn create_route(
        &self,
        _: SecurityContext,
        _: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unused())
    }
    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        Err(unused())
    }
    async fn list_routes(
        &self,
        _: SecurityContext,
        _: Option<Uuid>,
        _: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Err(unused())
    }
    async fn update_route(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unused())
    }
    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Err(unused())
    }
    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unused())
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap();
        self.captured.lock().unwrap().push(Captured {
            method: parts.method,
            uri: parts.uri.to_string(),
            content_type: parts
                .headers
                .get(http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap().to_owned()),
            body: bytes,
            subject_id: ctx.subject_id(),
        });
        match self.replies.lock().unwrap().remove(0) {
            Reply::Error(e) => Err(e),
            Reply::Response {
                status,
                source,
                body,
            } => {
                let mut resp = http::Response::builder()
                    .status(status)
                    .body(Body::from(body))
                    .unwrap();
                if let Some(source) = source {
                    resp.extensions_mut().insert(source);
                }
                Ok(resp)
            }
        }
    }
}

// -- Helpers --------------------------------------------------------------

fn ok(status: u16, body: &Value) -> Reply {
    Reply::Response {
        status,
        source: None,
        body: Bytes::from(body.to_string()),
    }
}

fn upstream_err(status: u16) -> Reply {
    Reply::Response {
        status,
        source: Some(ErrorSource::Upstream),
        body: Bytes::from(json!({"error": {"message": "nope"}}).to_string()),
    }
}

fn openai() -> ResolvedStorage {
    ResolvedStorage {
        provider_id: "openai".to_owned(),
        kind: StorageKind::Openai,
        alias: "api.openai.com".to_owned(),
        api_version: None,
        backend_label: "openai".to_owned(),
    }
}

fn azure() -> ResolvedStorage {
    ResolvedStorage {
        provider_id: "azure".to_owned(),
        kind: StorageKind::Azure,
        alias: "res.openai.azure.com".to_owned(),
        api_version: Some("2025-03-01-preview".to_owned()),
        backend_label: "azure".to_owned(),
    }
}

fn storage(gw: &Arc<FakeGw>) -> (OagwRagStorage, SecurityContext) {
    let identity = Arc::new(ServiceIdentity::default());
    let ctx = test_ctx();
    identity.set(ctx.clone());
    let gw: Arc<dyn ServiceGatewayClientV1> = gw.clone();
    (OagwRagStorage::new(gw, identity), ctx)
}

const ATT: &str = "3b1c7e0a-5d2f-4a8e-9c61-0f4d2b7a9e13";

// -- Upload ---------------------------------------------------------------

#[tokio::test]
async fn openai_upload_multipart_has_purpose_assistants() {
    let gw = FakeGw::with(ok(200, &json!({"id": "file-abc123"})));
    let (s, ctx) = storage(&gw);
    let id = s
        .upload_file(
            &openai(),
            "chat_att.pdf",
            "application/pdf",
            Bytes::from_static(b"%PDF-1.7 body"),
        )
        .await
        .unwrap();
    assert_eq!(id, "file-abc123");
    let c = gw.last();
    assert_eq!(c.method, http::Method::POST);
    assert_eq!(c.uri, "/api.openai.com/v1/files");
    assert_eq!(c.subject_id, ctx.subject_id());
    let ct = c.content_type.clone().unwrap();
    assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
    let boundary = ct.split("boundary=").nth(1).unwrap().trim_matches('"');
    let body = c.text();
    assert!(body.contains(&format!("--{boundary}\r\n")));
    assert!(
        body.contains("name=\"purpose\"\r\n\r\nassistants\r\n"),
        "{body}"
    );
    assert!(
        body.contains("name=\"file\"; filename=\"chat_att.pdf\""),
        "{body}"
    );
    assert!(body.contains("Content-Type: application/pdf\r\n\r\n%PDF-1.7 body\r\n"));
    assert!(body.ends_with(&format!("--{boundary}--\r\n")));
}

#[tokio::test]
async fn upload_without_id_in_response_is_failed() {
    let gw = FakeGw::with(ok(200, &json!({"object": "file"})));
    let (s, _) = storage(&gw);
    let err = s
        .upload_file(&openai(), "a.txt", "text/plain", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Failed(_)), "{err:?}");
}

#[tokio::test]
async fn azure_paths_carry_api_version() {
    let az = azure();
    let v = "api-version=2025-03-01-preview";
    let cases: Vec<(&str, http::Method, String, Reply)> = vec![
        (
            "upload",
            http::Method::POST,
            format!("/res.openai.azure.com/openai/files?{v}"),
            ok(200, &json!({"id": "file-1"})),
        ),
        (
            "delete_file",
            http::Method::DELETE,
            format!("/res.openai.azure.com/openai/files/file-1?{v}"),
            ok(200, &json!({"deleted": true})),
        ),
        (
            "create_vs",
            http::Method::POST,
            format!("/res.openai.azure.com/openai/vector_stores?{v}"),
            ok(200, &json!({"id": "vs_1"})),
        ),
        (
            "add",
            http::Method::POST,
            format!("/res.openai.azure.com/openai/vector_stores/vs_1/files?{v}"),
            ok(200, &json!({"id": "file-1", "status": "in_progress"})),
        ),
        (
            "status",
            http::Method::GET,
            format!("/res.openai.azure.com/openai/vector_stores/vs_1/files/file-1?{v}"),
            ok(200, &json!({"id": "file-1", "status": "completed"})),
        ),
        (
            "delete_vs",
            http::Method::DELETE,
            format!("/res.openai.azure.com/openai/vector_stores/vs_1?{v}"),
            ok(200, &json!({"deleted": true})),
        ),
    ];
    for (name, method, uri, reply) in cases {
        let gw = FakeGw::with(reply);
        let (s, _) = storage(&gw);
        match name {
            "upload" => s
                .upload_file(&az, "a.txt", "text/plain", Bytes::from_static(b"x"))
                .await
                .map(drop),
            "delete_file" => s.delete_file(&az, "file-1").await,
            "create_vs" => s.create_vector_store(&az, "chat").await.map(drop),
            "add" => s
                .add_file_to_vector_store(&az, "vs_1", "file-1", Uuid::parse_str(ATT).unwrap())
                .await
                .map(drop),
            "status" => s
                .vector_store_file_status(&az, "vs_1", "file-1")
                .await
                .map(drop),
            _ => s.delete_vector_store(&az, "vs_1").await,
        }
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let c = gw.last();
        assert_eq!(c.method, method, "{name}");
        assert_eq!(c.uri, uri, "{name}");
    }
}

#[tokio::test]
async fn azure_without_api_version_is_failed_and_sends_nothing() {
    let gw = Arc::new(FakeGw::default());
    let (s, _) = storage(&gw);
    let mut az = azure();
    az.api_version = None;
    let err = s.delete_file(&az, "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Failed(_)), "{err:?}");
    assert!(gw.captured.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn identity_not_ready_is_unavailable_and_sends_nothing() {
    let gw = Arc::new(FakeGw::default());
    let dyn_gw: Arc<dyn ServiceGatewayClientV1> = gw.clone();
    let s = OagwRagStorage::new(dyn_gw, Arc::new(ServiceIdentity::default()));
    let err = s.delete_file(&openai(), "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");
    let err = s.delete_vector_store(&openai(), "vs_1").await.unwrap_err();
    assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");
    assert!(gw.captured.lock().unwrap().is_empty());
}

// -- Vector stores --------------------------------------------------------

#[tokio::test]
async fn openai_paths_for_vector_store_calls() {
    let gw = FakeGw::with(ok(200, &json!({"id": "vs_9"})));
    let (s, _) = storage(&gw);
    assert_eq!(
        s.create_vector_store(&openai(), "chat-1").await.unwrap(),
        "vs_9"
    );
    let c = gw.last();
    assert_eq!(c.uri, "/api.openai.com/v1/vector_stores");
    assert_eq!(c.content_type.as_deref(), Some("application/json"));
    assert_eq!(c.json(), json!({"name": "chat-1"}));

    gw.replies
        .lock()
        .unwrap()
        .push(ok(200, &json!({"status": "completed"})));
    s.vector_store_file_status(&openai(), "vs_9", "file-1")
        .await
        .unwrap();
    let c = gw.last();
    assert_eq!(c.method, http::Method::GET);
    assert_eq!(c.uri, "/api.openai.com/v1/vector_stores/vs_9/files/file-1");

    gw.replies.lock().unwrap().push(ok(200, &json!({})));
    s.delete_vector_store(&openai(), "vs_9").await.unwrap();
    let c = gw.last();
    assert_eq!(c.method, http::Method::DELETE);
    assert_eq!(c.uri, "/api.openai.com/v1/vector_stores/vs_9");
}

#[tokio::test]
async fn add_file_sends_attachment_id_attribute() {
    let gw = FakeGw::with(ok(200, &json!({"id": "file-1", "status": "in_progress"})));
    let (s, _) = storage(&gw);
    let status = s
        .add_file_to_vector_store(&openai(), "vs_9", "file-1", Uuid::parse_str(ATT).unwrap())
        .await
        .unwrap();
    assert_eq!(status, IndexStatus::InProgress);
    let c = gw.last();
    assert_eq!(c.uri, "/api.openai.com/v1/vector_stores/vs_9/files");
    assert_eq!(
        c.json(),
        json!({"file_id": "file-1", "attributes": {"attachment_id": ATT}})
    );
}

#[tokio::test]
async fn status_missing_is_in_progress_unknown_is_failed() {
    for (body, want) in [
        (json!({"id": "file-1"}), IndexStatus::InProgress),
        (json!({"status": null}), IndexStatus::InProgress),
        (json!({"status": "in_progress"}), IndexStatus::InProgress),
        (json!({"status": "completed"}), IndexStatus::Completed),
        (json!({"status": "failed"}), IndexStatus::Failed),
        (json!({"status": "cancelled"}), IndexStatus::Failed),
        (json!({"status": "weird"}), IndexStatus::Failed),
    ] {
        let gw = FakeGw::with(ok(200, &body));
        let (s, _) = storage(&gw);
        let got = s
            .vector_store_file_status(&openai(), "vs_9", "file-1")
            .await
            .unwrap();
        assert_eq!(got, want, "{body}");
    }
}

// -- Deletes and error classification -------------------------------------

#[tokio::test]
async fn delete_404_is_not_found() {
    let gw = FakeGw::with(upstream_err(404));
    let (s, _) = storage(&gw);
    assert_eq!(
        s.delete_file(&openai(), "file-gone").await.unwrap_err(),
        StorageError::NotFound
    );
    gw.replies.lock().unwrap().push(upstream_err(404));
    assert_eq!(
        s.delete_vector_store(&openai(), "vs_gone")
            .await
            .unwrap_err(),
        StorageError::NotFound
    );
}

#[tokio::test]
async fn delete_2xx_is_ok_even_with_empty_body() {
    let gw = FakeGw::with(Reply::Response {
        status: 204,
        source: None,
        body: Bytes::new(),
    });
    let (s, _) = storage(&gw);
    s.delete_file(&openai(), "file-1").await.unwrap();
}

#[tokio::test]
async fn status_codes_classify_transient_and_failed() {
    for (status, transient) in [
        (429, true),
        (500, true),
        (503, true),
        (400, false),
        (403, false),
        (422, false),
    ] {
        let gw = FakeGw::with(upstream_err(status));
        let (s, _) = storage(&gw);
        let err = s.delete_file(&openai(), "file-1").await.unwrap_err();
        if transient {
            assert!(
                matches!(err, StorageError::Transient(_)),
                "{status}: {err:?}"
            );
        } else {
            assert!(matches!(err, StorageError::Failed(_)), "{status}: {err:?}");
        }
    }
}

#[tokio::test]
async fn gateway_errors_are_transient_even_for_404() {
    // Gateway pipeline error.
    let gw = FakeGw::with(Reply::Error(
        ProxyErr::deadline_exceeded("timed out").create(),
    ));
    let (s, _) = storage(&gw);
    let err = s.delete_file(&openai(), "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Transient(_)), "{err:?}");

    // Gateway-generated 404 (no route) is not "the file is gone".
    let gw = FakeGw::with(Reply::Response {
        status: 404,
        source: Some(ErrorSource::Gateway),
        body: Bytes::new(),
    });
    let (s, _) = storage(&gw);
    let err = s.delete_file(&openai(), "file-1").await.unwrap_err();
    assert!(matches!(err, StorageError::Transient(_)), "{err:?}");
}

#[tokio::test]
async fn ids_that_could_alter_the_path_are_rejected_without_a_request() {
    let gw = Arc::new(FakeGw::default());
    let (s, _) = storage(&gw);
    for bad in ["", "../x", "a/b", "a?b=1", "a b"] {
        let err = s.delete_file(&openai(), bad).await.unwrap_err();
        assert!(matches!(err, StorageError::Failed(_)), "{bad}: {err:?}");
    }
    assert!(gw.captured.lock().unwrap().is_empty());
}
