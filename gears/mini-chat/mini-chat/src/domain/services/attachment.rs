//! Attachments (DESIGN §3.3 "Upload/Get/Delete Attachment", §3.6 "File Upload",
//! §3.7 `attachments` / `chat_vector_stores`, §4 "Attachment Deletion", ADR-0007).
//!
//! Upload is two-step: the handler calls [`AttachmentService::prepare_upload`]
//! (authorization, chat, model, limits, concurrency permit) before it reads the
//! body, validates the `file` part with [`UploadContext::validate_part`] from
//! the part headers, streams the bytes under [`PartMeta::limit_bytes`] and then
//! calls [`AttachmentService::upload`].
//!
//! In a chat whose model is served by an `anthropic_messages` provider, an
//! uploaded image also gets a secondary copy in the Anthropic Files API
//! (DESIGN §2.2 "File storage (P1)"): best effort, recorded in the
//! `secondary_*` columns; images above `thumbnail.max_decode_bytes` get none.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use tracing::{info, warn};
use uuid::Uuid;

use super::chat::ChatService;
use super::model_catalog::ModelCatalogService;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::mime::{
    MimeError, attachment_kind, extension_for, normalize_filename, purposes, resolve_content_type,
};
use crate::domain::model::{AttachmentKind, AttachmentStatus, error_codes};
use crate::infra::db::entities::{attachment, chat};
use crate::infra::db::repos::attachment::ImgThumbnail;
use crate::infra::db::repos::{AttachmentRepo, ChatRepo, VectorStoreRepo};
use crate::infra::db::tx::with_tx_retry;
use crate::infra::llm::anthropic_files::SECONDARY_PROVIDER_KIND;
use crate::infra::llm::{
    AnthropicFilesClient, ProviderResolver, RagClient, ResolvedProvider, VsFileStatus,
};
use crate::infra::outbox::payloads::{
    ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AttachmentCleanupEvent, AttachmentCleanupPayload, SecondaryRef,
};
use crate::infra::outbox::{OutboxEnqueuer, QueueKind};
use crate::infra::thumbnail::make_thumbnail;
use crate::infra::workers::background_indexing::{self, IndexingDeps, IndexingJob};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// Polls of the vector-store row by an upload that lost the creation race.
pub const VECTOR_STORE_LOSER_POLLS: u32 = 5;

/// Timings of the upload and indexing waits (DESIGN §3.6 "File Upload",
/// §3.7 "Creation protocol", B.9.5). The defaults are the documented values;
/// tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadTimings {
    /// Indexing wait of the upload request, counted from the request start (25 s).
    pub indexing_deadline: Duration,
    /// First wait between indexing status reads (250 ms; doubles).
    pub poll_initial: Duration,
    /// Largest wait between status reads in the request (2 s).
    pub poll_max: Duration,
    /// Background indexing heartbeat round (20 s).
    pub background_round: Duration,
    /// Background indexing limit (10 min).
    pub background_limit: Duration,
    /// Largest wait between background status reads (5 s).
    pub background_poll_max: Duration,
    /// First wait before retrying a failed `ready` write (1 s; doubles, 4 attempts).
    pub set_ready_retry: Duration,
    /// First wait of the creation-race loser polling (250 ms; doubles, 5 polls).
    pub vector_store_poll_initial: Duration,
    /// Age after which a NULL vector-store placeholder is reclaimed (120 s).
    pub stale_placeholder_after: Duration,
}

/// Background indexing heartbeat in seconds (DESIGN B.9.5).
const BACKGROUND_ROUND_SECS: u64 = 20;
/// Minimum `upload_reaper.stale_after_secs`.
const MIN_REAPER_STALE_AFTER_SECS: u64 = 60;
// B.9.5: the heartbeat is at most half the minimum reaper staleness.
const _: () = assert!(BACKGROUND_ROUND_SECS * 2 <= MIN_REAPER_STALE_AFTER_SECS);

impl Default for UploadTimings {
    fn default() -> Self {
        Self {
            indexing_deadline: Duration::from_secs(25),
            poll_initial: Duration::from_millis(250),
            poll_max: Duration::from_secs(2),
            background_round: Duration::from_secs(BACKGROUND_ROUND_SECS),
            background_limit: Duration::from_secs(600),
            background_poll_max: Duration::from_secs(5),
            set_ready_retry: Duration::from_secs(1),
            vector_store_poll_initial: Duration::from_millis(250),
            stale_placeholder_after: Duration::from_secs(120),
        }
    }
}

