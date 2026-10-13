//! The steps of an upload after the file was read (DESIGN "File Upload" sequence).

use tracing::{debug, warn};
use uuid::Uuid;

use super::indexing::Indexing;
use super::model::{
    AttachmentDetail, AttachmentKind, ChatLimits, ERROR_INDEXING_FAILED, ERROR_UPLOAD_FAILED,
    ERROR_VECTOR_STORE_FAILED, IncomingFile, NewAttachment, ReadyUpdate, SecondaryCopy,
};
use super::ports::{FileUpload, IndexingStatus, StorageRoute};
use super::service::{Admission, AttachmentService, UploadSession};
use super::thumbnail;
use crate::domain::clock;
use crate::domain::error::DomainError;

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// The provider-side filename: `{chat_id}_{attachment_id}.{ext}`.
fn provider_filename(chat_id: Uuid, attachment_id: Uuid, extension: &str) -> String {
    format!("{chat_id}_{attachment_id}.{extension}")
}

impl AttachmentService {
    /// The upload of a validated, buffered file. Everything after the insert marks the row
    /// `failed` with its `error_code` before the error is returned.
    pub(super) async fn run_upload(
        &self,
        session: &UploadSession,
        admission: &Admission,
        file: &IncomingFile,
    ) -> Result<AttachmentDetail, DomainError> {
        let id = Uuid::new_v4();
        let new = NewAttachment {
            id,
            tenant_id: session.ctx.subject_tenant_id(),
            chat_id: session.chat_id,
            uploaded_by: session.ctx.subject_id(),
            filename: admission.filename.clone(),
            content_type: admission.content_type.clone(),
            size_bytes: i64::try_from(file.size).unwrap_or(i64::MAX),
            storage_backend: session.storage.primary.backend.clone(),
            kind: admission.kind,
            purposes: admission.purposes,
            now: clock::now(),
        };
        let limits = ChatLimits {
            max_documents: u64::from(self.rag.max_documents_per_chat),
            max_total_bytes: u64::from(self.rag.max_total_upload_mb_per_chat) * BYTES_PER_MIB,
        };
        self.repo
            .insert_pending(&session.scope, new, limits)
            .await?;
        self.metrics.attachments_pending(1);
        self.metrics.attachment_upload_bytes(
            admission.kind.as_str(),
            f64::from(u32::try_from(file.size).unwrap_or(u32::MAX)),
        );
        let provider_file = self.upload_primary(session, admission, file, id).await?;
        self.finish(session, admission, file, id, &provider_file)
            .await?;
        self.repo
            .find(&session.scope, session.chat_id, id)
            .await?
            .map(AttachmentDetail::from)
            .ok_or(DomainError::AttachmentNotFound { attachment_id: id })
    }

    /// The Files API upload and the `pending` to `uploaded` transition.
    async fn upload_primary(
        &self,
        session: &UploadSession,
        admission: &Admission,
        file: &IncomingFile,
        id: Uuid,
    ) -> Result<String, DomainError> {
        let upload = FileUpload {
            filename: provider_filename(session.chat_id, id, admission.extension),
            content_type: admission.content_type.clone(),
            chunks: file.chunks.clone(),
        };
        let uploaded = self
            .storage
            .upload_file(&session.ctx, &session.storage.primary, upload)
            .await;
        self.metrics.attachments_pending(-1);
        let provider_file = match uploaded {
            Ok(provider_file) => provider_file,
            Err(err) => {
                warn!(attachment_id = %id, error = %err, "the provider file upload failed");
                self.abort_upload(session, id, ERROR_UPLOAD_FAILED, None)
                    .await;
                return Err(DomainError::StorageUnavailable);
            }
        };
        match self
            .repo
            .mark_uploaded(&session.scope, id, &provider_file, clock::now())
            .await
        {
            Ok(true) => Ok(provider_file),
            Ok(false) => {
                // The row was reaped or deleted while the file was uploading: the file is debt of
                // nobody, so it is removed right away.
                warn!(attachment_id = %id, "the attachment left `pending` during its upload");
                self.delete_provider_file_later(session, provider_file);
                Err(DomainError::AttachmentNotFound { attachment_id: id })
            }
            Err(err) => {
                self.abort_upload(session, id, ERROR_UPLOAD_FAILED, Some(provider_file))
                    .await;
                Err(err)
            }
        }
    }

