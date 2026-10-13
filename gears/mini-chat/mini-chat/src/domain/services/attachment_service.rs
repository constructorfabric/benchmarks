//! Attachments (S§11; D "Upload / Get / Delete Attachment", D "File
//! Upload", D§3.7 `chat_vector_stores` creation protocol, ADR-0007).

use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use mini_chat_sdk::ModelCatalogEntry;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_security::SecurityContext;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::authz;
use crate::domain::clock::Clock;
use crate::domain::error::{DomainError, FeatureSubject};
use crate::domain::models::{AttachmentDetail, AttachmentKind};
use crate::domain::ports::{
    IndexStatus, OutboxPort, PendingWakes, SecondaryFilesPort, StorageError, StoragePort,
};
use crate::domain::services::{ChatService, ModelResolver};
use crate::infra::db::entity::{attachment, chat, chat_vector_store};
use crate::infra::db::repos::{AttachmentRepo, MessageAttachmentRepo, VectorStoreRepo};
use crate::infra::db::tx::with_retry;
use crate::infra::llm::provider_resolver::{ProviderResolver, StorageTarget};
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::mime::{self, ResolvedMime};
use crate::infra::outbox::payloads::{
    AttachmentCleanupEventType, AttachmentCleanupPayload, SecondaryRef,
};
use crate::infra::thumbnail;
use crate::infra::workers::background_indexing::{self, IndexingDeps, IndexingJob};

/// A placeholder `chat_vector_stores` row older than this is reclaimed
/// (its creator died between the insert and the CAS).
const STALE_PLACEHOLDER: time::Duration = time::Duration::seconds(120);
/// Polls of a vector-store creation loser before it gives up (503).
const LOSER_POLLS: u32 = 5;
/// Background round length in production (heartbeat interval of the
/// background indexing task).
pub(crate) const BG_ROUND: Duration = Duration::from_secs(20);
const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// Indexing waits of an upload and of the background indexing task
/// (D "File Upload"). Fixed in production ([`Default`]); tests inject
/// millisecond values through `AppDeps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingTimings {
    /// The upload stops waiting for indexing this long after it started.
    pub deadline: Duration,
    /// First wait between status reads in the request (doubles).
    pub poll_initial: Duration,
    /// Longest wait between status reads in the request.
    pub poll_max: Duration,
    /// Background round length (each round refreshes `updated_at`).
    pub bg_round: Duration,
    /// Longest wait between background status reads (doubles from
    /// `poll_initial`).
    pub bg_poll_max: Duration,
    /// Background indexing gives up after this long.
    pub bg_total: Duration,
    /// Waits between the attempts to set `ready` (4 attempts in total).
    pub ready_retry_delays: [Duration; 3],
    /// First wait of a vector-store creation loser polling for the store id
    /// (doubles, 5 polls).
    pub vector_store_poll_initial: Duration,
}

impl Default for IndexingTimings {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(25),
            poll_initial: Duration::from_millis(250),
            poll_max: Duration::from_secs(2),
            bg_round: BG_ROUND,
            bg_poll_max: Duration::from_secs(5),
            bg_total: Duration::from_secs(600),
            ready_retry_delays: [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            vector_store_poll_initial: Duration::from_millis(250),
        }
    }
}

/// Per-kind upload size limits of a chat: `min(rag limit, model
/// max_file_size_mb)` (D B.8); a model `max_file_size_mb` of 0 is no cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadLimits {
    pub document_bytes: u64,
    pub image_bytes: u64,
}

impl UploadLimits {
    fn for_kind(self, kind: AttachmentKind) -> u64 {
        match kind {
            AttachmentKind::Document => self.document_bytes,
            AttachmentKind::Image => self.image_bytes,
        }
    }
}

/// What the upload handler resolves before it reads the body.
pub struct UploadContext {
    pub chat: chat::Model,
    /// Tenant + owner scope of the chat.
    pub scope: AccessScope,
    /// The chat's model (resolved without the enabled filter).
    pub model: ModelCatalogEntry,
    pub storage: StorageTarget,
    pub limits: UploadLimits,
    /// Upload concurrency slot, held until the upload returns.
    pub permit: OwnedSemaphorePermit,
    /// Kill switch off and the chat's model supports code interpreter.
    pub code_interpreter_available: bool,
    pub disable_images: bool,
}

