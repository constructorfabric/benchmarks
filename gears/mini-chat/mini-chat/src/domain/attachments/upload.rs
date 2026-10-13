//! `POST /v1/chats/{id}/attachments` (DESIGN 3.3 "Upload Attachment", 3.6 "File
//! Upload").
//!
//! Order of one upload: PDP (`upload_attachment` on the chat), the owner-scoped
//! live chat (404), the chat's model in the catalog (400 `INVALID_MODEL`) - all
//! before the body is read - then the `Content-Type` boundary, the multipart
//! read, the `file` field, its content type, the MIME allowlist, the kind rules
//! (kill switches, model capabilities), the size while streaming, the
//! concurrency slot, and the insert with the per-chat limits in one
//! transaction. Only then does the provider phase start.
//!
//! The request is not detached: when the client goes away the row stays
//! `pending` / `uploaded` and the upload reaper finishes it (DESIGN 3.3).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use futures::Stream;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use toolkit_db::DbTx;
use toolkit_db::secure::TxConfig;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::mime::{self, FileKind};
use super::{AttachmentDetail, AttachmentService, NewAttachment, thumbnail};
use crate::config::MiniChatConfig;
use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::{ChatRow, chat_not_found};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::messages::AttachmentKind;
use crate::domain::ports::{
    AttachmentLifecycleRepository, ChatRepository, FileUpload, StorageTarget, VectorStoreRepository,
};

/// The request body as a byte stream.
pub type UploadStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// Longest stored file name, in characters.
const FILENAME_MAX_CHARS: usize = 255;

/// File name of a part that has none.
const DEFAULT_FILENAME: &str = "upload";

/// Bytes per MB / MiB in the size limits (`rag.max_total_upload_mb_per_chat`,
/// the model's `max_file_size_mb`).
const MIB: u64 = 1024 * 1024;

/// `Retry-After` of a provider or storage failure.
const PROVIDER_RETRY_AFTER_SECS: u64 = 10;

/// `Retry-After` of the upload concurrency limit.
const BUSY_RETRY_AFTER_SECS: u64 = 5;

/// Generic detail of every 503 of the upload.
const UNAVAILABLE_DETAIL: &str = "Service temporarily unavailable";

/// The 503 of a provider or storage failure: no provider text, no id.
pub(super) fn unavailable() -> DomainError {
    DomainError::ServiceUnavailable {
        detail: UNAVAILABLE_DETAIL.to_owned(),
        retry_after_secs: Some(PROVIDER_RETRY_AFTER_SECS),
        cause: None,
    }
}

fn busy() -> DomainError {
    DomainError::ServiceUnavailable {
        detail: UNAVAILABLE_DETAIL.to_owned(),
        retry_after_secs: Some(BUSY_RETRY_AFTER_SECS),
        cause: None,
    }
}

fn invalid(field: &'static str, reason: &'static str, detail: &str) -> DomainError {
    DomainError::InvalidInput {
        resource: ResourceKind::Attachment,
        field,
        reason,
        detail: detail.to_owned(),
    }
}

fn multipart_error(error: &multer::Error) -> DomainError {
    tracing::debug!(%error, "attachment upload: unreadable multipart body");
    invalid(
        "multipart",
        "MULTIPART_ERROR",
        "the request body is not a readable multipart/form-data document",
    )
}

fn too_large() -> DomainError {
    DomainError::OutOfRange {
        resource: ResourceKind::Attachment,
        field: "content_length",
        reason: "FILE_TOO_LARGE",
        detail: "the file exceeds the size limit".to_owned(),
    }
}

/// What the policy allows for images.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Images {
    /// The `disable_images` kill switch is on.
    Disabled,
    /// The chat's model has no `VISION_INPUT`.
    NoVision,
    Allowed,
}

/// The rules an upload is checked against: the gear configuration, the chat's
/// model and the kill switches.
struct Rules {
    allow_csv: bool,
    max_document_bytes: u64,
    max_image_bytes: u64,
    images: Images,
    code_interpreter: bool,
}