/// Infrastructure of [`AttachmentService`].
pub struct AttachmentDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<ChatAuthz>,
    pub chats: Arc<ChatService>,
    pub models: Arc<ModelCatalogService>,
    pub providers: Arc<ProviderResolver>,
    pub rag: Arc<RagClient>,
    /// Anthropic Files client (only when an `anthropic_messages` entry exists).
    pub anthropic_files: Option<Arc<AnthropicFilesClient>>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub timings: UploadTimings,
    /// Gear-stop token: background indexing tasks end when it is cancelled.
    pub stop: CancellationToken,
}

/// Everything an upload needs, resolved before the body is read.
#[allow(clippy::struct_excessive_bools)] // independent policy flags
pub struct UploadContext {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    /// Provider serving files / vector stores for the chat's model.
    pub provider: ResolvedProvider,
    /// The chat model's `anthropic_messages` provider: images get a secondary copy there.
    pub secondary: Option<ResolvedProvider>,
    /// `min(rag.uploaded_file_max_size_kb, model max_file_size_mb)` in bytes.
    pub document_limit_bytes: u64,
    /// `min(rag.uploaded_image_max_size_kb, model max_file_size_mb)` in bytes.
    pub image_limit_bytes: u64,
    /// The chat's model supports code interpreter and the kill switch is off.
    pub code_interpreter: bool,
    pub disable_images: bool,
    pub allow_csv: bool,
    started: Instant,
    _permit: OwnedSemaphorePermit,
}

/// The validated `file` part headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartMeta {
    pub filename: String,
    pub content_type: String,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// Largest accepted file size in bytes.
    pub limit_bytes: u64,
}

/// A fully read `file` part.
#[derive(Debug, Clone)]
pub struct UploadPart {
    pub meta: PartMeta,
    pub bytes: Bytes,
}

/// Last path component of a client filename (`/` and `\` separators).
fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

impl UploadContext {
    /// Validate the `file` part from its headers (before its bytes are read):
    /// filename, content type, image kill switch and code interpreter availability.
    ///
    /// # Errors
    /// `Multipart { MISSING_CONTENT_TYPE }`, `UnsupportedContentType`,
    /// `FeatureDisabled { images }`, `CodeInterpreterUnavailable`.
    pub fn validate_part(
        &self,
        filename: Option<&str>,
        content_type: Option<&str>,
    ) -> DomainResult<PartMeta> {
        let filename = normalize_filename(filename.map(base_name));
        let content_type =
            resolve_content_type(content_type, &filename, self.allow_csv).map_err(|e| match e {
                MimeError::MissingContentType => DomainError::Multipart {
                    field: "content_type",
                    reason: "MISSING_CONTENT_TYPE",
                    detail: "The file part has no content type".to_owned(),
                },
                MimeError::Unsupported(ct) => DomainError::UnsupportedContentType(ct),
            })?;
        let kind = attachment_kind(&content_type);
        if kind == AttachmentKind::Image && self.disable_images {
            return Err(DomainError::FeatureDisabled { subject: "images" });
        }
        let (for_file_search, for_code_interpreter) = purposes(&content_type);
        // Code interpreter as the only purpose needs the tool; as an extra
        // purpose it is dropped (DESIGN §4 "Code Interpreter Tool Availability").
        if for_code_interpreter && !self.code_interpreter && !for_file_search {
            return Err(DomainError::CodeInterpreterUnavailable);
        }
        let limit_bytes = match kind {
            AttachmentKind::Image => self.image_limit_bytes,
            AttachmentKind::Document => self.document_limit_bytes,
        };
        Ok(PartMeta {
            filename,
            content_type,
            kind,
            for_file_search,
            for_code_interpreter: for_code_interpreter && self.code_interpreter,
            limit_bytes,
        })
    }
}

/// Result of the in-request indexing wait.
enum Indexing {
    Ready,
    /// Still `in_progress` at the deadline; finished by the background task.
    InBackground,
}

pub struct AttachmentService {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    chats: Arc<ChatService>,
    models: Arc<ModelCatalogService>,
    providers: Arc<ProviderResolver>,
    rag: Arc<RagClient>,
    anthropic_files: Option<Arc<AnthropicFilesClient>>,
    outbox: Arc<OutboxEnqueuer>,
    timings: UploadTimings,
    stop: CancellationToken,
    uploads: Arc<Semaphore>,
}