/// The `file` part of an upload.
pub struct UploadPart<S> {
    /// Filename of the part as sent (sanitized by the service).
    pub filename: Option<String>,
    /// Content type of the part as sent.
    pub content_type: String,
    /// Body bytes of the part.
    pub stream: S,
}

/// Dependencies of [`AttachmentService`].
pub struct AttachmentDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub chats: Arc<ChatService>,
    pub models: Arc<ModelResolver>,
    pub providers: Arc<ProviderResolver>,
    pub storage: Arc<dyn StoragePort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub indexing: IndexingTimings,
    pub shutdown: CancellationToken,
    pub metrics: Arc<MiniChatMetrics>,
    /// Anthropic Files client (only when an `anthropic_messages` entry exists).
    pub secondary: Option<Arc<dyn SecondaryFilesPort>>,
}

/// Upload / get / delete of chat attachments.
pub struct AttachmentService {
    config: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    chats: Arc<ChatService>,
    models: Arc<ModelResolver>,
    providers: Arc<ProviderResolver>,
    storage: Arc<dyn StoragePort>,
    outbox: Arc<dyn OutboxPort>,
    timings: IndexingTimings,
    shutdown: CancellationToken,
    /// `rag.max_concurrent_uploads` slots (per instance).
    permits: Arc<Semaphore>,
    metrics: Arc<MiniChatMetrics>,
    secondary: Option<Arc<dyn SecondaryFilesPort>>,
}

/// What the cleanup of a deleted row does with its secondary copy.
enum SecondaryCleanup {
    /// No uploaded secondary copy.
    Nothing,
    /// Delete it (carried as the message's `secondary_ref`).
    Delete(SecondaryRef),
    /// The Anthropic upstream is not resolvable: skipped and counted.
    Skip {
        provider_kind: String,
        file_id: String,
    },
}

/// Where an upload's provider file lives (for failure handling).
struct Uploaded<'a> {
    up: &'a UploadContext,
    row: &'a attachment::Model,
    file_id: String,
    started: Instant,
}

impl AttachmentService {
    #[must_use]
    pub fn new(d: AttachmentDeps) -> Self {
        let permits = Arc::new(Semaphore::new(usize::from(
            d.config.rag.max_concurrent_uploads,
        )));
        Self {
            config: d.config,
            db: d.db,
            clock: d.clock,
            chats: d.chats,
            models: d.models,
            providers: d.providers,
            storage: d.storage,
            outbox: d.outbox,
            timings: d.indexing,
            shutdown: d.shutdown,
            permits,
            metrics: d.metrics,
            secondary: d.secondary,
        }
    }

