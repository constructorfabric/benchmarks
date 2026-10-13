//! The attachments service: upload, get and delete behind the PEP
//! (`cpt-cf-mini-chat-flow-attachments-upload-attachment`,
//! `-get-attachment`, `-delete-attachment`), the provider storage and indexing
//! steps, the background indexing continuation and the failure handling.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::classify::{classify_file, normalize_filename, size_limit_bytes};
use super::detail::project_detail;
use super::model::{
    AttachmentDetail, AttachmentKind, AttachmentStatus, ChatLimits, Classification, ClassifyRules,
    DeleteOutcome, FailOutcome, FailWithCleanup, FailureCode, NewAttachment, ReadyOutcome,
    SecondaryUpdate, SoftDelete, StorageTarget, StoreRow, StoredAttachment, Thumbnail,
    UploadTargets,
};
use super::payload::{CleanupEvent, CleanupMessage, SecondaryRef};
use super::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundResult, BackgroundRuntime, FileUpload,
    IndexStatus, StorageClient, StorageDirectory, UploadResult,
};
use super::source::{MultipartSource, read_file_content};
use super::thumbnail::generate_thumbnail;
use crate::config::{MiniChatConfig, RagConfig, ThumbnailConfig};
use crate::domain::authz::{AuthzService, ChatAction};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::model::Chat;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::quota::service::Clock;

/// `UPLOAD_INDEXING_DEADLINE`: every provider call of an upload request and the
/// indexing wait end this long after the upload start.
pub const UPLOAD_INDEXING_DEADLINE: Duration = Duration::from_secs(25);
/// First wait between two indexing status reads of the request (doubles up to
/// [`STATUS_WAIT_MAX`]).
pub const STATUS_WAIT_INITIAL: Duration = Duration::from_millis(250);
/// Largest wait between two status reads of the request.
pub const STATUS_WAIT_MAX: Duration = Duration::from_secs(2);
/// `BACKGROUND_INDEXING_LIMIT`: the background continuation stops after this.
pub const BACKGROUND_INDEXING_LIMIT: Duration = Duration::from_secs(600);
/// One round of the background continuation (heartbeat, then polling).
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
/// Largest wait between two status reads of the background continuation.
pub const BACKGROUND_WAIT_MAX: Duration = Duration::from_secs(5);
/// The waits between the 4 attempts of the background `ready` write.
pub const SET_READY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// The waits between the 5 polls of a vector store loser.
pub const STORE_POLL_WAITS: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// A placeholder row older than this is reclaimed.
pub const PLACEHOLDER_STALE_AFTER: Duration = Duration::from_secs(120);
/// `PROVIDER_DELETE_TIMEOUT`: each provider or secondary file delete.
pub const PROVIDER_DELETE_TIMEOUT: Duration = Duration::from_secs(10);
/// Name of the background indexing tasks on the gear's registry.
pub const BACKGROUND_INDEXING_TASK: &str = "attachment-background-indexing";
/// Name of the best-effort delete tasks on the gear's registry.
pub const BEST_EFFORT_DELETE_TASK: &str = "attachment-best-effort-delete";

const MIB: u64 = 1_048_576;

/// The configuration the service reads.
#[derive(Debug, Clone)]
pub struct AttachmentSettings {
    /// `rag.*`.
    pub rag: RagConfig,
    /// `thumbnail.*`.
    pub thumbnail: ThumbnailConfig,
}

impl AttachmentSettings {
    /// Takes the sections from the validated configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            rag: config.rag.clone(),
            thumbnail: config.thumbnail.clone(),
        }
    }
}

/// Everything the service is built from.
pub struct AttachmentsDeps {
    /// The PEP (`upload_attachment`, `read_attachment`, `delete_attachment`).
    pub authz: Arc<AuthzService>,
    /// Chat loading through the compiled scope.
    pub chats: Arc<dyn ChatRepository>,
    /// Model policy (catalog and kill switches).
    pub policy: Arc<dyn PolicyCatalog>,
    /// Provider entries as storage targets.
    pub directory: Arc<dyn StorageDirectory>,
    /// Attachment store.
    pub repo: Arc<dyn AttachmentRepository>,
    /// Provider Files and Vector Stores APIs.
    pub storage: Arc<dyn StorageClient>,
    /// Instruments.
    pub metrics: Arc<dyn AttachmentMetrics>,
    /// Background tasks and the S2S context.
    pub runtime: Arc<dyn BackgroundRuntime>,
    /// Configuration.
    pub settings: AttachmentSettings,
    /// Wall clock for row timestamps and the placeholder age.
    pub clock: Clock,
}

/// Upload, get and delete of chat attachments.
pub struct AttachmentsService {
    deps: AttachmentsDeps,
    permits: Arc<Semaphore>,
}

/// What the catalog and the configuration say about an upload into one chat.
struct UploadContext {
    targets: UploadTargets,
    rules: ClassifyRules,
    document_limit: u64,
    image_limit: u64,
}

/// Why an upload failed after its row exists.
#[derive(Debug)]
enum FailCause {
    /// A provider call failed, timed out or answered unusably.
    Provider,
    /// The chat vector store belongs to another backend.
    Mismatch,
    /// A database write failed.
    Database(DomainError),
}

/// The state of one accepted upload after its row was inserted.
struct Flow {
    row: StoredAttachment,
    targets: UploadTargets,
    file: FileUpload,
    secondary_ref: Option<SecondaryRef>,
    deadline: Instant,
}

/// Counts the pending gauge down when the request leaves `pending` or ends.
struct PendingGauge {
    metrics: Arc<dyn AttachmentMetrics>,
    released: bool,
}