impl AttachmentService {
    #[must_use]
    pub fn new(d: AttachmentDeps) -> Self {
        let permits = usize::from(d.config.rag.max_concurrent_uploads);
        Self {
            cfg: d.config,
            db: d.db,
            authz: d.authz,
            chats: d.chats,
            models: d.models,
            providers: d.providers,
            rag: d.rag,
            anthropic_files: d.anthropic_files,
            outbox: d.outbox,
            timings: d.timings,
            stop: d.stop,
            uploads: Arc::new(Semaphore::new(permits)),
        }
    }

    /// Steps before the body is read: authorization + chat (404), the chat's
    /// model without the enabled filter (`INVALID_MODEL`), the upload limits and
    /// code interpreter availability, the RAG provider, and a concurrency permit.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `InvalidModel`, policy failures,
    /// `UploadConcurrencyLimit`.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> DomainResult<UploadContext> {
        let started = Instant::now();
        let scope = self
            .authz
            .chat_scope(ctx, actions::UPLOAD_ATTACHMENT, Some(chat_id))
            .await?;
        let chat = self.chats.load_chat(&scope, chat_id).await?;
        let model_id = chat.model.as_deref().ok_or(DomainError::InvalidModel)?;
        let model = self
            .models
            .resolve_chat_model(ctx.subject_id(), model_id)
            .await?;
        let entry = &model.entry;
        let ks = model.snapshot.kill_switches;
        let model_limit = u64::from(entry.general_config.max_file_size_mb) * MIB;
        let rag = &self.cfg.rag;
        let provider = self
            .providers
            .resolve_rag(&entry.provider_id, chat.tenant_id)?;
        let secondary = match self.anthropic_files {
            Some(_) => Some(self.providers.resolve(&entry.provider_id, chat.tenant_id)?)
                .filter(|p| p.kind == ProviderKind::AnthropicMessages),
            None => None,
        };
        let permit = Arc::clone(&self.uploads)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;
        Ok(UploadContext {
            tenant_id: chat.tenant_id,
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            provider,
            secondary,
            document_limit_bytes: (u64::from(rag.uploaded_file_max_size_kb) * KIB).min(model_limit),
            image_limit_bytes: (u64::from(rag.uploaded_image_max_size_kb) * KIB).min(model_limit),
            code_interpreter: entry.general_config.tool_support.code_interpreter
                && !ks.disable_code_interpreter,
            disable_images: ks.disable_images,
            allow_csv: rag.allow_csv_upload,
            started,
            _permit: permit,
        })
    }

    /// Store the attachment: insert the `pending` row (per-chat limits), upload
    /// the file to the provider, then index the document, build the image
    /// thumbnail or finish the code-interpreter file. Returns the row as stored
    /// (`ready`, or `uploaded` while indexing continues in the background).
    ///
    /// # Errors
    /// `DocumentLimit`, `StorageLimit`, `ProviderMismatch`, `StorageUnavailable`
    /// (the row is then `failed`), database failures.
    pub async fn upload(
        &self,
        up: UploadContext,
        part: UploadPart,
    ) -> DomainResult<attachment::Model> {
        let UploadPart { meta, bytes } = part;
        let id = Uuid::new_v4();
        let now = now_utc();
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let row = attachment::Model {
            id,
            tenant_id: up.tenant_id,
            chat_id: up.chat_id,
            uploaded_by_user_id: up.user_id,
            filename: meta.filename.clone(),
            content_type: Some(meta.content_type.clone()),
            size_bytes: Some(size),
            storage_backend: up.provider.storage_backend.clone(),
            provider_file_id: None,
            status: AttachmentStatus::Pending.as_str().to_owned(),
            error_code: None,
            attachment_kind: meta.kind.as_str().to_owned(),
            for_file_search: meta.for_file_search,
            for_code_interpreter: meta.for_code_interpreter,
            doc_summary: None,
            img_thumbnail: None,
            img_thumbnail_width: None,
            img_thumbnail_height: None,
            summary_model: None,
            summary_updated_at: None,
            cleanup_status: None,
            cleanup_attempts: 0,
            last_cleanup_error: None,
            cleanup_updated_at: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            secondary_file_id: None,
            secondary_status: "not_attempted".to_owned(),
            secondary_provider_kind: None,
        };
        self.insert_within_limits(row).await?;

        let provider_name = format!(
            "{}_{}.{}",
            up.chat_id,
            id,
            extension_for(&meta.content_type)
        );
        let file_id = match self
            .rag
            .upload_file(
                &up.provider,
                &provider_name,
                &meta.content_type,
                bytes.clone(),
            )
            .await
        {
            Ok(f) => f,
            Err(e) => {
                self.fail(&up, id, error_codes::UPLOAD_FAILED).await;
                return Err(DomainError::StorageUnavailable(format!("file upload: {e}")));
            }
        };
        if !AttachmentRepo::mark_uploaded(&self.db.conn()?, up.tenant_id, id, &file_id, now_utc())
            .await?
        {
            return Err(self.abandon_deleted_upload(&up, id, &file_id).await);
        }

        if meta.kind == AttachmentKind::Image {
            self.secondary_copy(&up, id, &provider_name, &meta, &bytes)
                .await;
            let cfg = self.cfg.thumbnail.clone();
            let thumb = tokio::task::spawn_blocking(move || make_thumbnail(&bytes, &cfg))
                .await
                .ok()
                .flatten()
                .and_then(|t| {
                    Some(ImgThumbnail {
                        width: i32::try_from(t.width).ok()?,
                        height: i32::try_from(t.height).ok()?,
                        data: t.webp,
                    })
                });
            self.mark_ready(&up, id, thumb).await?;
        } else if meta.for_file_search {
            if let Indexing::Ready = self.index_document(&up, id, &meta, &file_id).await? {
                self.mark_ready(&up, id, None).await?;
            }
        } else {
            self.mark_ready(&up, id, None).await?;
        }
        AttachmentRepo::find_one(&self.db.conn()?, up.tenant_id, up.chat_id, id)
            .await?
            .ok_or_else(|| DomainError::internal("uploaded attachment row disappeared"))
    }

    /// One attachment of the caller in `chat_id`.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `AttachmentNotFound` (missing,
    /// deleted, other chat or other uploader).
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        id: Uuid,
    ) -> DomainResult<attachment::Model> {
        let chat = self
            .load_chat(ctx, actions::READ_ATTACHMENT, chat_id)
            .await?;
        AttachmentRepo::find_one(&self.db.conn()?, chat.tenant_id, chat.id, id)
            .await?
            .filter(|a| a.deleted_at.is_none() && a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::AttachmentNotFound)
    }

    /// Delete an attachment no message references: soft-delete, hand it to
    /// cleanup and enqueue the `attachment_deleted` cleanup message, in one
    /// transaction. Deleting an already deleted attachment is a no-op.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `AttachmentNotFound`,
    /// `AttachmentLocked`, database failures.
    pub async fn delete(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> DomainResult<()> {
        let chat = self
            .load_chat(ctx, actions::DELETE_ATTACHMENT, chat_id)
            .await?;
        let user_id = ctx.subject_id();
        let outbox = Arc::clone(&self.outbox);
        let (tenant_id, chat_id) = (chat.tenant_id, chat.id);
        // The upstream holding a secondary copy, derived outside the transaction
        // (a policy call) and only when the row has one.
        let secondary_alias =
            match AttachmentRepo::find_one(&self.db.conn()?, tenant_id, chat_id, id).await? {
                Some(a) if has_secondary_copy(&a) => {
                    secondary_provider(&self.models, &self.providers, &chat)
                        .await
                        .map(|p| p.alias)
                }
                _ => None,
            };
        let wake = with_tx_retry(&self.db, "attachment delete", move |tx| {
            let outbox = Arc::clone(&outbox);
            let secondary_alias = secondary_alias.clone();
            Box::pin(async move {
                // First statement a write: takes the write lock up front.
                ChatRepo::lock_for_write(tx, tenant_id, chat_id).await?;
                let a = AttachmentRepo::find_one(tx, tenant_id, chat_id, id)
                    .await?
                    .filter(|a| a.uploaded_by_user_id == user_id)
                    .ok_or(DomainError::AttachmentNotFound)?;
                if a.deleted_at.is_some() {
                    return Ok(None);
                }
                if AttachmentRepo::is_referenced(tx, tenant_id, chat_id, id).await? {
                    return Err(DomainError::AttachmentLocked);
                }
                let now = now_utc();
                if !AttachmentRepo::soft_delete_for_cleanup(tx, tenant_id, id, now).await? {
                    return Ok(None);
                }
                let secondary_ref = secondary_ref(&a, secondary_alias);
                let payload = AttachmentCleanupPayload {
                    event_type: AttachmentCleanupEvent::AttachmentDeleted,
                    tenant_id,
                    chat_id,
                    attachment_id: id,
                    provider_file_id: a.provider_file_id,
                    vector_store_id: None,
                    storage_backend: a.storage_backend,
                    attachment_kind: a.attachment_kind,
                    deleted_at: now,
                    secondary_ref,
                };
                outbox
                    .enqueue_json(
                        tx,
                        QueueKind::AttachmentCleanup,
                        tenant_id,
                        ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
                        &payload,
                    )
                    .await
                    .map(Some)
            })
        })
        .await?;
        if let Some(wake) = wake {
            wake.fire();
        }
        Ok(())
    }

    /// Upload the secondary (Anthropic) copy of an image of an Anthropic chat,
    /// best effort: the attachment itself is never affected. A provider failure
    /// is recorded as `secondary_status = 'failed'`; a database failure after
    /// the upload deletes the new copy again (it could never be cleaned up
    /// otherwise). Every failure is only logged. Images above
    /// `thumbnail.max_decode_bytes` get no copy.
    async fn secondary_copy(
        &self,
        up: &UploadContext,
        id: Uuid,
        filename: &str,
        meta: &PartMeta,
        bytes: &Bytes,
    ) {
        if let Err(e) = self.try_secondary_copy(up, id, filename, meta, bytes).await {
            warn!(attachment_id = %id, error = %e, "secondary (Anthropic) image copy not stored");
        }
    }

    async fn try_secondary_copy(
        &self,
        up: &UploadContext,
        id: Uuid,
        filename: &str,
        meta: &PartMeta,
        bytes: &Bytes,
    ) -> Result<(), String> {
        let (Some(client), Some(provider)) = (&self.anthropic_files, &up.secondary) else {
            return Ok(());
        };
        if bytes.len() > self.cfg.thumbnail.max_decode_bytes {
            info!(attachment_id = %id, "image above thumbnail.max_decode_bytes: no secondary copy");
            return Ok(());
        }
        self.set_secondary(up, id, "pending", None)
            .await
            .map_err(|e| format!("cannot record the pending copy: {e}"))?;
        let file_id = match client
            .upload(provider, filename, &meta.content_type, bytes.clone())
            .await
        {
            Ok(file_id) => file_id,
            Err(e) => {
                let recorded = self.set_secondary(up, id, "failed", None).await;
                return Err(match recorded {
                    Ok(()) => format!("upload failed: {e}"),
                    Err(db) => format!("upload failed: {e}; cannot record it: {db}"),
                });
            }
        };
        if let Err(db) = self.set_secondary(up, id, "uploaded", Some(&file_id)).await {
            let dropped = client.delete(provider, &file_id).await;
            return Err(format!(
                "cannot record the uploaded copy: {db}; best-effort delete of the copy: {}",
                dropped.map_or_else(|e| e.to_string(), |()| "done".to_owned())
            ));
        }
        Ok(())
    }

    /// Write the `secondary_*` columns (`secondary_provider_kind` with the file id).
    async fn set_secondary(
        &self,
        up: &UploadContext,
        id: Uuid,
        status: &str,
        file_id: Option<&str>,
    ) -> DomainResult<()> {
        AttachmentRepo::set_secondary(
            &self.db.conn()?,
            up.tenant_id,
            id,
            status,
            file_id,
            file_id.map(|_| SECONDARY_PROVIDER_KIND),
            now_utc(),
        )
        .await
    }

    async fn load_chat(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
        chat_id: Uuid,
    ) -> DomainResult<chat::Model> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        self.chats.load_chat(&scope, chat_id).await
    }

    /// Insert the `pending` row after the per-chat document / storage limits,
    /// in one transaction.
    async fn insert_within_limits(&self, row: attachment::Model) -> DomainResult<()> {
        let max_documents = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_bytes = u64::from(self.cfg.rag.max_total_upload_mb_per_chat) * MIB;
        with_tx_retry(&self.db, "attachment insert", move |tx| {
            let row = row.clone();
            Box::pin(async move {
                // First statement a write: takes the write lock up front, so the
                // limit check reads the committed state and the insert cannot
                // hit a stale snapshot.
                ChatRepo::lock_for_write(tx, row.tenant_id, row.chat_id).await?;
                let usage = AttachmentRepo::chat_usage(tx, row.tenant_id, row.chat_id).await?;
                if row.attachment_kind == AttachmentKind::Document.as_str()
                    && usage.documents >= max_documents
                {
                    return Err(DomainError::DocumentLimit);
                }
                let total = usage
                    .total_bytes
                    .saturating_add(row.size_bytes.unwrap_or(0));
                if u64::try_from(total).unwrap_or(u64::MAX) > max_bytes {
                    return Err(DomainError::StorageLimit);
                }
                AttachmentRepo::insert(tx, row).await
            })
        })
        .await
    }

    async fn mark_ready(
        &self,
        up: &UploadContext,
        id: Uuid,
        thumbnail: Option<ImgThumbnail>,
    ) -> DomainResult<()> {
        AttachmentRepo::mark_ready(&self.db.conn()?, up.tenant_id, id, thumbnail, now_utc())
            .await
            .map(drop)
    }

    /// Mark the row `failed` with `error_code` (a database failure is only logged:
    /// the request fails anyway and the upload reaper finishes the row).
    async fn fail(&self, up: &UploadContext, id: Uuid, error_code: &str) {
        let res = match self.db.conn() {
            Ok(conn) => AttachmentRepo::mark_failed(&conn, up.tenant_id, id, error_code, now_utc())
                .await
                .map(drop),
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            warn!(attachment_id = %id, error_code, error = %e, "cannot mark the attachment failed");
        }
    }

    /// Fail the row with `error_code` and delete the provider file (best
    /// effort, fire-and-forget, not retried); returns the 503 for `cause`.
    async fn fail_and_drop_file(
        &self,
        up: &UploadContext,
        id: Uuid,
        file_id: &str,
        error_code: &str,
        cause: String,
    ) -> DomainError {
        self.fail(up, id, error_code).await;
        self.drop_file(up, id, file_id);
        DomainError::StorageUnavailable(cause)
    }

    /// Delete the provider file in the background (best effort, not retried).
    fn drop_file(&self, up: &UploadContext, id: Uuid, file_id: &str) {
        let rag = Arc::clone(&self.rag);
        let provider = up.provider.clone();
        let file_id = file_id.to_owned();
        tokio::spawn(async move {
            if let Err(e) = rag.delete_file(&provider, &file_id).await {
                warn!(attachment_id = %id, error = %e, "best-effort provider file delete failed");
            }
        });
    }

    /// The row was deleted (attachment delete) or handed to cleanup (chat
    /// delete) while the file was being uploaded: its cleanup message carries
    /// no provider file id, so the file is deleted here (best effort) and the
    /// upload stops with 404 (attachment or chat).
    async fn abandon_deleted_upload(
        &self,
        up: &UploadContext,
        id: Uuid,
        file_id: &str,
    ) -> DomainError {
        info!(attachment_id = %id, "attachment or chat deleted during the upload; dropping the provider file");
        self.drop_file(up, id, file_id);
        let deleted_attachment = match self.db.conn() {
            Ok(conn) => AttachmentRepo::find_one(&conn, up.tenant_id, up.chat_id, id)
                .await
                .ok()
                .flatten()
                .is_some_and(|a| a.deleted_at.is_some()),
            Err(_) => false,
        };
        if deleted_attachment {
            DomainError::AttachmentNotFound
        } else {
            DomainError::ChatNotFound
        }
    }

    /// Add the document to the chat vector store and wait for indexing until
    /// the request deadline; a file still `in_progress` then is handed to the
    /// background indexing task.
    async fn index_document(
        &self,
        up: &UploadContext,
        id: Uuid,
        meta: &PartMeta,
        file_id: &str,
    ) -> DomainResult<Indexing> {
        let vs = match self
            .ensure_vector_store(up.tenant_id, up.chat_id, &up.provider)
            .await
        {
            Ok(vs) => vs,
            Err(e) => {
                // The row fails either way; the caller gets the protocol's own
                // error (409 provider_mismatch, 503, 500).
                self.fail_and_drop_file(
                    up,
                    id,
                    file_id,
                    error_codes::VECTOR_STORE_FAILED,
                    e.to_string(),
                )
                .await;
                return Err(e);
            }
        };
        let status = match self.rag.add_file(&up.provider, &vs, file_id, id).await {
            Ok(s) => s,
            Err(e) => {
                return Err(self
                    .fail_and_drop_file(
                        up,
                        id,
                        file_id,
                        error_codes::INDEXING_FAILED,
                        format!("add file to vector store: {e}"),
                    )
                    .await);
            }
        };
        match self.poll_indexing(up, id, &vs, file_id, status).await {
            Ok(true) => Ok(Indexing::Ready),
            Ok(false) => {
                info!(attachment_id = %id, "indexing still in progress at the request deadline; continuing in the background");
                self.spawn_background_indexing(up, id, meta, vs, file_id);
                Ok(Indexing::InBackground)
            }
            Err(cause) => Err(self
                .fail_and_drop_file(up, id, file_id, error_codes::INDEXING_FAILED, cause)
                .await),
        }
    }

    /// Poll the indexing status (waits from `poll_initial` doubling to
    /// `poll_max`, each read bounded by the deadline) until `completed`
    /// (`Ok(true)`), the request deadline (`Ok(false)`) or a failure
    /// (`Err(cause)`: a failed status or a non-transient read error).
    async fn poll_indexing(
        &self,
        up: &UploadContext,
        id: Uuid,
        vs: &str,
        file_id: &str,
        mut status: VsFileStatus,
    ) -> Result<bool, String> {
        let deadline = up.started + self.timings.indexing_deadline;
        let mut wait = self.timings.poll_initial;
        loop {
            match status {
                VsFileStatus::Completed => return Ok(true),
                VsFileStatus::Failed => {
                    return Err("vector store reported a failed indexing status".to_owned());
                }
                VsFileStatus::InProgress => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(wait.min(deadline - now)).await;
            wait = (wait * 2).min(self.timings.poll_max);
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            match tokio::time::timeout(left, self.rag.file_status(&up.provider, vs, file_id)).await
            {
                Err(_elapsed) => return Ok(false),
                Ok(Ok(s)) => status = s,
                Ok(Err(e)) if e.is_transient() => {
                    warn!(attachment_id = %id, error = %e, "transient indexing status read error");
                }
                Ok(Err(e)) => return Err(format!("indexing status read: {e}")),
            }
        }
    }

    fn spawn_background_indexing(
        &self,
        up: &UploadContext,
        id: Uuid,
        meta: &PartMeta,
        vs: String,
        file_id: &str,
    ) {
        let _task = background_indexing::spawn(
            IndexingDeps {
                db: Arc::clone(&self.db),
                rag: Arc::clone(&self.rag),
                outbox: Arc::clone(&self.outbox),
                timings: self.timings,
                stop: self.stop.clone(),
            },
            IndexingJob {
                tenant_id: up.tenant_id,
                chat_id: up.chat_id,
                attachment_id: id,
                provider: up.provider.clone(),
                vector_store_id: vs,
                provider_file_id: file_id.to_owned(),
                storage_backend: up.provider.storage_backend.clone(),
                attachment_kind: meta.kind.as_str().to_owned(),
            },
        );
    }

    /// The chat's vector store id, created on first use (DESIGN §3.7
    /// `chat_vector_stores` "Creation protocol", "Stale placeholder reclaim").
    ///
    /// # Errors
    /// `ProviderMismatch` when the chat's store belongs to another backend;
    /// `StorageUnavailable` when the store cannot be created or a concurrent
    /// creation does not finish within the loser polls; database failures.
    pub(crate) async fn ensure_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        p: &ResolvedProvider,
    ) -> DomainResult<String> {
        let backend = p.storage_backend.as_str();
        // A reclaimed stale placeholder restarts the protocol once.
        for _ in 0..2 {
            let existing = VectorStoreRepo::find(&self.db.conn()?, tenant_id, chat_id).await?;
            if let Some(row) = existing {
                if row.provider != backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(vs) = row.vector_store_id {
                    return Ok(vs);
                }
                if self.is_stale(row.created_at) {
                    info!(%chat_id, "reclaiming a stale vector store placeholder");
                    VectorStoreRepo::delete_placeholder(
                        &self.db.conn()?,
                        tenant_id,
                        chat_id,
                        row.id,
                    )
                    .await?;
                    continue;
                }
                return self.await_vector_store(tenant_id, chat_id, backend).await;
            }
            let placeholder = VectorStoreRepo::insert_placeholder(
                &self.db.conn()?,
                tenant_id,
                chat_id,
                backend,
                now_utc(),
            )
            .await;
            return match placeholder {
                Ok(row) => {
                    self.create_vector_store(tenant_id, chat_id, p, row.id)
                        .await
                }
                Err(DomainError::Conflict { .. }) => {
                    self.await_vector_store(tenant_id, chat_id, backend).await
                }
                Err(e) => Err(e),
            };
        }
        Err(DomainError::StorageUnavailable(
            "vector store placeholder keeps being reclaimed".to_owned(),
        ))
    }

    fn is_stale(&self, created_at: chrono::DateTime<chrono::Utc>) -> bool {
        let age = now_utc().signed_duration_since(created_at);
        age.to_std()
            .is_ok_and(|age| age > self.timings.stale_placeholder_after)
    }

    /// Winner path: create the provider store and CAS its id into the placeholder.
    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    async fn create_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        p: &ResolvedProvider,
        row_id: Uuid,
    ) -> DomainResult<String> {
        let vs = match self
            .rag
            .create_vector_store(p, &format!("chat_{chat_id}"))
            .await
        {
            Ok(vs) => vs,
            Err(e) => {
                if let Err(db) = self.delete_placeholder(tenant_id, chat_id, row_id).await {
                    warn!(%chat_id, error = %db, "cannot delete the vector store placeholder");
                }
                return Err(DomainError::StorageUnavailable(format!(
                    "create vector store: {e}"
                )));
            }
        };
        if VectorStoreRepo::set_vector_store_id(&self.db.conn()?, tenant_id, chat_id, row_id, &vs)
            .await?
        {
            return Ok(vs);
        }
        // The placeholder was reclaimed meanwhile: drop the new store, use the chat's.
        warn!(%chat_id, "vector store placeholder was reclaimed during creation");
        if let Err(e) = self.rag.delete_vector_store(p, &vs).await {
            warn!(%chat_id, error = %e, "best-effort vector store delete failed");
        }
        self.await_vector_store(tenant_id, chat_id, p.storage_backend.as_str())
            .await
    }

    async fn delete_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> DomainResult<()> {
        VectorStoreRepo::delete_placeholder(&self.db.conn()?, tenant_id, chat_id, row_id)
            .await
            .map(drop)
    }

    /// Loser path: poll the row (backoff doubling) until the winner set the id.
    async fn await_vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        backend: &str,
    ) -> DomainResult<String> {
        let mut wait = self.timings.vector_store_poll_initial;
        for _ in 0..VECTOR_STORE_LOSER_POLLS {
            tokio::time::sleep(wait).await;
            wait *= 2;
            if let Some(row) = VectorStoreRepo::find(&self.db.conn()?, tenant_id, chat_id).await? {
                if row.provider != backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(vs) = row.vector_store_id {
                    return Ok(vs);
                }
            }
        }
        Err(DomainError::StorageUnavailable(
            "vector store creation by a concurrent upload did not finish".to_owned(),
        ))
    }
}

