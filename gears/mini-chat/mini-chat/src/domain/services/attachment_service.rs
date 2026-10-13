//! Attachments: upload, get, delete (DESIGN section 3.3 "Get / Upload
//! Attachment", section 3.6 "File Upload", section 4 "Attachment Deletion").
//!
//! The upload is synchronous within the request: the row is inserted
//! `pending` (per-chat limits checked in the same transaction, write first),
//! the file goes to the provider (`uploaded`), then by purpose: a document
//! with `file_search` is added to the chat vector store and polled until 25 s
//! after the upload started (then a background task takes over), an XLSX
//! (code interpreter only) is `ready` at once, an image gets a thumbnail.

mod content;
mod indexing;

use std::sync::Arc;

use bytes::Bytes;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use time::OffsetDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use self::content::ContentType;
pub use self::indexing::IndexingTiming;
use self::indexing::{ERROR_INDEXING_FAILED, IndexTarget, Indexer, SyncOutcome};
use crate::config::MiniChatConfig;
use crate::domain::background::Background;
use crate::domain::enums::{AttachmentKind, AttachmentStatus};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, ChatAction, RagStorage, StorageError};
use crate::domain::services::message_service::ThumbnailView;
use crate::domain::services::model_service::ModelService;
use crate::domain::time::db_now;
use crate::infra::db::entities::{attachment, chat};
use crate::infra::db::repos::attachment_repo::{self, NewAttachment};
use crate::infra::db::repos::{chat_repo, tenant_scope, vector_store_repo};
use crate::infra::llm::{ProviderResolver, ResolvedStorage};
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;
use crate::infra::thumbnail::make_thumbnail;

const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";
const EVENT_DELETED: &str = "attachment_deleted";
const ERROR_UPLOAD_FAILED: &str = "upload_failed";
const ERROR_VECTOR_STORE_FAILED: &str = "vector_store_failed";
const BYTES_PER_KB: u64 = 1024;
const BYTES_PER_MB: u64 = 1024 * 1024;

/// Everything the upload needs, resolved before the body is read.
pub struct UploadPlan {
    pub chat: chat::Model,
    pub model: ModelCatalogEntry,
    pub kill_switches: KillSwitches,
    pub doc_limit_bytes: u64,
    pub image_limit_bytes: u64,
    pub code_interpreter_available: bool,
    pub storage: ResolvedStorage,
    started: Instant,
    _permit: OwnedSemaphorePermit,
}

impl UploadPlan {
    /// Size limit for a part with this filename and content type: the image
    /// limit for an image type, else the document limit.
    #[must_use]
    pub fn size_limit(&self, filename: Option<&str>, content_type: &str) -> u64 {
        match content::resolve(filename, content_type, true).map(ContentType::kind) {
            Some(AttachmentKind::Image) => self.image_limit_bytes,
            _ => self.doc_limit_bytes,
        }
    }
}

/// The `file` part of the multipart body.
#[derive(Clone, Debug)]
pub struct UploadedFile {
    pub filename: Option<String>,
    pub content_type: String,
    pub bytes: Bytes,
}

/// An attachment as the API shows it (`AttachmentDetail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentView {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: AttachmentStatus,
    pub kind: AttachmentKind,
    /// Only for `failed`.
    pub error_code: Option<String>,
    /// Only for a `ready` image.
    pub img_thumbnail: Option<ThumbnailView>,
    pub created_at: OffsetDateTime,
}

pub struct AttachmentDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub cfg: Arc<MiniChatConfig>,
    pub authz: Arc<dyn AuthzPort>,
    pub models: Arc<ModelService>,
    pub resolver: Arc<ProviderResolver>,
    pub storage: Arc<dyn RagStorage>,
    pub outbox: Arc<MiniChatOutbox>,
    pub background: Background,
}

pub struct AttachmentService {
    db: Arc<DBProvider<DomainError>>,
    cfg: Arc<MiniChatConfig>,
    authz: Arc<dyn AuthzPort>,
    models: Arc<ModelService>,
    resolver: Arc<ProviderResolver>,
    storage: Arc<dyn RagStorage>,
    outbox: Arc<MiniChatOutbox>,
    indexer: Indexer,
    uploads: Arc<Semaphore>,
}

