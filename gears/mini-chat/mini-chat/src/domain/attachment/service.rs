//! The attachment service: upload, get and delete (DESIGN "Upload Attachment", "Get Attachment",
//! "Attachment Deletion").
//!
//! An upload is two steps so that the HTTP layer can read the body in between: [`begin_upload`]
//! runs the checks that precede the body (authorization, chat, model, concurrency slot) and
//! [`admit`] validates the `file` part headers; [`upload`] then stores the buffered file.
//!
//! [`begin_upload`]: AttachmentService::begin_upload
//! [`admit`]: AttachmentService::admit
//! [`upload`]: AttachmentService::upload

use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use tracing::warn;
use uuid::Uuid;

use super::mime::{normalize_filename, resolve_content_type};
use super::model::{
    AttachmentDetail, AttachmentKind, IncomingFile, IndexingTimings, PROVIDER_KIND_ANTHROPIC,
    Purposes,
};
use super::ports::{AttachmentRepository, ChatStorage, DeleteOutcome, StorageGateway};
use crate::config::{RagConfig, ThumbnailConfig};
use crate::domain::authz::{Pep, actions};
use crate::domain::chat::ChatRepository;
use crate::domain::cleanup::{AttachmentCleanupEvent, AttachmentCleanupPayload, SecondaryRef};
use crate::domain::clock;
use crate::domain::error::{DisabledFeature, DomainError, MultipartIssue};
use crate::domain::metrics::Metrics;
use crate::domain::ports::ModelPolicyGateway;

const BYTES_PER_KIB: u64 = 1024;
const BYTES_PER_MIB: u64 = 1024 * 1024;

/// What the chat's model and the kill switches allow an upload to do.
#[domain_model]
#[derive(Debug, Clone, Copy)]
struct UploadPolicy {
    /// `general_config.max_file_size_mb` in bytes.
    model_max_bytes: u64,
    /// The code interpreter is available (no kill switch, model supports it).
    code_interpreter: bool,
    /// `disable_images` is on.
    images_disabled: bool,
}

/// The state of an upload between the pre-body checks and the stored file: the caller, the chat,
/// the model's limits, the storage routes and the concurrency slot (released on drop).
#[domain_model]
pub struct UploadSession {
    pub(super) ctx: SecurityContext,
    pub(super) scope: AccessScope,
    pub(super) chat_id: Uuid,
    policy: UploadPolicy,
    pub(super) storage: ChatStorage,
    pub(super) started: Instant,
    _permit: OwnedSemaphorePermit,
}

/// The validated headers of the `file` part.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The normalized filename.
    pub filename: String,
    /// The resolved MIME type.
    pub content_type: String,
    /// Document or image.
    pub kind: AttachmentKind,
    /// The tool paths after the kill switches and model capabilities apply.
    pub purposes: Purposes,
    /// The canonical file extension of the type.
    pub extension: &'static str,
    /// The size limit of the file in bytes.
    pub max_bytes: u64,
}

/// Collaborators and settings of the attachment service.
pub struct AttachmentServiceParts {
    /// The PEP.
    pub pep: Pep,
    /// Chat reads.
    pub chats: Arc<dyn ChatRepository>,
    /// Attachment persistence.
    pub repo: Arc<dyn AttachmentRepository>,
    /// The model policy (catalog and kill switches).
    pub policy: Arc<dyn ModelPolicyGateway>,
    /// The provider storage gateway.
    pub storage: Arc<dyn StorageGateway>,
    /// The metrics sink.
    pub metrics: Arc<dyn Metrics>,
    /// `rag`.
    pub rag: RagConfig,
    /// `thumbnail`.
    pub thumbnail: ThumbnailConfig,
    /// The indexing wait timings.
    pub timings: IndexingTimings,
}

/// Upload, get and delete of attachments.
#[domain_model]
pub struct AttachmentService {
    pub(super) pep: Pep,
    pub(super) chats: Arc<dyn ChatRepository>,
    pub(super) repo: Arc<dyn AttachmentRepository>,
    pub(super) policy: Arc<dyn ModelPolicyGateway>,
    pub(super) storage: Arc<dyn StorageGateway>,
    pub(super) metrics: Arc<dyn Metrics>,
    pub(super) rag: RagConfig,
    pub(super) thumbnail: ThumbnailConfig,
    pub(super) timings: IndexingTimings,
    uploads: Arc<Semaphore>,
    /// Background indexing tasks and best-effort provider deletes, so gear stop can wait for them.
    pub(super) tasks: TaskTracker,
    /// Cancelled at gear stop; the background tasks stop without changes.
    pub(super) shutdown: OnceLock<CancellationToken>,
}