/// The attachment has an uploaded secondary (Anthropic) copy.
pub(crate) fn has_secondary_copy(a: &attachment::Model) -> bool {
    a.secondary_status == "uploaded"
        && a.secondary_file_id.is_some()
        && a.secondary_provider_kind.is_some()
}

/// The `anthropic_messages` provider holding the secondary copies of `chat`:
/// the provider of the chat's model in the current catalog (tenant override
/// applied), as at upload time. When the model is gone (or no longer served by
/// an Anthropic provider) it falls back, with a warning, to the
/// `anthropic_messages` entry with the lowest id.
pub(crate) async fn secondary_provider(
    models: &ModelCatalogService,
    providers: &ProviderResolver,
    chat: &chat::Model,
) -> Option<ResolvedProvider> {
    let from_model = match chat.model.as_deref() {
        Some(model) => match models.resolve_chat_model(chat.user_id, model).await {
            Ok(m) => providers
                .resolve(&m.entry.provider_id, chat.tenant_id)
                .ok()
                .filter(|p| p.kind == ProviderKind::AnthropicMessages),
            Err(e) => {
                warn!(chat_id = %chat.id, model, error = %e, "cannot resolve the chat's model");
                None
            }
        },
        None => None,
    };
    from_model.or_else(|| {
        warn!(chat_id = %chat.id, "secondary copy provider not derivable from the chat's model; using the first anthropic_messages provider");
        providers.resolve_kind(ProviderKind::AnthropicMessages, chat.tenant_id)
    })
}

/// `secondary_ref` of a deleted attachment: set when its secondary copy was
/// uploaded and the upstream holding it (`alias`) is known.
fn secondary_ref(a: &attachment::Model, alias: Option<String>) -> Option<SecondaryRef> {
    if !has_secondary_copy(a) {
        return None;
    }
    let file_id = a.secondary_file_id.clone()?;
    let Some(upstream_alias) = alias else {
        // mini_chat_secondary_cleanup_skipped{provider_kind}
        warn!(attachment_id = %a.id, secondary_file_id = %file_id, "no Anthropic upstream: the secondary copy is not deleted");
        return None;
    };
    Some(SecondaryRef {
        file_id,
        provider_kind: a.secondary_provider_kind.clone()?,
        upstream_alias,
    })
}

#[cfg(test)]
#[path = "attachment_tests.rs"]
mod attachment_tests;
