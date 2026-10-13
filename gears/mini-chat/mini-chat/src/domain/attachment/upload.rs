//! The upload request (DESIGN "Upload Attachment", "File Upload"): checks before the body is
//! read, the multipart `file` part, per-chat limits, the provider upload and the per-kind
//! processing (indexing, code interpreter, thumbnail).

use std::sync::Arc;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use http::HeaderMap;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use opentelemetry::KeyValue;
use sea_orm::ActiveValue::{NotSet, Set};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::indexing::IndexJob;
use super::mime::{self, ResolvedMime};
use super::secondary::SecondaryCopy;
use super::thumbnail::make_thumbnail;
use super::{AttachmentService, AttachmentView};
use crate::config::RagConfig;
use crate::domain::authz::ChatAction;
use crate::domain::error::DomainError;
use crate::infra::db::entity::attachments;
use crate::infra::db::repo;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx;
use crate::infra::db::{AttachmentKind, AttachmentStatus};
use crate::infra::llm::StorageTarget;
use crate::infra::storage::FileUpload;
use crate::metrics::Metrics;

/// Longest stored filename, in characters.
const MAX_FILENAME_CHARS: usize = 255;
/// Name of the multipart part that carries the file.
const FILE_FIELD: &str = "file";
/// `error_code` of a failed provider upload.
const ERROR_UPLOAD_FAILED: &str = "upload_failed";
const BYTES_PER_KIB: u64 = 1024;
const BYTES_PER_MIB: u64 = 1024 * 1024;

