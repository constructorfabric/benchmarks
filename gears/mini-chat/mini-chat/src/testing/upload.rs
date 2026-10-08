//! `multipart/form-data` uploads for integration tests.

use axum::body::Body;
use axum::http::{Method, Request};
use uuid::Uuid;

use super::harness::{TestApp, TestResponse};
use super::users::TestUser;

/// Boundary of the bodies built by [`form_body`].
pub const BOUNDARY: &str = "mini-chat-test-boundary-7MA4YWxk";

/// One part of a multipart form.
#[derive(Debug, Clone)]
pub struct FormPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

impl FormPart {
    /// The `file` part.
    #[must_use]
    pub fn file(filename: Option<&str>, content_type: Option<&str>, data: &[u8]) -> Self {
        Self {
            name: "file".to_owned(),
            filename: filename.map(str::to_owned),
            content_type: content_type.map(str::to_owned),
            data: data.to_vec(),
        }
    }

    /// A plain text field.
    #[must_use]
    pub fn text(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            filename: None,
            content_type: None,
            data: value.as_bytes().to_vec(),
        }
    }
}

/// `multipart/form-data` body of `parts` with [`BOUNDARY`].
#[must_use]
pub fn form_body(parts: &[FormPart]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", p.name);
        if let Some(f) = &p.filename {
            disposition.push_str("; filename=\"");
            disposition.push_str(f);
            disposition.push('"');
        }
        out.extend_from_slice(disposition.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = &p.content_type {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&p.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// `Content-Type` header of [`form_body`] bodies.
#[must_use]
pub fn form_content_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// `/mini-chat/v1/chats/{chat}/attachments`.
#[must_use]
pub fn attachments_path(chat: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments")
}

impl TestApp {
    /// Upload one file as `user`.
    pub async fn upload(
        &self,
        user: TestUser,
        chat: Uuid,
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> TestResponse {
        let body = form_body(&[FormPart::file(Some(filename), Some(content_type), data)]);
        self.upload_raw(user, chat, Some(&form_content_type()), body)
            .await
    }

    /// `POST` a raw body with the given `Content-Type` to the chat's attachments.
    ///
    /// # Panics
    /// When the request cannot be built.
    #[allow(clippy::expect_used)]
    pub async fn upload_raw(
        &self,
        user: TestUser,
        chat: Uuid,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> TestResponse {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(attachments_path(chat));
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        let mut req = builder
            .body(Body::from(body))
            .expect("build upload request");
        req.extensions_mut().insert(user);
        TestResponse::read(self.raw(req).await).await
    }
}