impl PendingGauge {
    fn new(metrics: Arc<dyn AttachmentMetrics>) -> Self {
        metrics.pending_add(1);
        Self {
            metrics,
            released: false,
        }
    }

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.metrics.pending_add(-1);
        }
    }
}

impl Drop for PendingGauge {
    fn drop(&mut self) {
        self.release();
    }
}

/// `rejected` for a 4xx or 429 decided by this feature, `failed` otherwise.
fn result_of(error: &DomainError) -> UploadResult {
    match error {
        DomainError::InvalidMultipart(_)
        | DomainError::FileTooLarge
        | DomainError::DocumentLimit
        | DomainError::StorageLimit
        | DomainError::ProviderMismatch
        | DomainError::NotFound { .. }
        | DomainError::AttachmentLocked => UploadResult::Rejected,
        _ => UploadResult::Failed,
    }
}

impl AttachmentsService {
    /// Creates the service.
    #[must_use]
    pub fn new(deps: AttachmentsDeps) -> Self {
        let permits = usize::from(deps.settings.rag.max_concurrent_uploads.max(1));
        Self {
            deps,
            permits: Arc::new(Semaphore::new(permits)),
        }
    }

    fn now(&self) -> OffsetDateTime {
        (self.deps.clock)()
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-attachments-upload-endpoint:p1
    /// `POST /v1/chats/{id}/attachments`.
    ///
    /// Check order: PEP, chat load, chat-model resolution (`InvalidModel`),
    /// the concurrency permit (`UploadConcurrencyLimit`), multipart framing,
    /// classification, size, the `provider_mismatch` pre-check and the pending
    /// insert with the per-chat limits; nothing of the body is read before the
    /// permit is held.
    ///
    /// # Errors
    ///
    /// Every row of the upload error table of the FEATURE.
    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        source: &mut dyn MultipartSource,
    ) -> Result<AttachmentDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-upload-start
        let started = Instant::now();
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-upload-start
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-authorize-upload
        let scope = self
            .deps
            .authz
            .chat_scope(ctx, ChatAction::UploadAttachment, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-authorize-upload
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-load-chat
        let chat = self.deps.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-load-chat
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-resolve-context
        let upload = self.upload_context(ctx, &chat).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-resolve-context
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-acquire-permit
        let _permit = Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-acquire-permit
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-read-multipart
        let header = source
            .open_file()
            .await
            .map_err(DomainError::InvalidMultipart)?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-read-multipart
        let filename = normalize_filename(header.filename.as_deref());
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-classify
        let class = classify_file(&header.content_type, &filename, &upload.rules)?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-classify
        let kind = class.kind;
        let result = self
            .upload_classified(ctx, &chat, source, class, filename, upload, started)
            .await;
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-record-upload
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-count-upload
        match &result {
            Ok(_) => self.deps.metrics.upload(kind, UploadResult::Ok),
            Err(error) => self.deps.metrics.upload(kind, result_of(error)),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-count-upload
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-record-upload
        result
    }

    /// Steps 9 to 19 of the upload flow: everything after classification.
    #[allow(clippy::too_many_arguments)]
    async fn upload_classified(
        &self,
        ctx: &SecurityContext,
        chat: &Chat,
        source: &mut dyn MultipartSource,
        class: Classification,
        filename: String,
        upload: UploadContext,
        started: Instant,
    ) -> Result<AttachmentDetail, DomainError> {
        let limit = if class.kind == AttachmentKind::Image {
            upload.image_limit
        } else {
            upload.document_limit
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-read-content
        let bytes = read_file_content(source, limit).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-read-content
        let backend = upload.targets.storage.backend.clone();
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-precheck-provider
        if class.for_file_search {
            let existing = self.deps.repo.vector_store(chat.tenant_id, chat.id).await?;
            if existing.is_some_and(|store| store.provider != backend) {
                return Err(DomainError::ProviderMismatch);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-precheck-provider
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-pregenerate-attachment
        let new = NewAttachment {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            user_id: ctx.subject_id(),
            filename,
            content_type: class.content_type.clone(),
            size_bytes: size,
            storage_backend: backend,
            kind: class.kind,
            for_file_search: class.for_file_search,
            for_code_interpreter: class.for_code_interpreter,
            now: self.now(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-pregenerate-attachment
        let limits = ChatLimits {
            max_documents: self.deps.settings.rag.max_documents_per_chat,
            max_total_bytes: u64::from(self.deps.settings.rag.max_total_upload_mb_per_chat) * MIB,
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-insert-pending
        let row = self.deps.repo.insert_pending(&new, &limits).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-insert-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-pending-gauge
        let mut gauge = PendingGauge::new(Arc::clone(&self.deps.metrics));
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-pending-gauge
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-upload-bytes
        self.deps
            .metrics
            .upload_bytes(class.kind, u64::try_from(size).unwrap_or(0));
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-upload-bytes
        let attachment_id = row.id;
        let mut flow = Flow {
            row,
            targets: upload.targets,
            file: FileUpload {
                chat_id: chat.id,
                attachment_id,
                content_type: class.content_type,
                extension: class.extension.to_owned(),
                bytes,
            },
            secondary_ref: None,
            deadline: started + UPLOAD_INDEXING_DEADLINE,
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-upload-file
        self.store_file(ctx, &mut flow, &mut gauge).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-upload-file
        self.process(ctx, flow).await
    }

    /// Steps 12 of the flow: the provider file upload and the `uploaded` CAS.
    async fn store_file(
        &self,
        ctx: &SecurityContext,
        flow: &mut Flow,
        gauge: &mut PendingGauge,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-if-files-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-return-upload-failed
        let Some(file_id) = self.upload_to_provider(ctx, flow).await else {
            return Err(self
                .fail(flow, FailureCode::UploadFailed, FailCause::Provider, None)
                .await);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-return-upload-failed
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-if-files-failed
        // A document is never read again (indexing works on the provider file): free
        // its content now. An image keeps it for the secondary copy and the thumbnail.
        if flow.row.kind != AttachmentKind::Image {
            flow.file.bytes = Bytes::new();
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-set-uploaded
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-to-uploaded
        match self
            .deps
            .repo
            .set_uploaded(flow.row.tenant_id, flow.row.id, &file_id, self.now())
            .await
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-return-uploaded
            Ok(true) => {
                flow.row.status = AttachmentStatus::Uploaded;
                flow.row.provider_file_id = Some(file_id);
                gauge.release();
                Ok(())
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-return-uploaded
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-if-uploaded-lost
            Ok(false) => Err(self.lost_upload(&flow.row, &flow.targets, &file_id).await),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-if-uploaded-lost
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-if-uploaded-db-error
            Err(error) => Err(self
                .fail(
                    flow,
                    FailureCode::UploadFailed,
                    FailCause::Database(error),
                    Some(file_id),
                )
                .await),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-if-uploaded-db-error
            // @cpt-end:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-to-uploaded
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-set-uploaded
        }
    }

    /// The single provider file upload, bounded by the request deadline.
    async fn upload_to_provider(&self, ctx: &SecurityContext, flow: &Flow) -> Option<String> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-call-files
        let uploaded = tokio::time::timeout_at(
            flow.deadline,
            self.deps
                .storage
                .upload_file(ctx, &flow.targets.storage, &flow.file),
        )
        .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-call-files
        match uploaded {
            Ok(Ok(id)) => Some(id),
            Ok(Err(error)) => {
                tracing::warn!(attachment_id = %flow.row.id, chat_id = %flow.row.chat_id, cause = %error.cause(), "the provider file upload failed");
                None
            }
            Err(_elapsed) => {
                tracing::warn!(attachment_id = %flow.row.id, "the provider file upload timed out");
                None
            }
        }
    }

    /// The `pending` to `uploaded` CAS lost: the new file gets a durable deleter.
    async fn lost_upload(
        &self,
        row: &StoredAttachment,
        targets: &UploadTargets,
        file_id: &str,
    ) -> DomainError {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-enqueue-orphan-file
        let message = CleanupMessage::orphan(row, file_id, self.now());
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-orphan-enqueue-failed
        if let Err(error) = self.deps.repo.enqueue_orphan_file(&message).await {
            tracing::error!(attachment_id = %row.id, %error, "the orphan file message could not be enqueued");
            self.spawn_file_delete(targets.storage.clone(), file_id.to_owned());
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-orphan-enqueue-failed
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-enqueue-orphan-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-return-taken
        let chat_deleted = self
            .deps
            .repo
            .chat_deleted(row.tenant_id, row.chat_id)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
        DomainError::NotFound {
            resource: if chat_deleted {
                ResourceKind::Chat
            } else {
                ResourceKind::Attachment
            },
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-provider-file:p1:inst-return-taken
    }

    /// Steps 13 to 17 of the flow.
    async fn process(
        &self,
        ctx: &SecurityContext,
        mut flow: Flow,
    ) -> Result<AttachmentDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-secondary-copy
        if flow.row.kind == AttachmentKind::Image
            && let Some(alias) = flow.targets.secondary_alias.clone()
            && flow.file.bytes.len() <= self.deps.settings.thumbnail.max_decode_bytes
        {
            self.secondary_copy(ctx, &mut flow, &alias).await;
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-secondary-copy
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-case-code-interpreter
        let mut thumbnail = None;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-case-code-interpreter
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-match-path
        if flow.row.for_file_search {
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-case-file-search
            match self.index(ctx, &flow).await {
                // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-if-indexing-failed
                Err(failure) => {
                    return Err(self.fail(&flow, failure.code, failure.cause, None).await);
                }
                // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-if-indexing-failed
                // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-if-indexing-pending
                Ok(Indexed::Pending(store)) => {
                    self.spawn_background_indexing(&flow, store);
                    return Ok(project_detail(&flow.row));
                }
                // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-if-indexing-pending
                Ok(Indexed::Ready) => {}
            }
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-case-file-search
        } else if flow.row.kind == AttachmentKind::Image {
            // The file bytes are not needed after this step: hand them over, no copy.
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-case-image
            let bytes = std::mem::take(&mut flow.file.bytes);
            thumbnail = generate_thumbnail(bytes, &self.deps.settings.thumbnail).await;
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-case-image
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-match-path
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-mark-ready
        self.mark_ready(flow, thumbnail).await
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-mark-ready
    }

    /// Step 17: `uploaded` to `ready`.
    async fn mark_ready(
        &self,
        mut flow: Flow,
        thumbnail: Option<Thumbnail>,
    ) -> Result<AttachmentDetail, DomainError> {
        match self
            .deps
            .repo
            .set_ready(
                flow.row.tenant_id,
                flow.row.id,
                thumbnail.as_ref(),
                self.now(),
            )
            .await
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-return-ready
            Ok(ReadyOutcome::Ready) => {
                flow.row.status = AttachmentStatus::Ready;
                flow.row.thumbnail = thumbnail;
                Ok(project_detail(&flow.row))
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-return-ready
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-return-ready-taken
            Ok(ReadyOutcome::Taken { chat_deleted }) => Err(DomainError::NotFound {
                resource: if chat_deleted {
                    ResourceKind::Chat
                } else {
                    ResourceKind::Attachment
                },
            }),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-return-ready-taken
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-return-ready-db-error
            Err(error) => Err(self
                .fail(
                    &flow,
                    FailureCode::UploadFailed,
                    FailCause::Database(error),
                    None,
                )
                .await),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-return-ready-db-error
        }
    }

    /// The secondary Anthropic copy of an image; its outcome never fails the upload.
    async fn secondary_copy(&self, ctx: &SecurityContext, flow: &mut Flow, alias: &str) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-set-secondary-pending
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-secondary-copy:p1:inst-secondary-to-pending
        self.record_secondary(&flow.row, &SecondaryUpdate::Pending)
            .await;
        // @cpt-end:cpt-cf-mini-chat-state-attachments-secondary-copy:p1:inst-secondary-to-pending
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-set-secondary-pending
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-call-secondary
        let uploaded = tokio::time::timeout_at(
            flow.deadline,
            self.deps.storage.upload_secondary(ctx, alias, &flow.file),
        )
        .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-call-secondary
        if let Ok(Ok(file_id)) = uploaded {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-set-secondary-uploaded
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-secondary-copy:p1:inst-secondary-to-uploaded
            self.record_secondary(&flow.row, &SecondaryUpdate::Uploaded(file_id.clone()))
                .await;
            // @cpt-end:cpt-cf-mini-chat-state-attachments-secondary-copy:p1:inst-secondary-to-uploaded
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-set-secondary-uploaded
            "uploaded".clone_into(&mut flow.row.secondary_status);
            flow.row.secondary_file_id = Some(file_id.clone());
            flow.secondary_ref = Some(SecondaryRef {
                file_id,
                upstream_alias: alias.to_owned(),
            });
        } else {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-set-secondary-failed
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-secondary-copy:p1:inst-secondary-to-failed
            tracing::warn!(attachment_id = %flow.row.id, "the secondary copy of the image failed");
            self.record_secondary(&flow.row, &SecondaryUpdate::Failed)
                .await;
            // @cpt-end:cpt-cf-mini-chat-state-attachments-secondary-copy:p1:inst-secondary-to-failed
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-set-secondary-failed
        }
    }

    /// Records a secondary copy state; a failed write is only logged.
    async fn record_secondary(&self, row: &StoredAttachment, update: &SecondaryUpdate) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-return-secondary
        if let Err(error) = self
            .deps
            .repo
            .set_secondary(row.tenant_id, row.id, update, self.now())
            .await
        {
            tracing::warn!(attachment_id = %row.id, %error, "the secondary copy state could not be recorded");
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-upload-secondary-copy:p1:inst-return-secondary
    }

    // ---- the chat vector store ------------------------------------------------------------------

    // @cpt-dod:cpt-cf-mini-chat-dod-attachments-vector-store-indexing:p1
    /// `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`.
    async fn ensure_store(&self, ctx: &SecurityContext, flow: &Flow) -> Result<String, StoreError> {
        let repo = &self.deps.repo;
        let (tenant, chat) = (flow.row.tenant_id, flow.row.chat_id);
        let backend = &flow.targets.storage.backend;
        let mut reclaimed = false;
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-select-store
            let existing = repo.vector_store(tenant, chat).await.map_err(|error| {
                tracing::warn!(%error, "the vector store row could not be read");
                StoreError::Failed
            })?;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-select-store
            let Some(row) = existing else {
                return self.create_store(ctx, flow).await;
            };
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-fast-path
            if let Some(id) = &row.vector_store_id {
                return if row.provider == *backend {
                    Ok(id.clone())
                } else {
                    Err(StoreError::Mismatch)
                };
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-fast-path
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-reclaim-stale
            let age = self.now() - row.created_at;
            if !reclaimed
                && age > time::Duration::seconds(PLACEHOLDER_STALE_AFTER.as_secs().cast_signed())
            {
                repo.delete_placeholder(tenant, row.id)
                    .await
                    .map_err(|_| StoreError::Failed)?;
                reclaimed = true;
                continue;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-reclaim-stale
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-if-placeholder
            return self.poll_store(flow).await;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-if-placeholder
        }
    }

    /// The placeholder INSERT, the single provider create and the CAS.
    async fn create_store(&self, ctx: &SecurityContext, flow: &Flow) -> Result<String, StoreError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-insert-placeholder
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-to-creating
        let placeholder = StoreRow {
            id: Uuid::new_v4(),
            tenant_id: flow.row.tenant_id,
            chat_id: flow.row.chat_id,
            vector_store_id: None,
            provider: flow.targets.storage.backend.clone(),
            created_at: self.now(),
        };
        match self.deps.repo.insert_store_placeholder(&placeholder).await {
            Ok(()) => {}
            Err(DomainError::UniqueViolation) => return self.poll_store(flow).await,
            Err(error) => {
                tracing::warn!(%error, "the vector store placeholder could not be written");
                return Err(StoreError::Failed);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-to-creating
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-insert-placeholder
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-create-store
        let Some(store_id) = self.create_at_provider(ctx, flow).await else {
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-creating-to-absent
            if let Err(error) = self
                .deps
                .repo
                .delete_placeholder(placeholder.tenant_id, placeholder.id)
                .await
            {
                tracing::warn!(%error, "the vector store placeholder could not be removed");
            }
            // @cpt-end:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-creating-to-absent
            return Err(StoreError::Failed);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-create-store
        self.bind_created(flow, &placeholder, store_id).await
    }

    /// The CAS of the created store id onto the placeholder; a lost CAS or a
    /// failed write leaves the new store to a best-effort delete.
    async fn bind_created(
        &self,
        flow: &Flow,
        placeholder: &StoreRow,
        store_id: String,
    ) -> Result<String, StoreError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-cas-store-id
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-to-bound
        let bound = self
            .deps
            .repo
            .bind_store(placeholder.tenant_id, placeholder.id, &store_id)
            .await;
        if matches!(bound, Ok(true)) {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-return-new-store
            return Ok(store_id);
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-return-new-store
        }
        self.spawn_store_delete(flow.targets.storage.clone(), store_id);
        match bound {
            Ok(_) => self.poll_store(flow).await,
            Err(error) => {
                tracing::warn!(%error, "the vector store id could not be stored");
                Err(StoreError::Failed)
            }
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-chat-vector-store:p1:inst-to-bound
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-cas-store-id
    }

    /// The single provider vector store create, bounded by the request deadline.
    async fn create_at_provider(&self, ctx: &SecurityContext, flow: &Flow) -> Option<String> {
        let created = tokio::time::timeout_at(
            flow.deadline,
            self.deps
                .storage
                .create_vector_store(ctx, &flow.targets.storage, flow.row.chat_id),
        )
        .await;
        if let Ok(Ok(store_id)) = created {
            return Some(store_id);
        }
        tracing::warn!(chat_id = %flow.row.chat_id, "the vector store could not be created");
        None
    }

    /// A loser waits for the winner's store id: 5 polls, never past the deadline.
    async fn poll_store(&self, flow: &Flow) -> Result<String, StoreError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-for-each-poll
        for wait in STORE_POLL_WAITS {
            let left = flow.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(wait.min(left)).await;
            if Instant::now() >= flow.deadline {
                break;
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-poll-store
            let row = self
                .deps
                .repo
                .vector_store(flow.row.tenant_id, flow.row.chat_id)
                .await
                .map_err(|_| StoreError::Failed)?;
            if let Some(row) = row
                && let Some(id) = row.vector_store_id
            {
                return if row.provider == flow.targets.storage.backend {
                    Ok(id)
                } else {
                    Err(StoreError::Mismatch)
                };
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-poll-store
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-for-each-poll
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-return-poll-exhausted
        Err(StoreError::Failed)
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-ensure-vector-store:p1:inst-return-poll-exhausted
    }

    // ---- indexing -----------------------------------------------------------------------------------

    /// `cpt-cf-mini-chat-algo-attachments-index-document`, with the store resolution.
    async fn index(&self, ctx: &SecurityContext, flow: &Flow) -> Result<Indexed, Failure> {
        let store = self
            .ensure_store(ctx, flow)
            .await
            .map_err(|error| Failure {
                code: FailureCode::VectorStoreFailed,
                cause: match error {
                    StoreError::Mismatch => FailCause::Mismatch,
                    StoreError::Failed => FailCause::Provider,
                },
            })?;
        let indexing = Failure {
            code: FailureCode::IndexingFailed,
            cause: FailCause::Provider,
        };
        let file_id = flow.row.provider_file_id.clone().unwrap_or_default();
        let target = &flow.targets.storage;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-add-file
        let added = tokio::time::timeout_at(
            flow.deadline,
            self.deps
                .storage
                .add_file(ctx, target, &store, &file_id, flow.row.id),
        )
        .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-add-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-add-failed
        let Ok(Ok(mut status)) = added else {
            return Err(indexing);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-add-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-initial-status
        let mut wait = STATUS_WAIT_INITIAL;
        let mut warned = false;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-initial-status
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-while-in-progress
        while status == IndexStatus::InProgress {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-deadline
            let left = flow.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Indexed::Pending(store));
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-wait
            tokio::time::sleep(wait.min(left)).await;
            wait = (wait * 2).min(STATUS_WAIT_MAX);
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-wait
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-deadline
            if Instant::now() >= flow.deadline {
                continue;
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-read-status
            let read = tokio::time::timeout_at(
                flow.deadline,
                self.deps
                    .storage
                    .indexing_status(ctx, target, &store, &file_id),
            )
            .await;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-read-status
            match read {
                Err(_elapsed) => {}
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-transient
                Ok(Err(error)) if error.is_transient() => {
                    if !warned {
                        warned = true;
                        tracing::warn!(attachment_id = %flow.row.id, cause = %error.cause(), "a transient indexing status error");
                    }
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-read-error
                Ok(Err(_)) => return Err(indexing),
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-read-error
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-update-status
                Ok(Ok(next)) => status = next,
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-update-status
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-if-transient
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-while-in-progress
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-return-indexed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-return-index-failed
        if status == IndexStatus::Completed {
            Ok(Indexed::Ready)
        } else {
            Err(indexing)
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-return-index-failed
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-index-document:p1:inst-return-indexed
    }

    // ---- the failure of an upload ------------------------------------------------------------------

    /// `cpt-cf-mini-chat-algo-attachments-fail-upload`.
    async fn fail(
        &self,
        flow: &Flow,
        code: FailureCode,
        cause: FailCause,
        held: Option<String>,
    ) -> DomainError {
        let row = &flow.row;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-prepare-failure
        match row.provider_file_id.clone().or_else(|| held.clone()) {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-set-failed
            None => {
                if let Err(error) = self
                    .deps
                    .repo
                    .set_failed(row.tenant_id, row.id, code, self.now())
                    .await
                {
                    tracing::error!(attachment_id = %row.id, %error, "the failed upload could not be recorded");
                }
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-set-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-fail-with-cleanup
            Some(file) => self.fail_with_file(flow, code, file, held.is_some()).await,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-fail-with-cleanup
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-prepare-failure
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-log-failure
        tracing::warn!(attachment_id = %row.id, chat_id = %row.chat_id, code = code.as_str(), "an upload failed");
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-log-failure
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-match-cause
        match cause {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-case-mismatch
            FailCause::Mismatch => DomainError::ProviderMismatch,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-case-mismatch
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-case-db
            FailCause::Database(error) => error,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-case-db
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-case-storage
            FailCause::Provider => DomainError::StorageUnavailable,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-case-storage
        }
    }

    /// The failure of a row that has a provider file: one write-first transaction
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-match-cause
    /// with the cleanup message; `held` is a file the row could not take.
    async fn fail_with_file(&self, flow: &Flow, code: FailureCode, file: String, held: bool) {
        let row = &flow.row;
        let now = self.now();
        let event = if code == FailureCode::IndexingFailed {
            CleanupEvent::AttachmentIndexingFailed
        } else {
            CleanupEvent::AttachmentUploadFailed
        };
        let request = FailWithCleanup {
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            attachment_id: row.id,
            code,
            provider_file_id: file.clone(),
            now,
            message: CleanupMessage::for_row(
                event,
                row,
                Some(file.clone()),
                flow.secondary_ref.clone(),
                now,
            ),
            orphan: held.then(|| CleanupMessage::orphan(row, &file, now)),
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-best-effort-delete
        let outcome: Result<FailOutcome, DomainError> =
            self.deps.repo.fail_with_cleanup(&request).await;
        if let Err(error) = outcome {
            tracing::error!(attachment_id = %row.id, %error, "the failure of an upload could not be recorded durably");
            if held {
                self.spawn_file_delete(flow.targets.storage.clone(), file);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-best-effort-delete
    }

    // ---- tasks off the request ------------------------------------------------------------------------

    /// A best-effort provider file delete with the S2S context; not retried.
    fn spawn_file_delete(&self, target: StorageTarget, file_id: String) {
        let Some(ctx) = self.deps.runtime.s2s_context() else {
            tracing::warn!(
                "no S2S context: a provider file is left to the provider orphan reconciliation"
            );
            return;
        };
        let storage = Arc::clone(&self.deps.storage);
        self.deps.runtime.spawn(
            BEST_EFFORT_DELETE_TASK,
            Box::pin(async move {
                let outcome = tokio::time::timeout(
                    PROVIDER_DELETE_TIMEOUT,
                    storage.delete_file(&ctx, &target, &file_id),
                )
                .await;
                if !matches!(outcome, Ok(Ok(()))) {
                    tracing::warn!("a best-effort provider file delete failed");
                }
            }),
        );
    }

    /// A best-effort delete of an orphan, still empty provider vector store.
    fn spawn_store_delete(&self, target: StorageTarget, store_id: String) {
        let Some(ctx) = self.deps.runtime.s2s_context() else {
            return;
        };
        let storage = Arc::clone(&self.deps.storage);
        self.deps.runtime.spawn(
            BEST_EFFORT_DELETE_TASK,
            Box::pin(async move {
                let outcome = tokio::time::timeout(
                    PROVIDER_DELETE_TIMEOUT,
                    storage.delete_vector_store(&ctx, &target, &store_id),
                )
                .await;
                if !matches!(outcome, Ok(Ok(()))) {
                    tracing::warn!("the orphan vector store delete failed");
                }
            }),
        );
    }

    /// `cpt-cf-mini-chat-algo-attachments-background-indexing`.
    fn spawn_background_indexing(&self, flow: &Flow, store_id: String) {
        let job = BackgroundJob {
            repo: Arc::clone(&self.deps.repo),
            storage: Arc::clone(&self.deps.storage),
            metrics: Arc::clone(&self.deps.metrics),
            runtime: Arc::clone(&self.deps.runtime),
            clock: Arc::clone(&self.deps.clock),
            target: flow.targets.storage.clone(),
            store_id,
            row: flow.row.clone(),
        };
        let runtime = Arc::clone(&self.deps.runtime);
        if !runtime.spawn(BACKGROUND_INDEXING_TASK, Box::pin(run_background(job))) {
            tracing::warn!(attachment_id = %flow.row.id, "the gear has not started: the reaper finishes the row");
        }
    }

    // ---- get and delete -------------------------------------------------------------------------------

    // @cpt-dod:cpt-cf-mini-chat-dod-attachments-get-attachment:p1
    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    ///
    /// `NotFound` of the chat or of the attachment (unknown, soft-deleted, of
    /// another chat or uploaded by another user), `AuthzDenied`, `PdpUnavailable`.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-authorize-read
        let scope = self
            .deps
            .authz
            .chat_scope(ctx, ChatAction::ReadAttachment, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-authorize-read
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-load-attachment
        let chat = self.deps.chats.load(&scope, chat_id).await?;
        let row = self.deps.repo.find(&scope, chat.id, attachment_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-load-attachment
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-if-attachment-missing
        match row {
            Some(row)
                if row.deleted_at.is_none() && row.uploaded_by_user_id == ctx.subject_id() =>
            {
                // @cpt-begin:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-return-detail
                Ok(project_detail(&row))
                // @cpt-end:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-return-detail
            }
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-return-attachment-404
            _ => Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
            }),
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-return-attachment-404
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-if-attachment-missing
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-attachments-delete:p1
    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`: soft delete with the
    /// cleanup enqueue in one transaction; idempotent.
    ///
    /// # Errors
    ///
    /// `NotFound`, `AttachmentLocked`, `AuthzDenied`, `PdpUnavailable`, `Database`.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-authorize-delete
        let scope = self
            .deps
            .authz
            .chat_scope(ctx, ChatAction::DeleteAttachment, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-authorize-delete
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-load-chat-for-delete
        let chat = self.deps.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-load-chat-for-delete
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-select-for-delete
        let row = self.deps.repo.find(&scope, chat.id, attachment_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-select-for-delete
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-if-delete-missing
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-return-delete-404
        let Some(row) = row.filter(|row| row.uploaded_by_user_id == ctx.subject_id()) else {
            return Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
            });
        };
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-return-delete-404
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-if-delete-missing
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-if-already-deleted
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-return-repeat-204
        if row.deleted_at.is_some() {
            return Ok(());
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-return-repeat-204
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-if-already-deleted
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-resolve-secondary-ref
        let secondary_alias =
            if row.secondary_status == "uploaded" && row.secondary_file_id.is_some() {
                self.secondary_alias_of(ctx, &chat, &row).await
            } else {
                None
            };
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-resolve-secondary-ref
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-soft-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-prepare-delete
        let request = SoftDelete {
            chat_id: chat.id,
            attachment_id,
            secondary_alias,
            now: self.now(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-prepare-delete
        match self.deps.repo.soft_delete(&scope, &request).await? {
            DeleteOutcome::Locked => Err(DomainError::AttachmentLocked),
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-return-204
            DeleteOutcome::Deleted | DeleteOutcome::AlreadyDeleted => Ok(()),
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-return-204
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-soft-delete
    }

    /// Best effort: the Anthropic alias of the chat's provider.
    async fn secondary_alias_of(
        &self,
        ctx: &SecurityContext,
        chat: &Chat,
        row: &StoredAttachment,
    ) -> Option<String> {
        let resolved = self.catalog_entry(ctx, chat).await.and_then(|(entry, _)| {
            self.deps
                .directory
                .upload_targets(&entry.provider_id, chat.tenant_id)
        });
        let alias = resolved.ok().and_then(|targets| targets.secondary_alias);
        if alias.is_none() {
            tracing::warn!(attachment_id = %row.id, "the secondary copy cannot be referenced in the cleanup message");
            self.deps.metrics.secondary_cleanup_skipped();
        }
        alias
    }

    // ---- the upload context -------------------------------------------------------------------------

    async fn catalog_entry(
        &self,
        ctx: &SecurityContext,
        chat: &Chat,
    ) -> Result<(ModelCatalogEntry, KillSwitches), DomainError> {
        let user = ctx.subject_id();
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-read-snapshot
        let version = self.deps.policy.current_policy_version(user).await?;
        let snapshot = self
            .deps
            .policy
            .policy_snapshot(user, version.policy_version)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-read-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-find-chat-model
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == chat.model)
            .cloned()
            .ok_or(DomainError::InvalidModel)?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-find-chat-model
        Ok((entry, snapshot.kill_switches))
    }

    /// `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`.
    async fn upload_context(
        &self,
        ctx: &SecurityContext,
        chat: &Chat,
    ) -> Result<UploadContext, DomainError> {
        let (entry, switches) = self.catalog_entry(ctx, chat).await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-resolve-chat-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-resolve-storage-target
        let targets = self
            .deps
            .directory
            .upload_targets(&entry.provider_id, chat.tenant_id)?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-resolve-storage-target
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-resolve-chat-provider
        let rag = &self.deps.settings.rag;
        let max_mb = entry.general_config.max_file_size_mb;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-return-context
        Ok(UploadContext {
            targets,
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-capabilities
            rules: ClassifyRules {
                allow_csv_upload: rag.allow_csv_upload,
                disable_images: switches.disable_images,
                code_interpreter_available: entry.general_config.tool_support.code_interpreter
                    && !switches.disable_code_interpreter,
            },
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-capabilities
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-size-limits
            document_limit: size_limit_bytes(rag.uploaded_file_max_size_kb, max_mb),
            image_limit: size_limit_bytes(rag.uploaded_image_max_size_kb, max_mb),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-size-limits
        })
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-return-context
    }
}

/// Why the vector store of a chat could not be resolved.
enum StoreError {
    Mismatch,
    Failed,
}

/// What a request-side indexing step reached.
enum Indexed {
    Ready,
    /// Still `in_progress` at the deadline, in this store.
    Pending(String),
}

/// A failed step with its `error_code` and cause.
struct Failure {
    code: FailureCode,
    cause: FailCause,
}

/// Everything the background continuation owns.
struct BackgroundJob {
    repo: Arc<dyn AttachmentRepository>,
    storage: Arc<dyn StorageClient>,
    metrics: Arc<dyn AttachmentMetrics>,
    runtime: Arc<dyn BackgroundRuntime>,
    clock: Clock,
    target: StorageTarget,
    store_id: String,
    row: StoredAttachment,
}

/// How the background polling ended.
enum Reached {
    Ready,
    Failed,
    Timeout,
}

/// How one polling round of the background continuation ended.
enum Round {
    /// The round's time is used up; the next round starts with a heartbeat.
    Elapsed,
    /// The gear is stopping.
    Stopped,
    /// The indexing reached its end.
    Reached(Reached),
}

/// `cpt-cf-mini-chat-algo-attachments-background-indexing`.
async fn run_background(job: BackgroundJob) {
    let Some(ctx) = job.runtime.s2s_context() else {
        tracing::warn!(attachment_id = %job.row.id, "no S2S context for the background indexing");
        return;
    };
    let stop = job.runtime.stop_token();
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-start-window
    let window_end = Instant::now() + BACKGROUND_INDEXING_LIMIT;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-start-window
    let file_id = job.row.provider_file_id.clone().unwrap_or_default();
    let mut last_transient: Option<String> = None;
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-while-window
    let reached = loop {
        let now = Instant::now();
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-window-ended
        if stop.is_cancelled() {
            return;
        }
        if now >= window_end {
            break Reached::Timeout;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-window-ended
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-heartbeat
        if !heartbeat(&job).await {
            return;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-heartbeat
        let round_end = (now + BACKGROUND_ROUND).min(window_end);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-poll-round
        match poll_round(&job, &ctx, &file_id, round_end, &mut last_transient).await {
            Round::Elapsed => {}
            Round::Stopped => return,
            Round::Reached(reached) => break reached,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-poll-round
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-while-window
    match reached {
        Reached::Ready => background_ready(&job).await,
        Reached::Failed => background_failed(&job, &file_id, BackgroundResult::Failed).await,
        Reached::Timeout => {
            log_timeout(&job, last_transient.as_deref());
            background_failed(&job, &file_id, BackgroundResult::Timeout).await;
        }
    }
}

fn log_timeout(job: &BackgroundJob, last_transient: Option<&str>) {
    tracing::warn!(attachment_id = %job.row.id, last_transient = ?last_transient, "the background indexing ran out of time");
}

/// The heartbeat of a round; `false` when the row left the indexing states.
async fn heartbeat(job: &BackgroundJob) -> bool {
    match job
        .repo
        .heartbeat(job.row.tenant_id, job.row.id, (job.clock)())
        .await
    {
        Ok(alive) => alive,
        Err(error) => {
            tracing::warn!(attachment_id = %job.row.id, %error, "the indexing heartbeat failed");
            true
        }
    }
}

/// One round of status reads: waits double up to [`BACKGROUND_WAIT_MAX`].
async fn poll_round(
    job: &BackgroundJob,
    ctx: &SecurityContext,
    file_id: &str,
    round_end: Instant,
    last_transient: &mut Option<String>,
) -> Round {
    let stop = job.runtime.stop_token();
    let mut wait = STATUS_WAIT_INITIAL;
    loop {
        let left = round_end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Round::Elapsed;
        }
        tokio::select! {
            () = stop.cancelled() => return Round::Stopped,
            () = tokio::time::sleep(wait.min(left)) => {}
        }
        wait = (wait * 2).min(BACKGROUND_WAIT_MAX);
        if Instant::now() >= round_end {
            return Round::Elapsed;
        }
        let read = tokio::select! {
            () = stop.cancelled() => return Round::Stopped,
            read = tokio::time::timeout_at(
                round_end,
                job.storage.indexing_status(ctx, &job.target, &job.store_id, file_id),
            ) => read,
        };
        match read {
            Err(_elapsed) => return Round::Elapsed,
            Ok(Err(error)) if error.is_transient() => {
                if last_transient.is_none() {
                    tracing::warn!(attachment_id = %job.row.id, cause = %error.cause(), "a transient indexing status error");
                }
                *last_transient = Some(error.cause());
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-failed
            Ok(Err(_) | Ok(IndexStatus::Failed)) => return Round::Reached(Reached::Failed),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-completed
            Ok(Ok(IndexStatus::Completed)) => return Round::Reached(Reached::Ready),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-completed
            Ok(Ok(IndexStatus::InProgress)) => {}
        }
    }
}

/// The `ready` write of the background task: 4 attempts, 1, 2 and 4 s apart.
async fn background_ready(job: &BackgroundJob) {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-mark-ready
    for attempt in 0..=SET_READY_BACKOFF.len() {
        match job
            .repo
            .set_ready(job.row.tenant_id, job.row.id, None, (job.clock)())
            .await
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-ready
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-background-metric
            Ok(ReadyOutcome::Ready) => {
                job.metrics.background_indexing(BackgroundResult::Ready);
                return;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-background-metric
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-ready
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-taken
            Ok(ReadyOutcome::Taken { .. }) => return,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-taken
            Err(error) => {
                tracing::warn!(attachment_id = %job.row.id, attempt, %error, "the background ready write failed");
                if let Some(pause) = SET_READY_BACKOFF.get(attempt) {
                    tokio::time::sleep(*pause).await;
                }
            }
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-mark-ready
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-set-ready-failed
    job.metrics
        .background_indexing(BackgroundResult::SetReadyFailed);
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-set-ready-failed
}

/// The failure transaction of the background task.
async fn background_failed(job: &BackgroundJob, file_id: &str, result: BackgroundResult) {
    let now = (job.clock)();
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-fail-row
    let request = FailWithCleanup {
        tenant_id: job.row.tenant_id,
        chat_id: job.row.chat_id,
        attachment_id: job.row.id,
        code: FailureCode::IndexingFailed,
        provider_file_id: file_id.to_owned(),
        now,
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-enqueue
        message: CleanupMessage::for_row(
            CleanupEvent::AttachmentIndexingFailed,
            &job.row,
            Some(file_id.to_owned()),
            None,
            now,
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-enqueue
        orphan: None,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-fail-row
    match job.repo.fail_with_cleanup(&request).await {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-return-failed
        Ok(FailOutcome::Failed) => job.metrics.background_indexing(result),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-bg-return-failed
        Ok(_) => {}
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-tx-failed
        Err(error) => {
            tracing::error!(attachment_id = %job.row.id, %error, "the background indexing failure could not be recorded");
        }
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-attachments-background-indexing:p1:inst-if-bg-tx-failed