impl AttachmentService {
    /// Uploads the `file` part of the multipart `body` to chat `chat_id` (DESIGN "Upload
    /// Attachment"). Returns the attachment `ready`, or `uploaded` when document indexing is
    /// still running at the request deadline (a background task keeps waiting).
    ///
    /// # Errors
    /// Before the body is read: PDP failures, `ChatNotFound`, `InvalidModel` / policy plugin
    /// failures. Then `MultipartBoundaryRequired`, `MultipartError`, `MissingFile`,
    /// `MissingContentType`, `UnsupportedContentType`, `FeatureDisabled { images }`,
    /// `CodeInterpreterUnavailable`, `UploadConcurrencyLimit`, `FileTooLarge`, `DocumentLimit`,
    /// `StorageLimit`, `ProviderMismatch`, `StorageUnavailable` (the row stays `failed`), database
    /// failures.
    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        headers: &HeaderMap,
        body: Body,
    ) -> Result<AttachmentView, DomainError> {
        let mut kind = None;
        let result = self
            .upload_inner(ctx, chat_id, headers, body, &mut kind)
            .await;
        let result_label = match &result {
            Ok(_) => "ok",
            Err(err) => result_label(err),
        };
        self.deps.metrics.attachment_upload.add(
            1,
            &[
                KeyValue::new("kind", kind.map_or("unknown", AttachmentKind::as_str)),
                KeyValue::new("result", result_label),
            ],
        );
        result
    }

    async fn upload_inner(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        headers: &HeaderMap,
        body: Body,
        kind_slot: &mut Option<AttachmentKind>,
    ) -> Result<AttachmentView, DomainError> {
        let deadline = Instant::now() + self.timings.request_deadline;
        let ((scope, tenant_id), chat) = self
            .load_chat(ctx, ChatAction::UploadAttachment, chat_id)
            .await?;
        let (snapshot, model) = self
            .deps
            .models
            .resolve_chat_model(ctx, &chat.model)
            .await?;

        let (file, _permit) = self
            .receive(headers, body, snapshot.kill_switches, &model, kind_slot)
            .await?;
        let target = self
            .deps
            .providers
            .storage_target(&model.provider_id, tenant_id)?;
        if file.mime.for_file_search {
            self.check_store_provider(&scope, chat_id, &target).await?;
        }
        let attachment_id = Uuid::new_v4();
        self.insert_pending(&PendingRow {
            scope: &scope,
            tenant_id,
            chat_id,
            attachment_id,
            user_id: ctx.subject_id(),
            file: &file,
            storage_backend: &target.storage_backend,
        })
        .await?;
        let file_id = self
            .store_file(&scope, &target, chat_id, attachment_id, &file)
            .await?;

        let job = IndexJob {
            scope: scope.clone(),
            tenant_id,
            chat_id,
            attachment_id,
            file_id,
            target,
        };
        match file.mime.kind {
            AttachmentKind::Document if file.mime.for_file_search => {
                self.index_document(job, deadline).await?;
            }
            AttachmentKind::Document => {
                self.set_ready(&scope, chat_id, attachment_id, None).await?;
            }
            AttachmentKind::Image => {
                self.copy_secondary(SecondaryCopy {
                    scope: &scope,
                    tenant_id,
                    attachment_id,
                    provider_id: &model.provider_id,
                    filename: &stored_filename(chat_id, attachment_id, &file.mime),
                    content_type: &file.mime.content_type,
                    bytes: &file.bytes,
                    deadline,
                })
                .await;
                let thumbnail = self.thumbnail(attachment_id, file.bytes).await;
                self.set_ready(&scope, chat_id, attachment_id, thumbnail)
                    .await?;
            }
        }
        self.view(&scope, chat_id, attachment_id).await
    }

    /// Reads the multipart `file` part: its filename and validated type (before the part's body
    /// is read), an upload slot, then the bytes within the size limit.
    async fn receive(
        &self,
        headers: &HeaderMap,
        body: Body,
        kill_switches: KillSwitches,
        model: &ModelCatalogEntry,
        kind_slot: &mut Option<AttachmentKind>,
    ) -> Result<(ReceivedFile, OwnedSemaphorePermit), DomainError> {
        let boundary = boundary(headers)?;
        let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
        let mut field = next_file_field(&mut multipart).await?;
        let filename = normalize_filename(field.file_name());
        let part_type = field
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mime = mime::resolve(
            part_type.as_deref(),
            &filename,
            self.deps.cfg.rag.allow_csv_upload,
        )?;
        *kind_slot = Some(mime.kind);
        let mime = filter_purposes(mime, kill_switches, model)?;

        let permit = Arc::clone(&self.uploads)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;
        let limit = size_limit(&self.deps.cfg.rag, mime.kind, model);
        let bytes = read_limited(&mut field, limit).await?;
        #[allow(clippy::cast_precision_loss)] // a histogram sample
        self.deps.metrics.attachment_upload_bytes.record(
            bytes.len() as f64,
            &[KeyValue::new("kind", mime.kind.as_str())],
        );
        Ok((
            ReceivedFile {
                filename,
                mime,
                bytes,
            },
            permit,
        ))
    }

    /// Uploads the file to the provider (`{chat_id}_{attachment_id}.{ext}`) and records it on the
    /// `pending` row (`uploaded` + provider file id); a failed upload marks the row `failed`.
    async fn store_file(
        &self,
        scope: &AccessScope,
        target: &StorageTarget,
        chat_id: Uuid,
        attachment_id: Uuid,
        file: &ReceivedFile,
    ) -> Result<String, DomainError> {
        let _pending = PendingGauge::new(&self.deps.metrics);
        let upload = FileUpload {
            filename: stored_filename(chat_id, attachment_id, &file.mime),
            content_type: file.mime.content_type.clone(),
            body: one_chunk(file.bytes.clone()),
        };
        let file_id = match self.deps.files.upload(target, upload).await {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(%attachment_id, error = %err, "provider upload failed");
                self.mark_failed(scope, attachment_id, ERROR_UPLOAD_FAILED)
                    .await;
                return Err(DomainError::StorageUnavailable(err.to_string()));
            }
        };
        let (s, id) = (scope.clone(), file_id.clone());
        let recorded = write_tx(&self.deps.db, move |tx| {
            let (scope, id) = (s.clone(), id.clone());
            Box::pin(async move {
                repo::attachments::set_uploaded(tx, &scope, attachment_id, &id, db_now()).await
            })
        })
        .await?;
        if !recorded {
            // The chat (or the row) was deleted during the provider upload: nobody else knows
            // this file id, so it is deleted here.
            tracing::warn!(%attachment_id, "attachment withdrawn during the provider upload");
            self.delete_file_best_effort(target, &file_id, attachment_id);
            return Err(withdrawn(chat_id));
        }
        Ok(file_id)
    }

    /// Deletes a provider file in the background (best effort, not retried).
    pub(super) fn delete_file_best_effort(
        &self,
        target: &StorageTarget,
        file_id: &str,
        attachment_id: Uuid,
    ) {
        let (files, target, file_id) = (
            Arc::clone(&self.deps.files),
            target.clone(),
            file_id.to_owned(),
        );
        tokio::spawn(async move {
            if let Err(err) = files.delete(&target, &file_id).await {
                tracing::warn!(%attachment_id, error = %err, "best-effort provider file delete failed");
            }
        });
    }

    /// The preview of an uploaded image, generated on a blocking thread (`None` when skipped or
    /// failed).
    async fn thumbnail(
        &self,
        attachment_id: Uuid,
        bytes: Bytes,
    ) -> Option<repo::attachments::ThumbnailColumns> {
        let cfg = self.deps.cfg.thumbnail.clone();
        let thumbnail = tokio::task::spawn_blocking(move || make_thumbnail(&bytes, &cfg))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%attachment_id, error = %err, "thumbnail task failed");
                None
            })?;
        Some((
            thumbnail.bytes,
            i32::try_from(thumbnail.width).ok()?,
            i32::try_from(thumbnail.height).ok()?,
        ))
    }

    /// 409 when the chat's vector store belongs to another storage backend (checked before the
    /// provider upload; the creation protocol checks again).
    async fn check_store_provider(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        target: &StorageTarget,
    ) -> Result<(), DomainError> {
        let conn = self.deps.db.conn()?;
        match repo::vector_stores::find(&conn, scope, chat_id).await? {
            Some(row) if row.provider != target.storage_backend => {
                Err(DomainError::ProviderMismatch)
            }
            _ => Ok(()),
        }
    }

    /// Checks the per-chat limits and inserts the `pending` row, in one transaction.
    async fn insert_pending(&self, p: &PendingRow<'_>) -> Result<(), DomainError> {
        let rag = &self.deps.cfg.rag;
        let max_documents = u64::from(rag.max_documents_per_chat);
        let max_total = i64::from(rag.max_total_upload_mb_per_chat)
            .saturating_mul(i64::try_from(BYTES_PER_MIB).unwrap_or(i64::MAX));
        let mime = &p.file.mime;
        let size = i64::try_from(p.file.bytes.len()).unwrap_or(i64::MAX);
        let is_document = mime.kind == AttachmentKind::Document;
        let now = db_now();
        let row = attachments::ActiveModel {
            id: Set(p.attachment_id),
            tenant_id: Set(p.tenant_id),
            chat_id: Set(p.chat_id),
            uploaded_by_user_id: Set(p.user_id),
            filename: Set(p.file.filename.clone()),
            content_type: Set(mime.content_type.clone()),
            size_bytes: Set(size),
            storage_backend: Set(p.storage_backend.to_owned()),
            provider_file_id: Set(None),
            status: Set(AttachmentStatus::Pending.as_str().to_owned()),
            error_code: Set(None),
            attachment_kind: Set(mime.kind.as_str().to_owned()),
            for_file_search: Set(mime.for_file_search),
            for_code_interpreter: Set(mime.for_code_interpreter),
            doc_summary: NotSet,
            img_thumbnail: NotSet,
            img_thumbnail_width: NotSet,
            img_thumbnail_height: NotSet,
            summary_model: NotSet,
            summary_updated_at: NotSet,
            cleanup_status: NotSet,
            cleanup_attempts: NotSet,
            last_cleanup_error: NotSet,
            cleanup_updated_at: NotSet,
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: NotSet,
            secondary_file_id: NotSet,
            secondary_status: NotSet,
            secondary_provider_kind: NotSet,
        };
        let (scope, chat_id) = (p.scope.clone(), p.chat_id);
        write_tx(&self.deps.db, move |tx| {
            let (scope, row) = (scope.clone(), row.clone());
            Box::pin(async move {
                // Serializes the count-and-insert of concurrent uploads to this chat.
                repo::chats::lock_live(tx, &scope, chat_id)
                    .await?
                    .ok_or_else(|| withdrawn(chat_id))?;
                let usage = repo::attachments::chat_usage(tx, &scope, chat_id).await?;
                if is_document && usage.documents >= max_documents {
                    return Err(DomainError::DocumentLimit);
                }
                if usage.total_bytes.saturating_add(size) > max_total {
                    return Err(DomainError::StorageLimit);
                }
                repo::attachments::insert(tx, &scope, row).await
            })
        })
        .await
    }

    /// Moves a `pending` / `uploaded` row to `failed`; a database failure is only logged (the
    /// upload reaper finishes the row).
    pub(super) async fn mark_failed(
        &self,
        scope: &AccessScope,
        id: Uuid,
        error_code: &'static str,
    ) {
        let scope = scope.clone();
        let res = write_tx(&self.deps.db, move |tx| {
            let scope = scope.clone();
            Box::pin(async move {
                repo::attachments::set_failed(tx, &scope, id, error_code, db_now())
                    .await
                    .map(drop)
            })
        })
        .await;
        if let Err(err) = res {
            tracing::error!(attachment_id = %id, error = %err, "failed to record the upload failure");
        }
    }

    /// Moves a live `uploaded` row to `ready` (with the image preview, if any). A row deleted
    /// meanwhile stays as it is.
    ///
    /// # Errors
    /// `ChatNotFound` when the row was deleted or claimed by the cleanup of its deleted chat
    /// meanwhile (the provider file is recorded on the row, so the cleanup deletes it); database
    /// failures.
    pub(super) async fn set_ready(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        thumbnail: Option<repo::attachments::ThumbnailColumns>,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        let set = write_tx(&self.deps.db, move |tx| {
            let (scope, thumbnail) = (scope.clone(), thumbnail.clone());
            Box::pin(async move {
                repo::attachments::set_ready(tx, &scope, id, thumbnail, db_now()).await
            })
        })
        .await?;
        if !set {
            tracing::warn!(attachment_id = %id, "attachment withdrawn before it became ready");
            return Err(withdrawn(chat_id));
        }
        Ok(())
    }

    /// The stored attachment as the API shows it.
    async fn view(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<AttachmentView, DomainError> {
        let conn = self.deps.db.conn()?;
        repo::attachments::find_in_chat(&conn, scope, chat_id, id)
            .await?
            .ok_or_else(|| DomainError::Internal(format!("attachment {id} vanished")))?
            .try_into()
    }
}