impl AttachmentService {
    /// Creates the service.
    #[must_use]
    pub fn new(parts: AttachmentServiceParts) -> Self {
        let slots = usize::from(parts.rag.max_concurrent_uploads);
        Self {
            pep: parts.pep,
            chats: parts.chats,
            repo: parts.repo,
            policy: parts.policy,
            storage: parts.storage,
            metrics: parts.metrics,
            rag: parts.rag,
            thumbnail: parts.thumbnail,
            timings: parts.timings,
            uploads: Arc::new(Semaphore::new(slots)),
            tasks: TaskTracker::new(),
            shutdown: OnceLock::new(),
        }
    }

    /// Binds the token that stops the background tasks (gear stop). Only the first binding takes
    /// effect.
    pub fn bind_shutdown(&self, token: CancellationToken) {
        if self.shutdown.set(token).is_err() {
            warn!("the attachment shutdown token is already bound; keeping the first one");
        }
    }

    /// Cancels the background tasks and waits until they finished; the caller bounds the wait.
    pub async fn shutdown(&self) {
        if let Some(token) = self.shutdown.get() {
            token.cancel();
        }
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub(super) fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.get().cloned().unwrap_or_default()
    }

    /// The checks that precede the body: authorization, the chat (404), the chat's model without
    /// the enabled filter (400 `INVALID_MODEL`) and a concurrency slot (503).
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `InvalidModel`, `PolicyUnavailable`,
    /// `UploadConcurrencyLimit` or an internal error.
    pub async fn begin_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<UploadSession, DomainError> {
        let started = Instant::now();
        let scope = self
            .pep
            .chat_scope(ctx, actions::UPLOAD_ATTACHMENT, Some(chat_id))
            .await?;
        let chat = self
            .chats
            .find(&scope, chat_id)
            .await?
            .ok_or(DomainError::ChatNotFound { chat_id })?
            .chat;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == chat.model)
            .ok_or_else(|| DomainError::InvalidModel {
                detail: format!("the chat's model `{}` is not in the catalog", chat.model),
            })?;
        let storage = self
            .storage
            .chat_storage(ctx.subject_tenant_id(), &entry.provider_id)?;
        let policy = UploadPolicy {
            model_max_bytes: u64::from(entry.general_config.max_file_size_mb) * BYTES_PER_MIB,
            code_interpreter: entry.general_config.tool_support.code_interpreter
                && !snapshot.kill_switches.disable_code_interpreter,
            images_disabled: snapshot.kill_switches.disable_images,
        };
        let permit = Arc::clone(&self.uploads)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;
        Ok(UploadSession {
            ctx: ctx.clone(),
            scope,
            chat_id,
            policy,
            storage,
            started,
            _permit: permit,
        })
    }

    /// Validates the headers of the `file` part before its body is read: the content type, the
    /// kill switches, the model capabilities and the size limit.
    ///
    /// # Errors
    /// `InvalidMultipart` (missing content type), `UnsupportedContentType`, `FeatureDisabled` for
    /// images or `CodeInterpreterUnavailable`.
    pub fn admit(
        &self,
        session: &UploadSession,
        filename: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<Admission, DomainError> {
        let declared = content_type.ok_or(DomainError::InvalidMultipart {
            issue: MultipartIssue::MissingContentType,
        })?;
        let filename = normalize_filename(filename);
        let resolved = resolve_content_type(declared, &filename, self.rag.allow_csv_upload)
            .ok_or(DomainError::UnsupportedContentType)?;
        if resolved.kind == AttachmentKind::Image && session.policy.images_disabled {
            return Err(DomainError::FeatureDisabled {
                feature: DisabledFeature::Images,
            });
        }
        let mut purposes = resolved.purposes;
        if purposes.code_interpreter && !session.policy.code_interpreter {
            purposes.code_interpreter = false;
            if !purposes.file_search {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
        }
        let configured_kb = match resolved.kind {
            AttachmentKind::Document => self.rag.uploaded_file_max_size_kb,
            AttachmentKind::Image => self.rag.uploaded_image_max_size_kb,
        };
        let max_bytes =
            (u64::from(configured_kb) * BYTES_PER_KIB).min(session.policy.model_max_bytes);
        Ok(Admission {
            filename,
            content_type: resolved.content_type,
            kind: resolved.kind,
            purposes,
            extension: resolved.extension,
            max_bytes,
        })
    }

    /// Stores the buffered file: the row, the provider upload and the purpose-specific steps.
    ///
    /// # Errors
    /// `DocumentLimit`, `StorageLimit`, `StorageUnavailable`, `ProviderMismatch` or an internal
    /// error; a failure after the row insert leaves the row `failed` with its `error_code`.
    pub async fn upload(
        &self,
        session: UploadSession,
        admission: Admission,
        file: IncomingFile,
    ) -> Result<AttachmentDetail, DomainError> {
        let result = self.run_upload(&session, &admission, &file).await;
        let outcome = match &result {
            Ok(_) => "ok",
            Err(DomainError::DocumentLimit | DomainError::StorageLimit) => "limit_exceeded",
            Err(DomainError::StorageUnavailable) => "storage_failed",
            Err(DomainError::ProviderMismatch) => "provider_mismatch",
            Err(_) => "error",
        };
        self.metrics
            .attachment_upload(admission.kind.as_str(), outcome);
        result
    }

    /// Reads an attachment of one of the caller's chats. Another user's attachment, another
    /// chat's and a soft-deleted one are `AttachmentNotFound`.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `AttachmentNotFound` or an internal
    /// error.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetail, DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::READ_ATTACHMENT, Some(chat_id))
            .await?;
        self.require_chat(&scope, chat_id).await?;
        self.repo
            .find(&scope, chat_id, attachment_id)
            .await?
            .filter(|row| row.deleted_at.is_none() && row.uploaded_by == ctx.subject_id())
            .map(AttachmentDetail::from)
            .ok_or(DomainError::AttachmentNotFound { attachment_id })
    }

    /// Deletes an attachment that no submitted message references: soft delete plus the cleanup
    /// event in one transaction. The uploader check runs before the idempotency check, so a
    /// repeated delete by the uploader is a no-op and by anyone else a 404.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `AttachmentNotFound`, `AttachmentLocked`,
    /// `OutboxPayloadTooLarge` or an internal error.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::DELETE_ATTACHMENT, Some(chat_id))
            .await?;
        let chat = self.require_chat(&scope, chat_id).await?;
        let row = self
            .repo
            .find(&scope, chat_id, attachment_id)
            .await?
            .filter(|row| row.uploaded_by == ctx.subject_id())
            .ok_or(DomainError::AttachmentNotFound { attachment_id })?;
        if row.deleted_at.is_some() {
            return Ok(());
        }
        let secondary_ref = match row.secondary_file_id.as_deref() {
            Some(file_id) => self.secondary_ref(ctx, &chat.model, file_id).await,
            None => None,
        };
        let payload = AttachmentCleanupPayload {
            event_type: AttachmentCleanupEvent::Deleted,
            tenant_id: row.tenant_id,
            chat_id,
            attachment_id,
            provider_file_id: row.provider_file_id,
            vector_store_id: None,
            storage_backend: row.storage_backend,
            attachment_kind: row.kind.as_str().to_owned(),
            deleted_at: clock::now(),
            secondary_ref,
        };
        match self
            .repo
            .delete(&scope, chat_id, &payload, payload.deleted_at)
            .await?
        {
            DeleteOutcome::Deleted | DeleteOutcome::AlreadyDeleted => Ok(()),
            DeleteOutcome::Locked => Err(DomainError::AttachmentLocked),
        }
    }

    async fn require_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<crate::domain::chat::Chat, DomainError> {
        self.chats
            .find(scope, chat_id)
            .await?
            .map(|detail| detail.chat)
            .ok_or(DomainError::ChatNotFound { chat_id })
    }

    /// The reference to the secondary copy of an image, resolved through the chat's model
    /// provider; `None` (skipped, counted) when the provider cannot be resolved any more.
    async fn secondary_ref(
        &self,
        ctx: &SecurityContext,
        model: &str,
        file_id: &str,
    ) -> Option<SecondaryRef> {
        let resolved = match self.policy.current_snapshot(ctx.subject_id()).await {
            Ok(snapshot) => snapshot
                .model_catalog
                .into_iter()
                .find(|entry| entry.id == model)
                .and_then(|entry| {
                    self.storage
                        .chat_storage(ctx.subject_tenant_id(), &entry.provider_id)
                        .ok()
                })
                .and_then(|storage| storage.secondary),
            Err(err) => {
                warn!(error = %err, "the policy could not be read to locate the secondary copy");
                None
            }
        };
        let Some(route) = resolved else {
            warn!("the secondary file of a deleted attachment cannot be located; skipping it");
            self.metrics
                .secondary_cleanup_skipped(PROVIDER_KIND_ANTHROPIC);
            return None;
        };
        Some(SecondaryRef {
            file_id: file_id.to_owned(),
            provider_kind: route.provider_kind.to_owned(),
            upstream_alias: route.alias,
        })
    }
}