    /// Checks of an upload before its body is read (S§11): authorization
    /// and the scoped chat, the chat's model, the storage target, the size
    /// limits and an upload concurrency slot.
    ///
    /// # Errors
    /// `ChatNotFound`, `InvalidModel`, authorization, plugin and provider
    /// resolution failures, `UploadConcurrencyLimit`.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<UploadContext, DomainError> {
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::UPLOAD_ATTACHMENT, chat_id)
            .await?;
        let (snap, model) = self
            .models
            .resolve_chat_model(ctx.subject_id(), &chat.model)
            .await?;
        let storage = self
            .providers
            .resolve_storage(&model.provider_id, ctx.subject_tenant_id())?;
        // `max_file_size_mb = 0` = no per-model cap (the field defaults to 0).
        let model_limit = match model.general_config.max_file_size_mb {
            0 => u64::MAX,
            mb => u64::from(mb) * MIB,
        };
        let rag = &self.config.rag;
        let limits = UploadLimits {
            document_bytes: (u64::from(rag.uploaded_file_max_size_kb) * KIB).min(model_limit),
            image_bytes: (u64::from(rag.uploaded_image_max_size_kb) * KIB).min(model_limit),
        };
        let permit = Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;
        let ks = snap.kill_switches;
        let code_interpreter_available =
            model.general_config.tool_support.code_interpreter && !ks.disable_code_interpreter;
        Ok(UploadContext {
            chat,
            scope,
            model,
            storage,
            limits,
            permit,
            code_interpreter_available,
            disable_images: ks.disable_images,
        })
    }

    /// Upload the `file` part (S§11, D "File Upload"): MIME / kind /
    /// purpose resolution, size limit, per-chat limits, the `pending` row,
    /// the provider upload, then the kind-specific path (vector store and
    /// indexing wait, thumbnail, or nothing for code-interpreter files).
    /// Returns the row as `ready`, or `uploaded` when indexing is still
    /// running at the deadline (a background task finishes it).
    ///
    /// # Errors
    /// `UnsupportedContentType`, `FeatureDisabled(Images)`,
    /// `CodeInterpreterUnavailable`, `FileTooLarge`, `Multipart` (body read
    /// error), `ProviderMismatch`, `DocumentLimit`, `StorageLimit`,
    /// `StorageUnavailable` (provider failures; the row is `failed`),
    /// database failures.
    pub async fn upload<S, E>(
        &self,
        ctx: &SecurityContext,
        up: UploadContext,
        part: UploadPart<S>,
    ) -> Result<AttachmentDetail, DomainError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send,
        E: Display,
    {
        let mut kind = "unknown";
        let result = self.upload_part(ctx, up, part, &mut kind).await;
        match &result {
            Ok(detail) => self.metrics.attachment_upload(
                kind,
                "ok",
                Some(u64::try_from(detail.size_bytes).unwrap_or(0)),
            ),
            Err(_) => self.metrics.attachment_upload(kind, "error", None),
        }
        result
    }

    /// [`Self::upload`] without the metrics; `kind` is set once the MIME
    /// type is resolved.
    async fn upload_part<S, E>(
        &self,
        ctx: &SecurityContext,
        up: UploadContext,
        part: UploadPart<S>,
        kind: &mut &'static str,
    ) -> Result<AttachmentDetail, DomainError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send,
        E: Display,
    {
        let started = Instant::now();
        let filename = mime::sanitize_filename(part.filename.as_deref());
        let resolved = mime::resolve(
            &part.content_type,
            &filename,
            self.config.rag.allow_csv_upload,
        )?;
        *kind = resolved.kind.as_str();
        if resolved.kind == AttachmentKind::Image && up.disable_images {
            return Err(DomainError::FeatureDisabled(FeatureSubject::Images));
        }
        if resolved.for_code_interpreter
            && !resolved.for_file_search
            && !up.code_interpreter_available
        {
            return Err(DomainError::CodeInterpreterUnavailable);
        }
        let bytes = read_limited(part.stream, up.limits.for_kind(resolved.kind)).await?;
        let uses_vector_store =
            resolved.for_file_search && resolved.kind == AttachmentKind::Document;
        if uses_vector_store {
            self.check_store_backend(&up).await?;
        }

        let row = self
            .insert_pending(ctx, &up, &filename, &resolved, bytes.len())
            .await?;
        // Counted while this request processes the row (a background
        // indexing task holds its own guard).
        let _pending = self.metrics.attachment_pending();
        let provider_name = format!("{}_{}.{}", up.chat.id, row.id, resolved.ext);
        let file_id = match self
            .storage
            .upload_file(&up.storage, &provider_name, &resolved.mime, bytes.clone())
            .await
        {
            Ok(id) => id,
            Err(e) => {
                log_failure("provider file upload failed", row.id, &e);
                self.mark_failed(&up, row.id, "upload_failed").await;
                return Err(DomainError::StorageUnavailable);
            }
        };
        AttachmentRepo
            .mark_uploaded(
                &self.db.conn()?,
                &up.scope,
                up.chat.id,
                row.id,
                &file_id,
                self.clock.now(),
            )
            .await?;

        let uploaded = Uploaded {
            up: &up,
            row: &row,
            file_id,
            started,
        };
        if resolved.kind == AttachmentKind::Image {
            self.secondary_copy(&up, row.id, &provider_name, &resolved.mime, &bytes)
                .await;
            self.finish_image(&uploaded, bytes).await?;
        } else if uses_vector_store {
            self.index_document(&uploaded).await?;
        } else {
            self.set_ready(&uploaded, None).await?;
        }
        let row = AttachmentRepo
            .find_in_chat(&self.db.conn()?, &up.scope, up.chat.id, &[row.id])
            .await?
            .pop()
            .ok_or_else(|| DomainError::Internal("uploaded attachment row vanished".into()))?;
        Ok(AttachmentDetail::from_row(&row))
    }

    /// Attachment of the chat, not deleted, uploaded by the caller.
    ///
    /// # Errors
    /// `ChatNotFound`, `AttachmentNotFound`, authorization and database
    /// failures.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetail, DomainError> {
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::READ_ATTACHMENT, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let row = self
            .own_attachment(&conn, &scope, &chat, ctx.subject_id(), attachment_id)
            .await?
            .filter(|a| a.deleted_at.is_none())
            .ok_or(DomainError::AttachmentNotFound)?;
        Ok(AttachmentDetail::from_row(&row))
    }

    /// Delete an unreferenced attachment (D "Attachment Deletion", Phase 1):
    /// soft-delete + attachment cleanup outbox event in one transaction.
    /// Deleting an already deleted attachment succeeds without a new event.
    ///
    /// # Errors
    /// `ChatNotFound`, `AttachmentNotFound` (also another user's
    /// attachment, checked before idempotency), `AttachmentLocked`,
    /// authorization, outbox and database failures.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::DELETE_ATTACHMENT, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let row = self
            .own_attachment(&conn, &scope, &chat, ctx.subject_id(), attachment_id)
            .await?
            .ok_or(DomainError::AttachmentNotFound)?;
        if row.deleted_at.is_some() {
            return Ok(());
        }
        let now = self.clock.now();
        let secondary = self.secondary_cleanup(ctx.subject_id(), &chat, &row).await;
        let secondary_ref = match &secondary {
            SecondaryCleanup::Delete(r) => Some(r.clone()),
            SecondaryCleanup::Nothing | SecondaryCleanup::Skip { .. } => None,
        };
        let attachment_id = row.id;
        let outbox = Arc::clone(&self.outbox);
        let (wakes, deleted) = with_retry(&self.db, move |tx| {
            let (scope, row, outbox, secondary_ref) = (
                scope.clone(),
                row.clone(),
                Arc::clone(&outbox),
                secondary_ref.clone(),
            );
            Box::pin(async move {
                if MessageAttachmentRepo
                    .is_referenced(tx, &scope, row.chat_id, row.id)
                    .await?
                {
                    return Err(DomainError::AttachmentLocked);
                }
                let mut wakes = PendingWakes::new();
                let deleted = AttachmentRepo
                    .soft_delete(tx, &scope, row.chat_id, row.id, now)
                    .await?
                    == 1;
                if deleted {
                    let mut payload =
                        cleanup_payload(&row, AttachmentCleanupEventType::AttachmentDeleted, now);
                    payload.secondary_ref = secondary_ref.clone();
                    outbox
                        .enqueue_attachment_cleanup(tx, &payload, &mut wakes)
                        .await?;
                }
                Ok((wakes, deleted))
            })
        })
        .await?;
        wakes.fire_all();
        if let (
            true,
            SecondaryCleanup::Skip {
                provider_kind,
                file_id,
            },
        ) = (deleted, &secondary)
        {
            // Counted only once the delete committed.
            self.metrics.secondary_cleanup_skipped(provider_kind);
            warn!(%attachment_id, secondary_file_id = %file_id, "secondary file cleanup skipped: the chat's model is not served by an Anthropic provider");
        }
        Ok(())
    }

    /// The attachment `id` of `chat` uploaded by `user` (deleted rows
    /// included); another user's attachment is `None`.
    async fn own_attachment(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat: &chat::Model,
        user: Uuid,
        id: Uuid,
    ) -> Result<Option<attachment::Model>, DomainError> {
        Ok(AttachmentRepo
            .find_in_chat(runner, scope, chat.id, &[id])
            .await?
            .into_iter()
            .find(|a| a.uploaded_by_user_id == user))
    }

    /// Per-chat limits and the `pending` row, in one transaction.
    async fn insert_pending(
        &self,
        ctx: &SecurityContext,
        up: &UploadContext,
        filename: &str,
        resolved: &ResolvedMime,
        size: usize,
    ) -> Result<attachment::Model, DomainError> {
        let now = self.clock.now();
        let row = attachment::Model {
            id: Uuid::new_v4(),
            tenant_id: up.chat.tenant_id,
            chat_id: up.chat.id,
            uploaded_by_user_id: ctx.subject_id(),
            filename: filename.to_owned(),
            content_type: resolved.mime.clone(),
            size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
            storage_backend: up.storage.storage_backend.clone(),
            provider_file_id: None,
            status: "pending".to_owned(),
            error_code: None,
            attachment_kind: resolved.kind.as_str().to_owned(),
            for_file_search: resolved.for_file_search,
            for_code_interpreter: resolved.for_code_interpreter && up.code_interpreter_available,
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
        let rag = &self.config.rag;
        let (max_docs, max_bytes) = (
            u64::from(rag.max_documents_per_chat),
            u64::from(rag.max_total_upload_mb_per_chat) * MIB,
        );
        let is_document = resolved.kind == AttachmentKind::Document;
        let scope = up.scope.clone();
        with_retry(&self.db, move |tx| {
            let (scope, row) = (scope.clone(), row.clone());
            Box::pin(async move {
                let (docs, bytes) = AttachmentRepo.chat_usage(tx, &scope, row.chat_id).await?;
                if is_document && docs >= max_docs {
                    return Err(DomainError::DocumentLimit);
                }
                let size = u64::try_from(row.size_bytes).unwrap_or(u64::MAX);
                if bytes.saturating_add(size) > max_bytes {
                    return Err(DomainError::StorageLimit);
                }
                Ok(AttachmentRepo.insert(tx, &scope, row).await?)
            })
        })
        .await
    }

    /// Secondary copy of an image in the Anthropic Files API when the chat's
    /// model is served by an `anthropic_messages` provider and the image is
    /// at most `thumbnail.max_decode_bytes` (best effort: a failure is
    /// recorded as `secondary_status = failed` and the upload goes on).
    async fn secondary_copy(
        &self,
        up: &UploadContext,
        id: Uuid,
        filename: &str,
        content_type: &str,
        bytes: &Bytes,
    ) {
        let Some(files) = &self.secondary else {
            return;
        };
        let Ok(target) = self
            .providers
            .resolve(&up.model.provider_id, up.chat.tenant_id)
        else {
            return;
        };
        if target.kind != ProviderKind::AnthropicMessages {
            return;
        }
        if bytes.len() > self.config.thumbnail.max_decode_bytes {
            debug!(attachment_id = %id, "image above thumbnail.max_decode_bytes: no secondary copy");
            return;
        }
        self.set_secondary(up, id, "pending", None).await;
        match files
            .upload(&target.alias, filename, content_type, bytes.clone())
            .await
        {
            Ok(file_id) => {
                self.set_secondary(up, id, "uploaded", Some(&file_id)).await;
            }
            Err(e) => {
                log_failure("secondary (Anthropic) file upload failed", id, &e);
                self.set_secondary(up, id, "failed", None).await;
            }
        }
    }

    async fn set_secondary(
        &self,
        up: &UploadContext,
        id: Uuid,
        status: &str,
        file_id: Option<&str>,
    ) {
        let result = match self.db.conn() {
            Ok(conn) => AttachmentRepo
                .set_secondary(
                    &conn,
                    &up.scope,
                    up.chat.id,
                    id,
                    status,
                    file_id,
                    self.clock.now(),
                )
                .await
                .map_err(DomainError::from),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            log_failure("secondary file state update failed", id, &e);
        }
    }

    /// Secondary copy cleanup of a row being deleted: delete it through the
    /// alias of the chat model's provider (resolved like the upload) when
    /// that provider is still `anthropic_messages`, otherwise skip it.
    async fn secondary_cleanup(
        &self,
        user: Uuid,
        chat: &chat::Model,
        row: &attachment::Model,
    ) -> SecondaryCleanup {
        let Some(file_id) = row
            .secondary_file_id
            .clone()
            .filter(|_| row.secondary_status == "uploaded")
        else {
            return SecondaryCleanup::Nothing;
        };
        let provider_kind = row
            .secondary_provider_kind
            .clone()
            .unwrap_or_else(|| "anthropic".to_owned());
        let alias = match self.models.resolve_chat_model(user, &chat.model).await {
            Ok((_, model)) => self.providers.alias_if_kind(
                &model.provider_id,
                ProviderKind::AnthropicMessages,
                chat.tenant_id,
            ),
            Err(e) => {
                warn!(attachment_id = %row.id, error = %e, "chat model not resolved for the secondary file cleanup");
                None
            }
        };
        match alias {
            Some(upstream_alias) => SecondaryCleanup::Delete(SecondaryRef {
                file_id,
                provider_kind,
                upstream_alias,
            }),
            None => SecondaryCleanup::Skip {
                provider_kind,
                file_id,
            },
        }
    }

    /// Thumbnail (best effort) and `ready`.
    async fn finish_image(&self, u: &Uploaded<'_>, bytes: Bytes) -> Result<(), DomainError> {
        let cfg = self.config.thumbnail.clone();
        let thumb = tokio::task::spawn_blocking(move || thumbnail::make_thumbnail(&bytes, &cfg))
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "thumbnail task failed");
                None
            })
            .and_then(|t| {
                Some((
                    t.webp,
                    i32::try_from(t.width).ok()?,
                    i32::try_from(t.height).ok()?,
                ))
            });
        self.set_ready(u, thumb).await
    }

    /// 409 when the chat's vector store belongs to another backend
    /// (checked before the row is inserted).
    async fn check_store_backend(&self, up: &UploadContext) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        match VectorStoreRepo
            .find_by_chat(&conn, &up.scope, up.chat.id)
            .await?
        {
            Some(vs) if vs.provider != up.storage.storage_backend => {
                Err(DomainError::ProviderMismatch)
            }
            _ => Ok(()),
        }
    }

    /// `uploaded` → `ready`.
    async fn set_ready(
        &self,
        u: &Uploaded<'_>,
        thumbnail: Option<(Vec<u8>, i32, i32)>,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        AttachmentRepo
            .mark_ready(
                &conn,
                &u.up.scope,
                u.up.chat.id,
                u.row.id,
                thumbnail,
                self.clock.now(),
            )
            .await?;
        Ok(())
    }

    /// Vector store, add the file, wait for indexing until the deadline
    /// (D "File Upload"). Leaves the row `ready`, `uploaded` (background
    /// task spawned) or `failed`.
    async fn index_document(&self, u: &Uploaded<'_>) -> Result<(), DomainError> {
        let vs = match self.ensure_vector_store(u.up).await {
            Ok(vs) => vs,
            Err(e) => {
                log_failure("chat vector store unavailable", u.row.id, &e);
                self.fail_and_delete_file(u, "vector_store_failed").await;
                return Err(match e {
                    DomainError::ProviderMismatch => e,
                    _ => DomainError::StorageUnavailable,
                });
            }
        };
        let added = self
            .storage
            .add_file_to_vector_store(&u.up.storage, &vs, &u.file_id, u.row.id)
            .await
            .map_err(|e| log_failure("vector store file add failed", u.row.id, &e));
        let status = match added {
            Ok(first) => self.poll_until_deadline(u, &vs, first).await,
            Err(()) => IndexStatus::Failed,
        };
        match status {
            IndexStatus::Completed => self.set_ready(u, None).await,
            IndexStatus::Failed => {
                self.fail_and_delete_file(u, "indexing_failed").await;
                Err(DomainError::StorageUnavailable)
            }
            IndexStatus::InProgress => {
                self.spawn_background_indexing(u, vs);
                Ok(())
            }
        }
    }

    /// Poll the indexing status from `status` on until it is terminal or the
    /// upload deadline passes (`InProgress`). Transient read errors keep
    /// polling; other read errors count as `Failed`.
    async fn poll_until_deadline(
        &self,
        u: &Uploaded<'_>,
        vs: &str,
        mut status: IndexStatus,
    ) -> IndexStatus {
        let deadline = u.started + self.timings.deadline;
        let mut wait = self.timings.poll_initial;
        while status == IndexStatus::InProgress {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            tokio::time::sleep(wait.min(left)).await;
            wait = (wait * 2).min(self.timings.poll_max);
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let read = self
                .storage
                .vector_store_file_status(&u.up.storage, vs, &u.file_id);
            status = match tokio::time::timeout(left, read).await {
                Err(_elapsed) => break,
                Ok(Ok(s)) => s,
                Ok(Err(StorageError::Transient(m))) => {
                    debug!(error = %m, attachment_id = %u.row.id, "transient indexing status error");
                    IndexStatus::InProgress
                }
                Ok(Err(e)) => {
                    log_failure("indexing status read failed", u.row.id, &e);
                    IndexStatus::Failed
                }
            };
        }
        status
    }

    fn spawn_background_indexing(&self, u: &Uploaded<'_>, vector_store_id: String) {
        // Detached: it ends on its own or with the gear's shutdown token.
        let _detached = background_indexing::spawn(
            IndexingDeps {
                db: Arc::clone(&self.db),
                clock: Arc::clone(&self.clock),
                storage: Arc::clone(&self.storage),
                outbox: Arc::clone(&self.outbox),
                timings: self.timings,
                metrics: Arc::clone(&self.metrics),
            },
            IndexingJob {
                tenant_id: u.row.tenant_id,
                chat_id: u.row.chat_id,
                attachment_id: u.row.id,
                attachment_kind: u.row.attachment_kind.clone(),
                storage_backend: u.row.storage_backend.clone(),
                vector_store_id,
                provider_file_id: u.file_id.clone(),
                storage: u.up.storage.clone(),
            },
            self.shutdown.child_token(),
        );
    }

    async fn fail_and_delete_file(&self, u: &Uploaded<'_>, code: &str) {
        self.mark_failed(u.up, u.row.id, code).await;
        // Fire-and-forget, not retried (D "File Upload").
        let (storage, target, file_id) = (
            Arc::clone(&self.storage),
            u.up.storage.clone(),
            u.file_id.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = storage.delete_file(&target, &file_id).await {
                warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    /// Record a failure on the row; a database error is logged (the
    /// original error is returned to the client).
    async fn mark_failed(&self, up: &UploadContext, id: Uuid, code: &str) {
        let result = async {
            let conn = self.db.conn()?;
            AttachmentRepo
                .mark_failed(&conn, &up.scope, up.chat.id, id, code, self.clock.now())
                .await?;
            Ok::<(), DomainError>(())
        };
        if let Err(e) = result.await {
            log_failure("could not mark attachment failed", id, &e);
        }
    }

    /// The chat's provider vector store, created on first use (D§3.7
    /// creation protocol: placeholder insert, loser polling, CAS, stale
    /// placeholder reclaim). No transaction is held across provider calls.
    async fn ensure_vector_store(&self, up: &UploadContext) -> Result<String, DomainError> {
        let backend = &up.storage.storage_backend;
        // At most: reclaim a stale placeholder, then insert or lose a race.
        for _ in 0..3 {
            let conn = self.db.conn()?;
            if let Some(row) = VectorStoreRepo
                .find_by_chat(&conn, &up.scope, up.chat.id)
                .await?
            {
                if row.provider != *backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = row.vector_store_id {
                    return Ok(id);
                }
                if self.clock.now() - row.created_at > STALE_PLACEHOLDER {
                    warn!(chat_id = %up.chat.id, "reclaiming stale vector store placeholder");
                    VectorStoreRepo
                        .delete_placeholder(&conn, &up.scope, up.chat.id, row.id)
                        .await?;
                    continue;
                }
                return self.wait_for_store(up).await;
            }
            let placeholder = chat_vector_store::Model {
                id: Uuid::new_v4(),
                tenant_id: up.chat.tenant_id,
                chat_id: up.chat.id,
                vector_store_id: None,
                provider: backend.clone(),
                file_count: 0,
                created_at: self.clock.now(),
            };
            let row_id = placeholder.id;
            match VectorStoreRepo.insert(&conn, &up.scope, placeholder).await {
                Ok(_) => return self.create_store(up, row_id).await,
                Err(e) if e.is_unique_violation() => return self.wait_for_store(up).await,
                Err(e) => return Err(e.into()),
            }
        }
        Err(DomainError::StorageUnavailable)
    }

    /// Winner path: create the provider store and CAS its id into the
    /// placeholder `row_id`.
    async fn create_store(&self, up: &UploadContext, row_id: Uuid) -> Result<String, DomainError> {
        let created = self
            .storage
            .create_vector_store(&up.storage, up.chat.id)
            .await;
        let Ok(vs) = created else {
            warn!(chat_id = %up.chat.id, "vector store creation failed");
            self.remove_placeholder(up, row_id).await;
            return Err(DomainError::StorageUnavailable);
        };
        let conn = self.db.conn()?;
        let set = VectorStoreRepo
            .set_vector_store_id(&conn, &up.scope, up.chat.id, row_id, &vs)
            .await?;
        if set == 1 {
            return Ok(vs);
        }
        // The placeholder was reclaimed meanwhile: drop our store and use
        // the one the chat has now.
        self.discard_store(up, &vs).await;
        self.wait_for_store(up).await
    }

    /// Best-effort delete of a store that lost the CAS.
    async fn discard_store(&self, up: &UploadContext, vs: &str) {
        warn!(chat_id = %up.chat.id, "vector store placeholder lost; deleting the new store");
        if let Err(e) = self.storage.delete_vector_store(&up.storage, vs).await {
            warn!(error = %e, "best-effort vector store delete failed");
        }
    }

    /// Best-effort delete of our placeholder after a failed creation.
    async fn remove_placeholder(&self, up: &UploadContext, row_id: Uuid) {
        let result = async {
            let conn = self.db.conn()?;
            VectorStoreRepo
                .delete_placeholder(&conn, &up.scope, up.chat.id, row_id)
                .await?;
            Ok::<(), DomainError>(())
        };
        if let Err(e) = result.await {
            warn!(error = %e, chat_id = %up.chat.id, "could not delete the vector store placeholder");
        }
    }

    /// Loser path: poll the chat's row until its store id is set
    /// ([`LOSER_POLLS`] polls with doubling waits), else 503.
    async fn wait_for_store(&self, up: &UploadContext) -> Result<String, DomainError> {
        let mut wait = self.timings.vector_store_poll_initial;
        for _ in 0..LOSER_POLLS {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.db.conn()?;
            if let Some(row) = VectorStoreRepo
                .find_by_chat(&conn, &up.scope, up.chat.id)
                .await?
            {
                if row.provider != up.storage.storage_backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = row.vector_store_id {
                    return Ok(id);
                }
            }
        }
        Err(DomainError::StorageUnavailable)
    }
}

/// Log a failed step of an upload (returns `()` for `map_err`).
fn log_failure(what: &str, attachment_id: Uuid, e: &dyn Display) {
    warn!(error = %e, attachment_id = %attachment_id, "{what}");
}

/// Attachment cleanup outbox payload of a row.
pub(crate) fn cleanup_payload(
    row: &attachment::Model,
    event_type: AttachmentCleanupEventType,
    now: time::OffsetDateTime,
) -> AttachmentCleanupPayload {
    AttachmentCleanupPayload {
        event_type,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        attachment_id: row.id,
        provider_file_id: row.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: row.storage_backend.clone(),
        attachment_kind: row.attachment_kind.clone(),
        deleted_at: now,
        secondary_ref: None,
    }
}

/// Buffer the part, rejecting it as soon as it exceeds `limit` bytes.
async fn read_limited<S, E>(stream: S, limit: u64) -> Result<Bytes, DomainError>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Display,
{
    let mut stream = std::pin::pin!(stream);
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            debug!(error = %e, "upload body read failed");
            DomainError::Multipart {
                field: "multipart",
                reason: "MULTIPART_ERROR",
            }
        })?;
        if (buf.len() + chunk.len()) as u64 > limit {
            return Err(DomainError::FileTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}