/// Provider-side filename of an attachment: `{chat_id}_{attachment_id}.{ext}`.
fn stored_filename(chat_id: Uuid, attachment_id: Uuid, mime: &ResolvedMime) -> String {
    format!("{chat_id}_{attachment_id}.{}", mime.extension())
}

/// The `file` part as received.
struct ReceivedFile {
    /// Normalized filename (stored and shown).
    filename: String,
    mime: ResolvedMime,
    bytes: Bytes,
}

/// The inputs of the `pending` row.
struct PendingRow<'a> {
    scope: &'a AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
    user_id: Uuid,
    file: &'a ReceivedFile,
    storage_backend: &'a str,
}

/// `attachments_pending` while the row is `pending` (also when the request is dropped).
struct PendingGauge(Arc<Metrics>);

impl PendingGauge {
    fn new(metrics: &Arc<Metrics>) -> Self {
        metrics.attachments_pending.add(1, &[]);
        Self(Arc::clone(metrics))
    }
}

impl Drop for PendingGauge {
    fn drop(&mut self) {
        self.0.attachments_pending.add(-1, &[]);
    }
}

/// Low-cardinality `result` label of a failed upload.
fn result_label(err: &DomainError) -> &'static str {
    match err {
        DomainError::FileTooLarge => "file_too_large",
        DomainError::UnsupportedContentType => "unsupported_content_type",
        DomainError::MultipartBoundaryRequired
        | DomainError::MultipartError(_)
        | DomainError::MissingFile
        | DomainError::MissingContentType => "invalid_request",
        DomainError::CodeInterpreterUnavailable | DomainError::FeatureDisabled { .. } => {
            "unavailable_feature"
        }
        DomainError::DocumentLimit => "document_limit",
        DomainError::StorageLimit => "storage_limit",
        DomainError::ProviderMismatch => "provider_mismatch",
        DomainError::UploadConcurrencyLimit => "concurrency_limit",
        DomainError::StorageUnavailable(_) => "storage_unavailable",
        _ => "error",
    }
}