    /// The purpose-specific steps after the provider upload.
    async fn finish(
        &self,
        session: &UploadSession,
        admission: &Admission,
        file: &IncomingFile,
        id: Uuid,
        provider_file: &str,
    ) -> Result<(), DomainError> {
        if admission.kind == AttachmentKind::Image {
            return self
                .finish_image(session, admission, file, id, provider_file)
                .await;
        }
        if admission.purposes.file_search {
            return self.index_document(session, id, provider_file).await;
        }
        self.mark_ready(session, id, &ReadyUpdate::plain(), provider_file)
            .await
    }

    /// The thumbnail and the secondary copy of an image, then `ready`.
    async fn finish_image(
        &self,
        session: &UploadSession,
        admission: &Admission,
        file: &IncomingFile,
        id: Uuid,
        provider_file: &str,
    ) -> Result<(), DomainError> {
        let chunks = file.chunks.clone();
        let size = file.size;
        let config = self.thumbnail.clone();
        let generated =
            tokio::task::spawn_blocking(move || thumbnail::generate(&chunks, size, &config)).await;
        let thumbnail = generated.unwrap_or_else(|err| {
            warn!(error = %err, "the thumbnail task failed");
            None
        });
        let secondary = self.secondary_copy(session, admission, file, id).await;
        self.mark_ready(
            session,
            id,
            &ReadyUpdate {
                thumbnail,
                secondary,
            },
            provider_file,
        )
        .await
    }

    /// The Anthropic Files API copy of an image of an Anthropic chat. Images above
    /// `thumbnail.max_decode_bytes` get none (their bytes are not processed in memory); a failed
    /// copy is recorded and does not fail the upload.
    async fn secondary_copy(
        &self,
        session: &UploadSession,
        admission: &Admission,
        file: &IncomingFile,
        id: Uuid,
    ) -> SecondaryCopy {
        let Some(route) = &session.storage.secondary else {
            return SecondaryCopy::NotAttempted;
        };
        if file.size > u64::try_from(self.thumbnail.max_decode_bytes).unwrap_or(u64::MAX) {
            return SecondaryCopy::NotAttempted;
        }
        let upload = FileUpload {
            filename: provider_filename(session.chat_id, id, admission.extension),
            content_type: admission.content_type.clone(),
            chunks: file.chunks.clone(),
        };
        match self
            .storage
            .upload_secondary(&session.ctx, route, upload)
            .await
        {
            Ok(file_id) => SecondaryCopy::Uploaded { file_id },
            Err(err) => {
                warn!(attachment_id = %id, error = %err, "the secondary image copy failed");
                SecondaryCopy::Failed
            }
        }
    }

    /// The vector store, the add-file call and the indexing wait of a document.
    async fn index_document(
        &self,
        session: &UploadSession,
        id: Uuid,
        provider_file: &str,
    ) -> Result<(), DomainError> {
        let vector_store = match self.resolve_vector_store(session).await {
            Ok(vector_store) => vector_store,
            Err(err) => {
                self.abort_with_file(session, id, ERROR_VECTOR_STORE_FAILED, provider_file)
                    .await;
                return Err(err);
            }
        };
        let status = self
            .add_to_store(session, id, &vector_store, provider_file)
            .await?;
        let waited = self
            .await_indexing(session, &vector_store, provider_file, status)
            .await;
        match waited {
            Indexing::Completed => {
                self.mark_ready(session, id, &ReadyUpdate::plain(), provider_file)
                    .await
            }
            Indexing::Failed => {
                self.abort_with_file(session, id, ERROR_INDEXING_FAILED, provider_file)
                    .await;
                Err(DomainError::StorageUnavailable)
            }
            Indexing::Running => {
                debug!(attachment_id = %id, "indexing is still running at the request deadline");
                self.spawn_background_indexing(session, id, vector_store, provider_file.to_owned());
                Ok(())
            }
        }
    }