impl Rules {
    fn new(config: &MiniChatConfig, entry: &ModelCatalogEntry, kill: &KillSwitches) -> Self {
        let model_max = u64::from(entry.general_config.max_file_size_mb) * MIB;
        let rag = &config.rag;
        let vision = entry
            .multimodal_capabilities
            .iter()
            .any(|capability| capability == "VISION_INPUT");
        Self {
            allow_csv: rag.allow_csv_upload,
            max_document_bytes: (u64::from(rag.uploaded_file_max_size_kb) * 1024).min(model_max),
            max_image_bytes: (u64::from(rag.uploaded_image_max_size_kb) * 1024).min(model_max),
            images: if kill.disable_images {
                Images::Disabled
            } else if vision {
                Images::Allowed
            } else {
                Images::NoVision
            },
            code_interpreter: entry.general_config.tool_support.code_interpreter
                && !kill.disable_code_interpreter,
        }
    }

    /// Kill switches and model capabilities (DESIGN 3.3 error table).
    fn check_kind(&self, file: &FileKind) -> Result<(), DomainError> {
        if file.kind == AttachmentKind::Image {
            match self.images {
                Images::Disabled => return Err(DomainError::FeatureDisabled { feature: "images" }),
                Images::NoVision => {
                    return Err(invalid(
                        "content_type",
                        "VISION_NOT_SUPPORTED",
                        "the chat model does not accept image inputs",
                    ));
                }
                Images::Allowed => {}
            }
        }
        if file.for_code_interpreter && !self.code_interpreter {
            return Err(invalid(
                "file",
                "CODE_INTERPRETER_UNAVAILABLE",
                "the code interpreter is not available for this chat",
            ));
        }
        Ok(())
    }

    fn limit_for(&self, kind: AttachmentKind) -> u64 {
        match kind {
            AttachmentKind::Image => self.max_image_bytes,
            AttachmentKind::Document => self.max_document_bytes,
        }
    }
}

/// The accepted `file` part.
struct ParsedUpload {
    filename: String,
    kind: FileKind,
    data: Bytes,
}

/// The base name of a client supplied file name, at most 255 characters.
fn clean_filename(raw: Option<&str>) -> String {
    let base = raw
        .map(|name| name.rsplit(['/', '\\']).next().unwrap_or(name).trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_FILENAME);
    mime::truncate_filename(base, FILENAME_MAX_CHARS)
}

/// Streams the field into memory, aborting as soon as it exceeds `limit`.
async fn read_bytes(field: &mut multer::Field<'_>, limit: u64) -> Result<Bytes, DomainError> {
    let mut data = BytesMut::new();
    while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
        let size = u64::try_from(data.len() + chunk.len()).unwrap_or(u64::MAX);
        if size > limit {
            return Err(too_large());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data.freeze())
}

/// Reads and validates the multipart body (everything between the boundary and
/// the insert).
async fn read_file(
    content_type: Option<&str>,
    body: UploadStream,
    rules: &Rules,
) -> Result<ParsedUpload, DomainError> {
    let boundary = content_type
        .and_then(|value| multer::parse_boundary(value).ok())
        .ok_or_else(|| {
            invalid(
                "content_type",
                "BOUNDARY_REQUIRED",
                "Content-Type must be multipart/form-data with a boundary",
            )
        })?;
    let mut multipart = multer::Multipart::new(body, boundary);
    let mut field = loop {
        match multipart
            .next_field()
            .await
            .map_err(|e| multipart_error(&e))?
        {
            None => {
                return Err(invalid(
                    "file",
                    "MISSING_FILE",
                    "the form has no `file` field",
                ));
            }
            Some(field) if field.name() == Some("file") => break field,
            Some(_) => {}
        }
    };
    let declared = field
        .content_type()
        .map(|mime| mime.essence_str().to_owned());
    let declared = declared.ok_or_else(|| {
        invalid(
            "content_type",
            "MISSING_CONTENT_TYPE",
            "the file part has no content type",
        )
    })?;
    let filename = clean_filename(field.file_name());
    let kind = mime::classify(&declared, &filename, rules.allow_csv).ok_or_else(|| {
        invalid(
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
            "the content type is not supported",
        )
    })?;
    rules.check_kind(&kind)?;
    let data = read_bytes(&mut field, rules.limit_for(kind.kind)).await?;
    Ok(ParsedUpload {
        filename,
        kind,
        data,
    })
}

/// Everything the provider phase and the failure paths need.
pub(super) struct Job<'a> {
    pub(super) ctx: &'a SecurityContext,
    /// Tenant scope of the chat children.
    pub(super) scope: AccessScope,
    pub(super) chat: &'a ChatRow,
    pub(super) attachment_id: Uuid,
    pub(super) target: StorageTarget,
    /// Start of the request (the indexing deadline counts from here).
    pub(super) started: Instant,
}