/// How an accepted file is processed after the provider upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    FileSearch,
    CodeInterpreter,
    Image,
}

/// A validated upload, ready to insert.
struct Prepared {
    row: NewAttachment,
    content: ContentType,
    route: Route,
    bytes: Bytes,
}

impl AttachmentService {
    #[must_use]
    pub fn new(deps: AttachmentDeps) -> Self {
        let permits = usize::from(deps.cfg.rag.max_concurrent_uploads.max(1));
        let indexer = Indexer {
            db: Arc::clone(&deps.db),
            storage: Arc::clone(&deps.storage),
            outbox: Arc::clone(&deps.outbox),
            background: deps.background,
            timing: IndexingTiming::default(),
        };
        Self {
            db: deps.db,
            cfg: deps.cfg,
            authz: deps.authz,
            models: deps.models,
            resolver: deps.resolver,
            storage: deps.storage,
            outbox: deps.outbox,
            indexer,
            uploads: Arc::new(Semaphore::new(permits)),
        }
    }

    /// Replaces the indexing waits (tests run them scaled down).
    #[cfg(test)]
    pub(crate) fn with_timing(mut self, timing: IndexingTiming) -> Self {
        self.indexer.timing = timing;
        self
    }

    /// Pre-body checks of an upload: authorization, the chat (404), its model
    /// (resolved without the enabled filter; `InvalidModel` when gone from the
    /// catalog), the storage provider, and an upload concurrency permit held
    /// by the returned plan.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat), `InvalidModel`,
    /// `ProviderResolution`, `UploadConcurrency`, policy plugin or database
    /// failure.
    pub async fn begin_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<UploadPlan, DomainError> {
        let started = Instant::now();
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::UploadAttachment, Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        let chat = chat_repo::find_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        let (snapshot, model) = self
            .models
            .resolve_for_chat(ctx.subject_id(), &chat.model, false)
            .await?;
        let storage = self
            .resolver
            .resolve_storage(&model.provider_id, chat.tenant_id)?;
        let permit = Arc::clone(&self.uploads)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrency)?;
        let rag = &self.cfg.rag;
        let model_limit = u64::from(model.general_config.max_file_size_mb) * BYTES_PER_MB;
        let kill_switches = snapshot.kill_switches;
        let code_interpreter_available = !kill_switches.disable_code_interpreter
            && model.general_config.tool_support.code_interpreter;
        let doc_limit_bytes =
            (u64::from(rag.uploaded_file_max_size_kb) * BYTES_PER_KB).min(model_limit);
        let image_limit_bytes =
            (u64::from(rag.uploaded_image_max_size_kb) * BYTES_PER_KB).min(model_limit);
        Ok(UploadPlan {
            chat,
            model,
            kill_switches,
            doc_limit_bytes,
            image_limit_bytes,
            code_interpreter_available,
            storage,
            started,
            _permit: permit,
        })
    }

    /// Stores the file: validation, the `pending` row with the per-chat
    /// limits, the provider upload and the purpose-specific step. Returns the
    /// attachment as stored (`ready`, or `uploaded` while indexing goes on in
    /// the background).
    ///
    /// # Errors
    /// `UnsupportedContentType`, `FileTooLarge`, `FeatureDisabled { images }`,
    /// `CodeInterpreterUnavailable`, `ProviderMismatch`, `DocumentLimit`,
    /// `StorageLimit`, `StorageUnavailable` (provider upload, vector store or
    /// indexing failure; the row stays `failed`), database failure.
    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        plan: UploadPlan,
        file: UploadedFile,
    ) -> Result<AttachmentView, DomainError> {
        let prepared = self.prepare(ctx, &plan, file)?;
        if prepared.route == Route::FileSearch {
            // Cheap pre-check; the creation protocol re-checks under races.
            let conn = self.db.conn()?;
            if let Some(row) =
                vector_store_repo::find(&conn, plan.chat.tenant_id, plan.chat.id).await?
                && row.provider != plan.storage.backend_label
            {
                return Err(DomainError::ProviderMismatch);
            }
        }
        self.insert_with_limits(&prepared).await?;
        let row = &prepared.row;
        let file_id = self.upload_to_provider(&plan.storage, &prepared).await?;
        let conn = self.db.conn()?;
        if attachment_repo::set_uploaded(&conn, row.tenant_id, row.id, &file_id, db_now()).await?
            == 0
        {
            return Err(self.upload_superseded(&plan.storage, row, &file_id).await);
        }

        match prepared.route {
            Route::Image => {
                let thumbnail = self.thumbnail(prepared.bytes.clone()).await;
                attachment_repo::set_ready(&conn, row.tenant_id, row.id, thumbnail, db_now())
                    .await?;
            }
            Route::CodeInterpreter => {
                attachment_repo::set_ready(&conn, row.tenant_id, row.id, None, db_now()).await?;
            }
            Route::FileSearch => self.index_document(&plan, row, file_id).await?,
        }
        let stored = attachment_repo::find_in_chat(&conn, row.tenant_id, row.chat_id, row.id)
            .await?
            .ok_or_else(|| DomainError::Internal(format!("attachment {} vanished", row.id)))?;
        attachment_view(stored)
    }

    /// Provider upload of a `pending` row (`{chat_id}_{attachment_id}.{ext}`);
    /// a failure marks the row `failed`.
    async fn upload_to_provider(
        &self,
        st: &ResolvedStorage,
        prepared: &Prepared,
    ) -> Result<String, DomainError> {
        let row = &prepared.row;
        let provider_name = format!("{}_{}.{}", row.chat_id, row.id, prepared.content.ext);
        match self
            .storage
            .upload_file(
                st,
                &provider_name,
                prepared.content.mime,
                prepared.bytes.clone(),
            )
            .await
        {
            Ok(id) => Ok(id),
            Err(e) => {
                tracing::warn!(attachment_id = %row.id, error = %e, "provider file upload failed");
                self.mark_failed(row, ERROR_UPLOAD_FAILED).await;
                Err(DomainError::StorageUnavailable)
            }
        }
    }

    /// Validation before anything is written.
    fn prepare(
        &self,
        ctx: &SecurityContext,
        plan: &UploadPlan,
        file: UploadedFile,
    ) -> Result<Prepared, DomainError> {
        let content = content::resolve(
            file.filename.as_deref(),
            &file.content_type,
            self.cfg.rag.allow_csv_upload,
        )
        .ok_or(DomainError::UnsupportedContentType)?;
        let kind = content.kind();
        let limit = match kind {
            AttachmentKind::Image => plan.image_limit_bytes,
            AttachmentKind::Document => plan.doc_limit_bytes,
        };
        let size = u64::try_from(file.bytes.len()).unwrap_or(u64::MAX);
        if size > limit {
            return Err(DomainError::FileTooLarge);
        }
        let route = match kind {
            AttachmentKind::Image => {
                if plan.kill_switches.disable_images {
                    return Err(DomainError::FeatureDisabled { subject: "images" });
                }
                Route::Image
            }
            AttachmentKind::Document if content.code_interpreter_only() => {
                if !plan.code_interpreter_available {
                    return Err(DomainError::CodeInterpreterUnavailable);
                }
                Route::CodeInterpreter
            }
            AttachmentKind::Document => Route::FileSearch,
        };
        let row = NewAttachment {
            id: Uuid::new_v4(),
            tenant_id: plan.chat.tenant_id,
            chat_id: plan.chat.id,
            uploaded_by_user_id: ctx.subject_id(),
            filename: content::normalize_filename(file.filename.as_deref()),
            content_type: content.mime.to_owned(),
            size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
            storage_backend: plan.storage.backend_label.clone(),
            kind,
            for_file_search: route == Route::FileSearch,
            for_code_interpreter: route == Route::CodeInterpreter,
        };
        Ok(Prepared {
            row,
            content,
            route,
            bytes: file.bytes,
        })
    }

    /// Inserts the `pending` row, then checks the per-chat limits in the same
    /// transaction (write first, Ruling R5); a violation rolls back.
    async fn insert_with_limits(&self, p: &Prepared) -> Result<(), DomainError> {
        let row = p.row.clone();
        let max_documents = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_bytes = i64::from(self.cfg.rag.max_total_upload_mb_per_chat)
            .saturating_mul(i64::try_from(BYTES_PER_MB).unwrap_or(i64::MAX));
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    attachment_repo::insert_pending(tx, &row, db_now()).await?;
                    let usage = attachment_repo::chat_usage(tx, row.tenant_id, row.chat_id).await?;
                    if row.kind == AttachmentKind::Document && usage.documents > max_documents {
                        return Err(DomainError::DocumentLimit);
                    }
                    if usage.total_bytes > max_bytes {
                        return Err(DomainError::StorageLimit);
                    }
                    Ok(())
                })
            })
            .await
    }

    /// Vector store, add, poll; background task when still indexing at the
    /// deadline.
    async fn index_document(
        &self,
        plan: &UploadPlan,
        row: &NewAttachment,
        file_id: String,
    ) -> Result<(), DomainError> {
        let vector_store_id = match self
            .indexer
            .chat_vector_store(row.tenant_id, row.chat_id, &plan.storage)
            .await
        {
            Ok(vs) => vs,
            Err(e) => {
                self.mark_failed(row, ERROR_VECTOR_STORE_FAILED).await;
                self.indexer.delete_file_best_effort(&plan.storage, file_id);
                return Err(e);
            }
        };
        let target = IndexTarget {
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            attachment_id: row.id,
            provider_file_id: file_id,
            vector_store_id,
            storage: plan.storage.clone(),
        };
        match self.indexer.index(&target, plan.started).await {
            SyncOutcome::Ready => {
                let conn = self.db.conn()?;
                attachment_repo::set_ready(&conn, row.tenant_id, row.id, None, db_now()).await?;
            }
            SyncOutcome::Failed => {
                self.mark_failed(row, ERROR_INDEXING_FAILED).await;
                self.indexer
                    .delete_file_best_effort(&plan.storage, target.provider_file_id);
                return Err(DomainError::StorageUnavailable);
            }
            SyncOutcome::Pending => {
                tracing::info!(attachment_id = %row.id, "indexing still running at the deadline; continuing in the background");
                self.indexer.spawn_background(target);
            }
        }
        Ok(())
    }

    /// The chat or the attachment was deleted during the provider upload, so
    /// the row was not marked `uploaded` and cleanup never learns the new
    /// file id: delete the file (best effort) and fail the upload with the
    /// 404 of whatever is gone.
    async fn upload_superseded(
        &self,
        st: &ResolvedStorage,
        row: &NewAttachment,
        file_id: &str,
    ) -> DomainError {
        tracing::info!(attachment_id = %row.id, "attachment deleted during the provider upload; deleting the new file");
        if let Err(e) = self.storage.delete_file(st, file_id).await
            && e != StorageError::NotFound
        {
            tracing::warn!(attachment_id = %row.id, error = %e, "best-effort provider file delete failed");
        }
        let chat_live = match self.db.conn() {
            Ok(conn) => chat_repo::find_scoped(&conn, &tenant_scope(row.tenant_id), row.chat_id)
                .await
                .map(|c| c.is_some()),
            Err(e) => Err(e),
        };
        match chat_live {
            Ok(true) => attachment_not_found(),
            Ok(false) => DomainError::NotFound {
                resource: ResourceKind::Chat,
            },
            Err(e) => e,
        }
    }

    /// Best effort: the client gets the original error either way.
    async fn mark_failed(&self, row: &NewAttachment, error_code: &str) {
        let res = match self.db.conn() {
            Ok(conn) => {
                attachment_repo::set_failed(&conn, row.tenant_id, row.id, error_code, db_now())
                    .await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            tracing::error!(attachment_id = %row.id, error = %e, "could not mark the attachment failed");
        }
    }

    /// Best-effort thumbnail (off the async workers).
    async fn thumbnail(&self, bytes: Bytes) -> Option<attachment_repo::ThumbnailColumns> {
        let cfg = self.cfg.thumbnail.clone();
        let made = tokio::task::spawn_blocking(move || make_thumbnail(&bytes, &cfg))
            .await
            .ok()
            .flatten()?;
        let w = i32::try_from(made.width).ok()?;
        let h = i32::try_from(made.height).ok()?;
        Some((made.webp, w, h))
    }

    /// One live attachment of the caller in the chat. Another uploader's
    /// attachment is indistinguishable from an unknown id.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat or attachment), database
    /// failure, `Internal` for an unknown stored enum value.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentView, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::ReadAttachment, Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        let chat = chat_repo::find_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        let row = attachment_repo::find_in_chat(&conn, chat.tenant_id, chat.id, attachment_id)
            .await?
            .filter(|r| r.deleted_at.is_none() && r.uploaded_by_user_id == ctx.subject_id())
            .ok_or_else(attachment_not_found)?;
        attachment_view(row)
    }

    /// Soft-deletes the caller's attachment unless a message references it,
    /// enqueueing the provider cleanup in the same transaction. Deleting an
    /// already deleted attachment succeeds without a new event; the uploader
    /// check comes first.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat or attachment),
    /// `AttachmentLocked`, outbox or database failure.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::DeleteAttachment, Some(chat_id))
            .await?;
        let chat = {
            let conn = self.db.conn()?;
            chat_repo::find_scoped(&conn, &scope, chat_id)
                .await?
                .ok_or(DomainError::NotFound {
                    resource: ResourceKind::Chat,
                })?
        };
        let (tenant_id, user_id) = (chat.tenant_id, ctx.subject_id());
        let outbox = Arc::clone(&self.outbox);
        let wake = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    // Write first (Ruling R5); the guarded UPDATE is the check.
                    let now = db_now();
                    let deleted = attachment_repo::soft_delete_unreferenced(
                        tx,
                        tenant_id,
                        chat_id,
                        attachment_id,
                        user_id,
                        now,
                    )
                    .await?;
                    let row = attachment_repo::find_in_chat(tx, tenant_id, chat_id, attachment_id)
                        .await?
                        .filter(|r| r.uploaded_by_user_id == user_id)
                        .ok_or_else(attachment_not_found)?;
                    if deleted == 0 {
                        if row.deleted_at.is_some() {
                            return Ok(None);
                        }
                        if attachment_repo::is_referenced(tx, tenant_id, chat_id, attachment_id)
                            .await?
                        {
                            return Err(DomainError::AttachmentLocked);
                        }
                        return Err(DomainError::Internal(format!(
                            "attachment {attachment_id} delete matched no row"
                        )));
                    }
                    let payload = AttachmentCleanupPayload {
                        event_type: EVENT_DELETED.to_owned(),
                        tenant_id,
                        chat_id,
                        attachment_id,
                        provider_file_id: row.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: row.attachment_kind.clone(),
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    Ok(Some(outbox.enqueue_attachment_cleanup(tx, &payload).await?))
                })
            })
            .await?;
        if let Some(wake) = wake {
            wake.fire();
        }
        Ok(())
    }
}