    /// Adds the file to the vector store; a failure fails the row with `indexing_failed`.
    async fn add_to_store(
        &self,
        session: &UploadSession,
        id: Uuid,
        vector_store: &str,
        provider_file: &str,
    ) -> Result<IndexingStatus, DomainError> {
        let added = self
            .storage
            .add_file_to_store(
                &session.ctx,
                &session.storage.primary,
                vector_store,
                provider_file,
                id,
            )
            .await;
        match added {
            Ok(status) => Ok(status),
            Err(err) => {
                warn!(attachment_id = %id, error = %err, "adding the file to the vector store failed");
                self.abort_with_file(session, id, ERROR_INDEXING_FAILED, provider_file)
                    .await;
                Err(DomainError::StorageUnavailable)
            }
        }
    }

    async fn abort_with_file(
        &self,
        session: &UploadSession,
        id: Uuid,
        error_code: &str,
        provider_file: &str,
    ) {
        self.abort_upload(session, id, error_code, Some(provider_file.to_owned()))
            .await;
    }

    /// `uploaded` to `ready`; a row that no longer qualifies (deleted, reaped, owned by chat
    /// cleanup) is gone for the caller. The provider file and a just uploaded secondary copy are
    /// then removed best effort: the cleanup that owns the row never learns of the secondary copy
    /// and may have been queued before the file id was stored. A delete of a file the cleanup
    /// already removed is a 404, which counts as success.
    async fn mark_ready(
        &self,
        session: &UploadSession,
        id: Uuid,
        update: &ReadyUpdate,
        provider_file: &str,
    ) -> Result<(), DomainError> {
        match self
            .repo
            .mark_ready(&session.scope, id, update, clock::now())
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                warn!(attachment_id = %id, "the attachment left `uploaded` before it became ready");
                self.delete_provider_file_later(session, provider_file.to_owned());
                if let SecondaryCopy::Uploaded { file_id } = &update.secondary {
                    self.delete_secondary_file_later(session, file_id.clone());
                }
                Err(DomainError::AttachmentNotFound { attachment_id: id })
            }
            Err(err) => Err(err),
        }
    }

    /// Deletes a secondary copy in a tracked background task; a failure is only logged.
    fn delete_secondary_file_later(&self, session: &UploadSession, file_id: String) {
        let Some(route) = session.storage.secondary.clone() else {
            return;
        };
        let storage = std::sync::Arc::clone(&self.storage);
        let ctx = session.ctx.clone();
        self.tasks.spawn(async move {
            if let Err(err) = storage.delete_secondary(&ctx, &route.alias, &file_id).await {
                warn!(error = %err, "the secondary copy of a gone attachment could not be deleted");
            }
        });
    }

    /// Marks the row `failed` and, when the provider holds a file for it, deletes that file best
    /// effort (fire and forget, not retried).
    async fn abort_upload(
        &self,
        session: &UploadSession,
        id: Uuid,
        error_code: &str,
        provider_file: Option<String>,
    ) {
        if let Err(err) = self
            .repo
            .mark_failed(&session.scope, id, error_code, clock::now())
            .await
        {
            warn!(attachment_id = %id, error = %err, "the failed upload could not be recorded");
        }
        if let Some(provider_file) = provider_file {
            self.delete_provider_file_later(session, provider_file);
        }
    }

    /// Deletes a provider file in a tracked background task; a failure is only logged.
    fn delete_provider_file_later(&self, session: &UploadSession, provider_file: String) {
        let storage = std::sync::Arc::clone(&self.storage);
        let ctx = session.ctx.clone();
        let route: StorageRoute = session.storage.primary.clone();
        self.tasks.spawn(async move {
            if let Err(err) = storage.delete_file(&ctx, &route, &provider_file).await {
                warn!(error = %err, "the provider file of a failed upload could not be deleted");
            }
        });
    }
}