/// The multipart boundary of the request's `Content-Type`.
fn boundary(headers: &HeaderMap) -> Result<String, DomainError> {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    multer::parse_boundary(content_type).map_err(|_| DomainError::MultipartBoundaryRequired)
}

fn multipart_error(err: &multer::Error) -> DomainError {
    DomainError::MultipartError(err.to_string())
}

/// The first part named `file`; other parts are skipped.
async fn next_file_field(
    multipart: &mut multer::Multipart<'static>,
) -> Result<multer::Field<'static>, DomainError> {
    loop {
        match multipart
            .next_field()
            .await
            .map_err(|e| multipart_error(&e))?
        {
            None => return Err(DomainError::MissingFile),
            Some(field) if field.name() == Some(FILE_FIELD) => return Ok(field),
            Some(_) => {}
        }
    }
}

/// Reads the part into memory, failing as soon as it exceeds `limit` bytes.
async fn read_limited(
    field: &mut multer::Field<'static>,
    limit: u64,
) -> Result<Bytes, DomainError> {
    let mut buf = BytesMut::new();
    while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
        let total = u64::try_from(buf.len() + chunk.len()).unwrap_or(u64::MAX);
        if total > limit {
            return Err(DomainError::FileTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// `min(rag.uploaded_{file|image}_max_size_kb, model max_file_size_mb)` in bytes.
fn size_limit(rag: &RagConfig, kind: AttachmentKind, model: &ModelCatalogEntry) -> u64 {
    let gear_kb = match kind {
        AttachmentKind::Document => rag.uploaded_file_max_size_kb,
        AttachmentKind::Image => rag.uploaded_image_max_size_kb,
    };
    let gear = u64::from(gear_kb) * BYTES_PER_KIB;
    if model.general_config.max_file_size_mb == 0 {
        tracing::warn!(model = %model.id, "catalog max_file_size_mb is 0: every upload is too large");
    }
    let model = u64::from(model.general_config.max_file_size_mb) * BYTES_PER_MIB;
    gear.min(model)
}

/// The answer to an upload whose row was deleted, or claimed by the cleanup of its deleted chat,
/// while it was being processed.
fn withdrawn(chat_id: Uuid) -> DomainError {
    DomainError::ChatNotFound {
        id: chat_id.to_string(),
    }
}

/// Rejects images while `disable_images` is on and drops the code-interpreter purpose when the
/// kill switch or the model rules it out; a document left without a purpose is rejected.
fn filter_purposes(
    mut mime: ResolvedMime,
    kill_switches: KillSwitches,
    model: &ModelCatalogEntry,
) -> Result<ResolvedMime, DomainError> {
    if mime.kind == AttachmentKind::Image {
        if kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled { subject: "images" });
        }
        return Ok(mime);
    }
    if kill_switches.disable_code_interpreter || !model.tool_support().code_interpreter {
        mime.for_code_interpreter = false;
    }
    if !mime.for_file_search && !mime.for_code_interpreter {
        return Err(DomainError::CodeInterpreterUnavailable);
    }
    Ok(mime)
}

/// A request body of one in-memory chunk.
fn one_chunk(bytes: Bytes) -> oagw_sdk::body::BodyStream {
    Box::pin(futures::stream::once(async move { Ok(bytes) }))
}

/// Filename of an upload: `"upload"` when missing or empty; longer than 255 characters it is
/// truncated on a character boundary, keeping the extension.
#[must_use]
pub fn normalize_filename(filename: Option<&str>) -> String {
    let name = filename.unwrap_or_default();
    if name.is_empty() {
        return "upload".to_owned();
    }
    if name.chars().count() <= MAX_FILENAME_CHARS {
        return name.to_owned();
    }
    // The extension (with its dot) is kept when at least one character of the stem fits.
    if let Some(dot) = name.rfind('.').filter(|&i| i > 0) {
        let (stem, ext) = name.split_at(dot);
        let ext_chars = ext.chars().count();
        if ext_chars < MAX_FILENAME_CHARS {
            let stem: String = stem.chars().take(MAX_FILENAME_CHARS - ext_chars).collect();
            return stem + ext;
        }
    }
    name.chars().take(MAX_FILENAME_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "й": two UTF-8 bytes.
    const CYRILLIC_I: &str = "\u{439}";
    /// An emoji: four UTF-8 bytes.
    const GRINNING_FACE: &str = "\u{1f600}";

    #[test]
    fn missing_or_empty_filename_is_upload() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("")), "upload");
        assert_eq!(normalize_filename(Some("a.pdf")), "a.pdf");
    }

    #[test]
    fn long_filenames_keep_their_extension() {
        let exact = format!("{}.pdf", "a".repeat(251));
        assert_eq!(
            normalize_filename(Some(&exact)),
            exact,
            "255 characters stay"
        );

        let long = format!("{}.pdf", "a".repeat(252));
        assert_eq!(
            normalize_filename(Some(&long)),
            format!("{}.pdf", "a".repeat(251))
        );

        let cyrillic = format!("{}.docx", CYRILLIC_I.repeat(300));
        let out = normalize_filename(Some(&cyrillic));
        assert_eq!(out, format!("{}.docx", CYRILLIC_I.repeat(250)));
        assert_eq!(out.chars().count(), 255);

        let emoji = format!("{}.txt", GRINNING_FACE.repeat(300));
        let out = normalize_filename(Some(&emoji));
        assert_eq!(out, format!("{}.txt", GRINNING_FACE.repeat(251)));
    }

    #[test]
    fn long_filenames_without_a_usable_extension_are_cut() {
        let plain = CYRILLIC_I.repeat(300);
        assert_eq!(normalize_filename(Some(&plain)), CYRILLIC_I.repeat(255));

        let huge_ext = format!("a.{}", "b".repeat(300));
        assert_eq!(
            normalize_filename(Some(&huge_ext)),
            format!("a.{}", "b".repeat(253))
        );
    }
}
