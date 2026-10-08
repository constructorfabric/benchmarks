//! Vector Stores API (`/vector_stores`).

use http::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Payload, RagStorage, path_id};
use crate::domain::ports::{IndexStatus, StorageError};
use crate::infra::llm::provider_resolver::StorageTarget;

/// A response without `status` counts as in progress; anything other than
/// `in_progress` / `completed` (`failed`, `cancelled`, unknown) as failed.
fn index_status(json: &Value) -> IndexStatus {
    match json.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(_) => IndexStatus::Failed,
    }
}

impl RagStorage {
    pub(super) async fn do_create_vector_store(
        &self,
        t: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError> {
        let body = json!({ "name": format!("chat-{chat_id}") });
        let json = self
            .call(
                "vector store create",
                t,
                Method::POST,
                "/vector_stores",
                Payload::Json(body),
                false,
            )
            .await?;
        json.get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                StorageError::Permanent("vector store create: response has no id".into())
            })
    }

    pub(super) async fn do_add_file(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let path = format!("/vector_stores/{}/files", path_id("vector store", vs)?);
        let body = json!({
            "file_id": file_id,
            "attributes": { "attachment_id": attachment_id.to_string() },
        });
        let json = self
            .call(
                "vector store file add",
                t,
                Method::POST,
                &path,
                Payload::Json(body),
                false,
            )
            .await?;
        Ok(index_status(&json))
    }

    pub(super) async fn do_file_status(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let path = format!(
            "/vector_stores/{}/files/{}",
            path_id("vector store", vs)?,
            path_id("file", file_id)?
        );
        let json = self
            .call(
                "vector store file status",
                t,
                Method::GET,
                &path,
                Payload::Empty,
                false,
            )
            .await?;
        Ok(index_status(&json))
    }

    pub(super) async fn do_delete_vector_store(
        &self,
        t: &StorageTarget,
        vs: &str,
    ) -> Result<(), StorageError> {
        let path = format!("/vector_stores/{}", path_id("vector store", vs)?);
        self.call(
            "vector store delete",
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