fn attachment_not_found() -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Attachment,
    }
}

fn attachment_view(a: attachment::Model) -> Result<AttachmentView, DomainError> {
    let kind = AttachmentKind::parse(&a.attachment_kind).ok_or_else(|| {
        DomainError::Internal(format!(
            "attachment {} has kind {}",
            a.id, a.attachment_kind
        ))
    })?;
    let status = AttachmentStatus::parse(&a.status).ok_or_else(|| {
        DomainError::Internal(format!("attachment {} has status {}", a.id, a.status))
    })?;
    let img_thumbnail = match (
        kind,
        status,
        a.img_thumbnail,
        a.img_thumbnail_width,
        a.img_thumbnail_height,
    ) {
        (AttachmentKind::Image, AttachmentStatus::Ready, Some(data), Some(width), Some(height)) => {
            Some(ThumbnailView {
                content_type: THUMBNAIL_CONTENT_TYPE,
                width,
                height,
                data,
            })
        }
        _ => None,
    };
    Ok(AttachmentView {
        id: a.id,
        filename: a.filename,
        content_type: a.content_type,
        size_bytes: a.size_bytes,
        status,
        kind,
        error_code: a.error_code.filter(|_| status == AttachmentStatus::Failed),
        img_thumbnail,
        created_at: a.created_at,
    })
}

#[cfg(test)]
#[path = "attachment_service_tests.rs"]
mod attachment_service_tests;
