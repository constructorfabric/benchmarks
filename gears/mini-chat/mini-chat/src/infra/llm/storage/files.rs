//! Files API (`/files`).

use bytes::Bytes;
use http::Method;
use oagw_sdk::{MultipartBody, Part};

use super::{Payload, RagStorage, path_id};
use crate::domain::ports::StorageError;
use crate::infra::llm::provider_resolver::StorageTarget;

/// Upload purpose of every file (documents and images, `OpenAI` and Azure).
const PURPOSE: &str = "assistants";

impl RagStorage {
    pub(super) async fn do_upload_file(
        &self,
        t: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let form = MultipartBody::new().text("purpose", PURPOSE).part(
            Part::bytes("file", bytes)
                .filename(filename)
                .content_type(content_type),
        );
        let json = self
            .call(
                "file upload",
                t,
                Method::POST,
                "/files",
                Payload::Multipart(form),
                false,
            )
            .await?;
        json.get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::Permanent("file upload: response has no file id".into()))
    }

    pub(super) async fn do_delete_file(
        &self,
        t: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let path = format!("/files/{}", path_id("file", file_id)?);
        self.call(
            "file delete",
            t,
            Method::DELETE,
            &path,
            Payload::Empty,
            true,
        )
        .await
        .map(drop)
    }
}