impl AttachmentService {
    /// Uploads a file to the chat.
    ///
    /// # Errors
    /// `AccessDenied` / `ServiceUnavailable` (PDP, the concurrency limit, a
    /// provider or storage failure), `NotFound` (chat), `InvalidModel`,
    /// `InvalidInput` (multipart, content type, capability), `OutOfRange`
    /// (`FILE_TOO_LARGE`), `FeatureDisabled` (`images`), `LimitExceeded`
    /// (`document_limit`, `storage_limit`), `ProviderMismatch`, `Internal`.
    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        content_type: Option<&str>,
        body: UploadStream,
    ) -> Result<AttachmentDetail, DomainError> {
        let started = Instant::now();
        let owner = chat_scope(
            &self.enforcer,
            ctx,
            actions::UPLOAD_ATTACHMENT,
            Some(chat_id),
        )
        .await?;
        let conn = self.shared.db.conn()?;
        let chat = conn
            .find_live_chat(&owner, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        // The chat's model is resolved without the enabled filter: a disabled
        // catalog entry still serves the uploads of its chats.
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == chat.model)
            .ok_or(DomainError::InvalidModel)?;
        let rules = Rules::new(&self.config, entry, &snapshot.kill_switches);

        let file = read_file(content_type, body, &rules).await?;
        let _slot = Arc::clone(&self.uploads)
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let backend = self.shared.storage.backend_for(&entry.provider_id)?;
        let tenant = owner.tenant_only();
        let new = self
            .admit(&owner, &tenant, &chat, ctx, &backend, &file)
            .await?;

        let job = Job {
            ctx,
            scope: tenant.clone(),
            chat: &chat,
            attachment_id: new.id,
            target: StorageTarget {
                tenant_id: chat.tenant_id,
                backend,
            },
            started,
        };
        self.provider_phase(&job, &new, file).await?;
        let row = conn
            .find_attachment(&tenant, chat_id, new.id)
            .await?
            .ok_or_else(|| DomainError::internal("the uploaded attachment disappeared"))?;
        AttachmentDetail::from_row(&row)
    }

    /// The vector store of another backend is refused up front, then the row is
    /// inserted under the per-chat limits.
    async fn admit(
        &self,
        owner: &AccessScope,
        tenant: &AccessScope,
        chat: &ChatRow,
        ctx: &SecurityContext,
        backend: &str,
        file: &ParsedUpload,
    ) -> Result<NewAttachment, DomainError> {
        if file.kind.for_file_search {
            let existing = self
                .shared
                .db
                .conn()?
                .find_vector_store(tenant, chat.id)
                .await?;
            if existing.is_some_and(|store| store.provider != backend) {
                return Err(DomainError::ProviderMismatch);
            }
        }
        let new = NewAttachment {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            user_id: ctx.subject_id(),
            filename: file.filename.clone(),
            content_type: file.kind.content_type.clone(),
            size_bytes: i64::try_from(file.data.len()).unwrap_or(i64::MAX),
            storage_backend: backend.to_owned(),
            kind: file.kind.kind,
            for_file_search: file.kind.for_file_search,
            for_code_interpreter: file.kind.for_code_interpreter,
            now: now_utc(),
        };
        let limits = Limits::of(&self.config);
        // The first statement of the transaction is a write (the chat lock): a
        // SQLite transaction that starts with a read and then upgrades fails
        // with SQLITE_BUSY_SNAPSHOT when another writer committed in between.
        self.shared
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (owner, tenant, new) = (owner.clone(), tenant.clone(), new.clone());
                Box::pin(
                    async move { insert_within_limits(tx, &owner, &tenant, new, limits).await },
                )
            })
            .await
    }

    /// The provider side of the upload: Files API, the document indexing or the
    /// image thumbnail, and the final status.
    async fn provider_phase(
        &self,
        job: &Job<'_>,
        new: &NewAttachment,
        file: ParsedUpload,
    ) -> Result<(), DomainError> {
        let provider_name = format!("{}_{}.{}", new.chat_id, new.id, file.kind.extension);
        let upload = FileUpload {
            filename: provider_name,
            content_type: new.content_type.clone(),
            data: file.data.clone(),
        };
        let provider_file = match self
            .shared
            .storage
            .upload_file(job.ctx, &job.target, upload)
            .await
        {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%error, attachment_id = %new.id, "provider file upload failed");
                return Err(self
                    .fail_with(job, super::error_code::UPLOAD_FAILED, None, unavailable())
                    .await);
            }
        };
        let conn = self.shared.db.conn()?;
        if !conn
            .mark_attachment_uploaded(&job.scope, new.id, &provider_file, now_utc())
            .await?
        {
            // Deleted while the upload ran: the cleanup message found no file.
            self.spawn_file_delete(job, &provider_file);
            return Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
                id: new.id.to_string(),
            });
        }
        if new.for_file_search {
            return self.index_document(job, &provider_file).await;
        }
        let thumbnail = if new.kind == AttachmentKind::Image {
            self.thumbnail_of(file.data).await
        } else {
            None
        };
        self.make_ready(job, thumbnail.as_ref()).await
    }

    /// The thumbnail of an image, off the async threads; `None` when it cannot
    /// be made.
    async fn thumbnail_of(&self, data: Bytes) -> Option<super::NewThumbnail> {
        let config = self.config.thumbnail.clone();
        tokio::task::spawn_blocking(move || thumbnail::generate(&data, &config))
            .await
            .ok()
            .flatten()
    }

    /// `uploaded` -> `ready`.
    pub(super) async fn make_ready(
        &self,
        job: &Job<'_>,
        thumbnail: Option<&super::NewThumbnail>,
    ) -> Result<(), DomainError> {
        let ready = self
            .shared
            .db
            .conn()?
            .mark_attachment_ready(&job.scope, job.attachment_id, thumbnail, now_utc())
            .await?;
        if ready {
            Ok(())
        } else {
            Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
                id: job.attachment_id.to_string(),
            })
        }
    }

    /// Marks the row `failed` with `code`, deletes the provider file in the
    /// background (best effort, once) and hands `error` back to the client.
    pub(super) async fn fail_with(
        &self,
        job: &Job<'_>,
        code: &'static str,
        provider_file: Option<&str>,
        error: DomainError,
    ) -> DomainError {
        let marked = match self.shared.db.conn() {
            Ok(conn) => {
                conn.mark_attachment_failed(&job.scope, job.attachment_id, code, now_utc())
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(cause) = marked {
            tracing::warn!(attachment_id = %job.attachment_id, %cause,
                "the failed upload could not be recorded; the upload reaper will take the row");
        }
        if let Some(file) = provider_file {
            self.spawn_file_delete(job, file);
        }
        error
    }

    /// The inline cleanup of a failed document: one best-effort `DELETE`, not
    /// retried, no outbox message.
    fn spawn_file_delete(&self, job: &Job<'_>, provider_file: &str) {
        let storage = Arc::clone(&self.shared.storage);
        let (ctx, target, file) = (
            job.ctx.clone(),
            job.target.clone(),
            provider_file.to_owned(),
        );
        self.tasks.spawn(async move {
            if let Err(error) = storage.delete_file(&ctx, &target, &file).await {
                tracing::warn!(%error, "best-effort delete of the provider file failed");
            }
        });
    }
}

/// The per-chat limits of DESIGN "RAG Quality & Scale Controls".
#[derive(Clone, Copy)]
struct Limits {
    max_documents: u64,
    max_total_bytes: u64,
}

impl Limits {
    fn of(config: &MiniChatConfig) -> Self {
        Self {
            max_documents: u64::from(config.rag.max_documents_per_chat),
            max_total_bytes: u64::from(config.rag.max_total_upload_mb_per_chat) * MIB,
        }
    }
}

/// The body of the insert transaction: lock the chat (write first), read what
/// the chat already holds, check the limits, insert.
async fn insert_within_limits(
    tx: &DbTx<'_>,
    owner: &AccessScope,
    tenant: &AccessScope,
    new: NewAttachment,
    limits: Limits,
) -> Result<NewAttachment, DomainError> {
    if !tx.lock_live_chat(owner, new.chat_id).await? {
        return Err(chat_not_found(new.chat_id));
    }
    let usage = tx.attachment_usage(tenant, new.chat_id).await?;
    if new.kind == AttachmentKind::Document && usage.documents >= limits.max_documents {
        return Err(DomainError::LimitExceeded {
            limit: "document_limit",
        });
    }
    let size = u64::try_from(new.size_bytes).unwrap_or(u64::MAX);
    if usage.total_bytes.saturating_add(size) > limits.max_total_bytes {
        return Err(DomainError::LimitExceeded {
            limit: "storage_limit",
        });
    }
    tx.insert_attachment(tenant, new.clone()).await?;
    Ok(new)
}
