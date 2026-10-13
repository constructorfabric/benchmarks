//! Attachment service: upload, read and deletion of chat attachments (DESIGN
//! section 3.3 "Get Attachment", "Upload Attachment"; section 3.6 "File
//! Upload"; section 3.7 `attachments`, `chat_vector_stores` creation
//! protocol; "Code Interpreter Tool Availability"; "Attachment Mutability and
//! Deletion", "Attachment Deletion"; "RAG Quality & Scale Controls"; B.7,
//! B.8; ADR-0007).
//!
//! **Upload** is split around the multipart body so that every check that
//! does not need the file runs before the body is read:
//!
//! 1. [`AttachmentService::begin_upload`] (before the body): PEP
//!    `upload_attachment`, the scoped chat load (`404` chat), the chat's
//!    model looked up without the `enabled` filter (`INVALID_MODEL`; plugin
//!    failures are 500), the storage backend of the model's provider
//!    (`rag_provider` or the provider itself), then a slot of the
//!    process-wide upload limit `rag.max_concurrent_uploads` (`503`,
//!    `Retry-After: 5`). The returned [`UploadSession`] holds the slot until
//!    it is dropped, and its start time is the start of the 25 s indexing
//!    deadline.
//! 2. [`AttachmentService::classify_file`] (from the `file` part headers):
//!    the filename (default `upload`, at most 255 characters keeping the
//!    extension), the MIME type ([`resolve_content_type`]: parameters
//!    dropped, `application/octet-stream` inferred from the extension,
//!    `text/csv` remapped to `text/plain` when `rag.allow_csv_upload`,
//!    anything else outside the supported set is `UNSUPPORTED_CONTENT_TYPE`),
//!    the kind and purposes ([`purposes`]: images are rejected with
//!    `FEATURE_DISABLED` under `disable_images`; XLSX is code interpreter
//!    only and `CODE_INTERPRETER_UNAVAILABLE` under `disable_code_interpreter`
//!    or without the model's `tool_support.code_interpreter`; every other
//!    document is `file_search`) and the effective size limit
//!    `min(rag.uploaded_{file|image}_max_size_kb, model max_file_size_mb)`.
//!    The caller counts the body bytes against [`FileSpec::check_size`]
//!    (`FILE_TOO_LARGE`) while it reads them.
//! 3. [`AttachmentService::upload`] (with the bytes): a `file_search`
//!    document into a chat whose vector store belongs to another backend is
//!    `409 provider_mismatch` before anything is written; the `pending` row
//!    is inserted with the per-chat document and size limits (`429`); the
//!    provider Files API upload (`{chat_id}_{attachment_id}.{ext}`,
//!    `purpose=assistants`) sets `uploaded` while the row is still unclaimed
//!    (a row deleted or claimed by a chat cleanup during the upload keeps no
//!    provider file id: the new file is deleted best effort and the request
//!    fails before any vector store work); then, by purpose:
//!    - document with `file_search`: the chat vector store (creation
//!      protocol below), the file added with `{attachment_id}`, indexing
//!      polled 250 ms doubling to 2 s until 25 s after the upload started
//!      (a missing status is `in_progress`, transient read errors keep
//!      polling, each read is bounded by the deadline). `completed` is
//!      `ready`; `failed`, `cancelled`, another status or a non-transient
//!      read error is `failed` / `indexing_failed`, a best-effort provider
//!      file delete and `503`; still `in_progress` at the deadline returns
//!      `uploaded` and hands the file to the background indexing task;
//!    - code interpreter only: `ready`;
//!    - image: the thumbnail ([`ThumbnailRenderer`], skipped on any failure)
//!      and `ready`.
//!
//!    Any provider or storage failure after the insert leaves the row
//!    `failed` (`upload_failed`, `vector_store_failed` or `indexing_failed`)
//!    and is `503` (`Retry-After: 10`); a `409 provider_mismatch` found by
//!    the creation protocol fails the row with `vector_store_failed`. Provider
//!    writes are not retried. A dropped request records nothing: the upload
//!    reaper owns such rows.
//!
//! **Vector store creation protocol** (chat vector store of a document):
//! reuse the chat's row when it has a store id (another backend is `409`);
//! otherwise insert the placeholder (`vector_store_id = NULL`, committed on
//! its own) while the chat is not deleted (`404` chat otherwise). The winner
//! creates the provider store, deletes the placeholder on failure (`503`),
//! re-checks that the chat is not deleted (a deleted chat's cleanup may have
//! run before the placeholder existed: the placeholder is removed, the new
//! store deleted best effort, `404` chat) and sets the id with a
//! compare-and-set; a lost
//! compare-and-set (the placeholder was reclaimed) deletes the new store best
//! effort and waits like a loser. A loser polls the row 250 ms doubling, 5
//! times, then `503`; it never creates a store. A placeholder still NULL 120 s
//! after `created_at` is stale: it is deleted and the protocol restarts once.
//!
//! **Background indexing** ([`BackgroundIndexing`]): rounds of 20 s for up to
//! 10 minutes; each round first refreshes `updated_at` while the row is still
//! `uploaded`, not deleted and has no `cleanup_status` (otherwise the task
//! stops without changes), then reads the status 250 ms doubling to 5 s.
//! `completed` sets `ready` (4 attempts, 1 / 2 / 4 s apart); `failed`,
//! `cancelled`, another status, a non-transient read error or the 10-minute
//! limit fail the row (`indexing_failed`, `cleanup_status = pending`) together
//! with an `attachment_indexing_failed` cleanup message in one transaction.
//! The task stops on the gear's [`CancellationToken`].
//!
//! **Read** ([`AttachmentService::get`]) and **delete**
//! ([`AttachmentService::delete`]) authorize through the parent chat (`404`
//! chat); an attachment that is unknown, in another chat or uploaded by
//! another user is `404` (attachment), checked before anything else, so a
//! repeated delete by another user is `404` too. `get` also hides deleted
//! rows; `delete` of an already deleted row is a no-op (`204`, no second
//! cleanup message), a referenced one is `409 attachment_locked`, otherwise
//! the soft delete and the `attachment_deleted` cleanup message are one
//! transaction.
//!
//! **Secondary copies** (Anthropic Files API, ADR-0005), when a
//! [`SecondaryFileUploader`] is supplied ([`AttachmentService::with_secondary`]):
//! after the provider upload of an image into a chat whose model's provider
//! needs a copy ([`SecondaryFileUploader::secondary_target`]), the image is
//! uploaded again with only its `file` part. The outcome is stored on the
//! unclaimed row (`secondary_status = uploaded` with the file id, or
//! `failed`; `secondary_provider_kind = anthropic`); a failed copy does not
//! fail the upload (the Anthropic adapter then drops the image block), and a
//! copy whose row was deleted or claimed by a cleanup meanwhile is deleted
//! best effort. Documents, and images larger than `thumbnail.max_decode_bytes`,
//! get no copy (`not_attempted`). The row also stores the provider entry the
//! copy was uploaded with (`secondary_provider_id`). The `attachment_deleted`
//! cleanup message carries `secondary_ref = {file_id, provider_kind,
//! upstream_alias}` for a row with a copy; the alias is resolved inside the
//! deletion transaction from the soft-deleted row's stored entry, so the
//! delete reaches the account the copy was uploaded to.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use time::OffsetDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use toolkit_security::{AccessScope, SecurityContext};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::{Action, AuthzService};
use crate::domain::chat_service::chat_not_found;
use crate::domain::error::{
    DomainError, InvalidArgumentReason, OutOfRangeReason, PreconditionSubject, ResourceKind,
};
use crate::domain::message_service::{ImgThumbnail, THUMBNAIL_CONTENT_TYPE};
use crate::domain::models_service::ModelsService;
use crate::domain::ports::{
    AttachmentKind, AttachmentRecord, AttachmentStatus, AttachmentStore, ChatRecord,
    ChatRepository, ChatUploadLimits, FileStorage, FileUpload, NewAttachment,
    NewVectorStorePlaceholder, SecondaryAliasResolver, SecondaryCopy, SecondaryFileUploader,
    SecondaryTarget, StorageTarget, ThumbnailRenderer, VectorStoreFileStatus,
    VectorStoreRepository, VectorStores, attachment_error, storage_now,
};

/// Filename of a `file` part without one.
pub const DEFAULT_FILENAME: &str = "upload";
/// Maximum stored filename length in characters.
pub const MAX_FILENAME_CHARS: usize = 255;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// Lowest `upload_reaper.stale_after_secs` the configuration accepts.
pub const MIN_STALE_AFTER_SECS: u64 = 60;

/// Interval of the background indexing heartbeat (one round).
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);

// The heartbeat must stay well below the reaper's staleness threshold.
const _: () = assert!(BACKGROUND_ROUND.as_secs() * 2 <= MIN_STALE_AFTER_SECS);

// ── Settings ─────────────────────────────────────────────────────────────

/// Waits and deadlines of an upload (DESIGN section 3.6, section 3.7
/// creation protocol, B.9.5). [`Default`] is the fixed design values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadTiming {
    /// Indexing wait inside the request, from the upload start (25 s).
    pub indexing_deadline: Duration,
    /// First wait between status reads (250 ms), doubled after each read.
    pub poll_first_wait: Duration,
    /// Upper bound of the wait inside the request (2 s).
    pub poll_max_wait: Duration,
    /// Heartbeat round of the background task (20 s).
    pub background_round: Duration,
    /// Upper bound of the wait of the background task (5 s).
    pub background_max_wait: Duration,
    /// Total background indexing time (10 min).
    pub background_limit: Duration,
    /// Waits between the attempts to set `ready` in the background (1, 2,
    /// 4 s: 4 attempts).
    pub set_ready_backoff: [Duration; 3],
    /// First wait of a loser polling the vector store row (250 ms, doubled).
    pub store_poll_first_wait: Duration,
    /// Polls of a loser before `503` (5).
    pub store_polls: u32,
    /// Age of a NULL placeholder that counts as stale (120 s).
    pub stale_placeholder_after: Duration,
}

impl Default for UploadTiming {
    fn default() -> Self {
        Self {
            indexing_deadline: Duration::from_secs(25),
            poll_first_wait: Duration::from_millis(250),
            poll_max_wait: Duration::from_secs(2),
            background_round: BACKGROUND_ROUND,
            background_max_wait: Duration::from_secs(5),
            background_limit: Duration::from_secs(600),
            set_ready_backoff: [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            store_poll_first_wait: Duration::from_millis(250),
            store_polls: 5,
            stale_placeholder_after: Duration::from_secs(120),
        }
    }
}

/// The gear configuration the attachment service reads (B.7, B.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSettings {
    /// `rag.max_documents_per_chat`.
    pub max_documents_per_chat: u32,
    /// `rag.max_total_upload_mb_per_chat`, in bytes.
    pub max_total_upload_bytes_per_chat: u64,
    /// `rag.allow_csv_upload`.
    pub allow_csv_upload: bool,
    /// `rag.max_concurrent_uploads` (per process).
    pub max_concurrent_uploads: u16,
    /// `rag.uploaded_file_max_size_kb`, in bytes.
    pub document_max_bytes: u64,
    /// `rag.uploaded_image_max_size_kb`, in bytes.
    pub image_max_bytes: u64,
    pub timing: UploadTiming,
}

impl AttachmentSettings {
    /// Settings of `config` with the fixed design timing.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        let rag = &config.rag;
        Self {
            max_documents_per_chat: rag.max_documents_per_chat,
            max_total_upload_bytes_per_chat: u64::from(rag.max_total_upload_mb_per_chat) * MIB,
            allow_csv_upload: rag.allow_csv_upload,
            max_concurrent_uploads: rag.max_concurrent_uploads,
            document_max_bytes: u64::from(rag.uploaded_file_max_size_kb) * KIB,
            image_max_bytes: u64::from(rag.uploaded_image_max_size_kb) * KIB,
            timing: UploadTiming::default(),
        }
    }
}

// ── MIME types, purposes, filenames ──────────────────────────────────────

/// A supported upload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadType {
    mime: &'static str,
    /// Extension of the provider-side filename.
    extension: &'static str,
    kind: AttachmentKind,
    /// Code interpreter is the only purpose (XLSX).
    code_interpreter_only: bool,
}

const fn document(mime: &'static str, extension: &'static str) -> UploadType {
    UploadType {
        mime,
        extension,
        kind: AttachmentKind::Document,
        code_interpreter_only: false,
    }
}

const fn image(mime: &'static str, extension: &'static str) -> UploadType {
    UploadType {
        mime,
        extension,
        kind: AttachmentKind::Image,
        code_interpreter_only: false,
    }
}

/// XLSX MIME type (code interpreter only).
pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// MIME type that is inferred from the filename extension.
pub const OCTET_STREAM: &str = "application/octet-stream";
/// CSV, accepted as `text/plain` when `rag.allow_csv_upload`.
pub const CSV_MIME: &str = "text/csv";
/// Plain text.
pub const TEXT_PLAIN: &str = "text/plain";

/// Supported types: the `file_search` document types of the provider, XLSX
/// for code interpreter, and the four image types.
const UPLOAD_TYPES: &[UploadType] = &[
    document("application/pdf", "pdf"),
    document(TEXT_PLAIN, "txt"),
    document("text/markdown", "md"),
    document("text/html", "html"),
    document("application/json", "json"),
    document("application/msword", "doc"),
    document(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    document(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    document("text/x-c", "c"),
    document("text/x-c++", "cpp"),
    document("text/x-csharp", "cs"),
    document("text/css", "css"),
    document("text/x-golang", "go"),
    document("text/x-java", "java"),
    document("text/javascript", "js"),
    document("text/x-php", "php"),
    document("text/x-python", "py"),
    document("text/x-ruby", "rb"),
    document("text/x-sh", "sh"),
    document("text/x-tex", "tex"),
    document("application/typescript", "ts"),
    UploadType {
        mime: XLSX_MIME,
        extension: "xlsx",
        kind: AttachmentKind::Document,
        code_interpreter_only: true,
    },
    image("image/png", "png"),
    image("image/jpeg", "jpg"),
    image("image/webp", "webp"),
    image("image/gif", "gif"),
];

/// Common alternative spellings of supported types.
const MIME_ALIASES: &[(&str, &str)] = &[
    ("image/jpg", "image/jpeg"),
    ("image/pjpeg", "image/jpeg"),
    ("text/x-markdown", "text/markdown"),
    ("application/javascript", "text/javascript"),
    ("application/x-javascript", "text/javascript"),
    ("application/x-sh", "text/x-sh"),
    ("application/x-tex", "text/x-tex"),
    ("text/x-typescript", "application/typescript"),
    ("text/x-script.python", "text/x-python"),
    ("application/csv", CSV_MIME),
];

/// MIME type of a filename extension (lowercase, without the dot), used for
/// `application/octet-stream` parts.
const EXTENSION_TYPES: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("txt", TEXT_PLAIN),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("json", "application/json"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("xlsx", XLSX_MIME),
    ("csv", CSV_MIME),
    ("c", "text/x-c"),
    ("cpp", "text/x-c++"),
    ("cs", "text/x-csharp"),
    ("css", "text/css"),
    ("go", "text/x-golang"),
    ("java", "text/x-java"),
    ("js", "text/javascript"),
    ("php", "text/x-php"),
    ("py", "text/x-python"),
    ("rb", "text/x-ruby"),
    ("sh", "text/x-sh"),
    ("tex", "text/x-tex"),
    ("ts", "application/typescript"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// A validated upload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedType {
    /// The MIME type stored and sent to the provider.
    pub content_type: &'static str,
    /// Extension of the provider-side filename.
    pub extension: &'static str,
    pub kind: AttachmentKind,
    /// XLSX: code interpreter is the only purpose.
    pub code_interpreter_only: bool,
}

/// Lowercase extension of `filename` (after the last dot), if any.
fn extension_of(filename: &str) -> Option<String> {
    let (stem, extension) = filename.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty()).then(|| extension.to_ascii_lowercase())
}

/// Resolves the declared content type of a `file` part.
///
/// Parameters are dropped and the type is compared case-insensitively;
/// `application/octet-stream` is replaced by the type of the filename
/// extension (an unknown extension keeps it, which is unsupported);
/// `text/csv` becomes `text/plain` when `allow_csv`.
///
/// # Errors
///
/// `400 UNSUPPORTED_CONTENT_TYPE` for a type outside the supported set.
pub fn resolve_content_type(
    declared: &str,
    filename: &str,
    allow_csv: bool,
) -> Result<ResolvedType, DomainError> {
    let unsupported =
        || DomainError::invalid_argument(InvalidArgumentReason::UnsupportedContentType);
    let mut essence = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if essence == OCTET_STREAM
        && let Some(inferred) = extension_of(filename).and_then(|extension| {
            EXTENSION_TYPES
                .iter()
                .find(|(known, _)| *known == extension)
                .map(|(_, mime)| *mime)
        })
    {
        inferred.clone_into(&mut essence);
    }
    if let Some((_, canonical)) = MIME_ALIASES.iter().find(|(alias, _)| *alias == essence) {
        (*canonical).clone_into(&mut essence);
    }
    if essence == CSV_MIME {
        if !allow_csv {
            return Err(unsupported());
        }
        TEXT_PLAIN.clone_into(&mut essence);
    }
    UPLOAD_TYPES
        .iter()
        .find(|known| known.mime == essence)
        .map(|known| ResolvedType {
            content_type: known.mime,
            extension: known.extension,
            kind: known.kind,
            code_interpreter_only: known.code_interpreter_only,
        })
        .ok_or_else(unsupported)
}

/// The `(for_file_search, for_code_interpreter)` purposes of `resolved`
/// after the kill switches and the model's tool support.
///
/// # Errors
///
/// `400 failed_precondition` (`images` / `FEATURE_DISABLED`) for an image
/// under `disable_images`; `400 CODE_INTERPRETER_UNAVAILABLE` for a
/// code-interpreter-only file when code interpreter is disabled or the
/// model lacks `tool_support.code_interpreter`.
pub fn purposes(
    resolved: &ResolvedType,
    kill_switches: KillSwitches,
    model: &ModelCatalogEntry,
) -> Result<(bool, bool), DomainError> {
    match resolved.kind {
        AttachmentKind::Image if kill_switches.disable_images => Err(
            DomainError::failed_precondition(PreconditionSubject::Images),
        ),
        AttachmentKind::Image => Ok((false, false)),
        AttachmentKind::Document if resolved.code_interpreter_only => {
            let available = !kill_switches.disable_code_interpreter
                && model.general_config.tool_support.code_interpreter;
            if available {
                Ok((false, true))
            } else {
                Err(DomainError::invalid_argument(
                    InvalidArgumentReason::CodeInterpreterUnavailable,
                ))
            }
        }
        AttachmentKind::Document => Ok((true, false)),
    }
}

/// The stored filename: [`DEFAULT_FILENAME`] when missing or blank,
/// truncated to [`MAX_FILENAME_CHARS`] characters keeping the extension.
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    let name = raw
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_FILENAME);
    if name.chars().count() <= MAX_FILENAME_CHARS {
        return name.to_owned();
    }
    if let Some(dot) = name.rfind('.').filter(|dot| *dot > 0) {
        let extension = &name[dot..];
        let extension_chars = extension.chars().count();
        if extension_chars < MAX_FILENAME_CHARS {
            let stem: String = name[..dot]
                .chars()
                .take(MAX_FILENAME_CHARS - extension_chars)
                .collect();
            return stem + extension;
        }
    }
    name.chars().take(MAX_FILENAME_CHARS).collect()
}

/// `min(gear limit of the kind, model max_file_size_mb)` in bytes.
#[must_use]
pub fn effective_max_bytes(
    kind: AttachmentKind,
    settings: &AttachmentSettings,
    model: &ModelCatalogEntry,
) -> u64 {
    let gear = match kind {
        AttachmentKind::Document => settings.document_max_bytes,
        AttachmentKind::Image => settings.image_max_bytes,
    };
    gear.min(u64::from(model.general_config.max_file_size_mb) * MIB)
}

// ── Upload API ───────────────────────────────────────────────────────────

/// An authorized upload into one chat, created before the body is read.
/// Holds a slot of the upload concurrency limit until dropped.
pub struct UploadSession {
    chat: ChatRecord,
    scope: AccessScope,
    model: ModelCatalogEntry,
    kill_switches: KillSwitches,
    uploader: Uuid,
    backend: String,
    /// Where an image's secondary copy goes, when the chat's provider needs
    /// one.
    secondary: Option<SecondaryTarget>,
    started: Instant,
    _slot: OwnedSemaphorePermit,
}

impl std::fmt::Debug for UploadSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadSession")
            .field("chat_id", &self.chat.id)
            .field("model", &self.model.id)
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl UploadSession {
    /// The target chat.
    #[must_use]
    pub const fn chat_id(&self) -> Uuid {
        self.chat.id
    }

    /// The chat's model, as resolved for this upload.
    #[must_use]
    pub const fn model(&self) -> &ModelCatalogEntry {
        &self.model
    }

    /// Start of the upload (the indexing deadline counts from here).
    #[must_use]
    pub const fn started(&self) -> Instant {
        self.started
    }
}

/// The validated `file` part of an upload.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent purpose flags of the attachment row"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// Stored filename.
    pub filename: String,
    /// Resolved MIME type.
    pub content_type: &'static str,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// Effective size limit in bytes.
    pub max_bytes: u64,
    /// Extension of the provider-side filename.
    pub provider_extension: &'static str,
}

impl FileSpec {
    /// Checks the bytes received so far against the effective limit.
    ///
    /// # Errors
    ///
    /// `400 out_of_range` (`content_length` / `FILE_TOO_LARGE`) above the
    /// limit.
    pub const fn check_size(&self, received: u64) -> Result<(), DomainError> {
        if received > self.max_bytes {
            Err(DomainError::out_of_range(OutOfRangeReason::FileTooLarge))
        } else {
            Ok(())
        }
    }
}

/// `AttachmentDetail`: the attachment as returned by upload and `GET`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetail {
    pub id: Uuid,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub status: AttachmentStatus,
    pub kind: AttachmentKind,
    /// Only when `status = failed`.
    pub error_code: Option<String>,
    /// Only for `kind = image` with `status = ready` and a stored thumbnail.
    pub img_thumbnail: Option<ImgThumbnail>,
    pub created_at: OffsetDateTime,
}

impl AttachmentDetail {
    /// The detail of a stored row (provider identifiers are dropped).
    #[must_use]
    pub fn from_record(record: AttachmentRecord) -> Self {
        let img_thumbnail = match (record.kind, record.status) {
            (AttachmentKind::Image, AttachmentStatus::Ready) => {
                record.thumbnail.map(|thumbnail| ImgThumbnail {
                    content_type: THUMBNAIL_CONTENT_TYPE,
                    width: thumbnail.width,
                    height: thumbnail.height,
                    data: thumbnail.data,
                })
            }
            _ => None,
        };
        let error_code = record
            .error_code
            .filter(|_| record.status == AttachmentStatus::Failed);
        Self {
            id: record.id,
            filename: record.filename,
            content_type: record.content_type,
            size_bytes: record.size_bytes,
            status: record.status,
            kind: record.kind,
            error_code,
            img_thumbnail,
            created_at: record.created_at,
        }
    }
}

/// `404 not_found` on the attachment resource.
#[must_use]
pub fn attachment_not_found(attachment_id: Uuid) -> DomainError {
    DomainError::not_found(ResourceKind::Attachment, attachment_id.to_string())
}

// ── Service ──────────────────────────────────────────────────────────────

/// The persistence and provider ports of the attachment service.
#[derive(Clone)]
pub struct AttachmentPorts {
    pub chats: Arc<dyn ChatRepository>,
    pub attachments: Arc<dyn AttachmentStore>,
    pub vector_stores: Arc<dyn VectorStoreRepository>,
    pub files: Arc<dyn FileStorage>,
    pub stores: Arc<dyn VectorStores>,
    pub thumbnails: Arc<dyn ThumbnailRenderer>,
}

impl std::fmt::Debug for AttachmentPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentPorts").finish_non_exhaustive()
    }
}

/// One upload after its row was inserted.
#[derive(Debug, Clone)]
struct Upload {
    scope: AccessScope,
    chat_id: Uuid,
    id: Uuid,
    target: StorageTarget,
    started: Instant,
}

/// How the indexing wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Indexing {
    Completed,
    Failed(String),
    /// Still `in_progress` at the deadline.
    Pending,
}

/// Secondary copies of images (module docs).
#[derive(Clone)]
struct SecondaryCopies {
    uploader: Arc<dyn SecondaryFileUploader>,
    /// Images above this size get no copy (`thumbnail.max_decode_bytes`).
    max_bytes: usize,
}

/// Attachment domain service.
pub struct AttachmentService {
    ports: AttachmentPorts,
    models: Arc<ModelsService>,
    authz: Arc<AuthzService>,
    settings: AttachmentSettings,
    slots: Arc<Semaphore>,
    shutdown: CancellationToken,
    secondary: Option<SecondaryCopies>,
}

impl std::fmt::Debug for AttachmentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentService")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl AttachmentService {
    /// Builds the service. `shutdown` stops the background indexing tasks
    /// (the gear cancels it on stop).
    #[must_use]
    pub fn new(
        ports: AttachmentPorts,
        models: Arc<ModelsService>,
        authz: Arc<AuthzService>,
        settings: AttachmentSettings,
        shutdown: CancellationToken,
    ) -> Self {
        let slots = Arc::new(Semaphore::new(usize::from(
            settings.max_concurrent_uploads.max(1),
        )));
        Self {
            ports,
            models,
            authz,
            settings,
            slots,
            shutdown,
            secondary: None,
        }
    }

    /// Creates secondary copies of images through `uploader`; images larger
    /// than `max_bytes` (`thumbnail.max_decode_bytes`) get none.
    #[must_use]
    pub fn with_secondary(
        mut self,
        uploader: Arc<dyn SecondaryFileUploader>,
        max_bytes: usize,
    ) -> Self {
        self.secondary = Some(SecondaryCopies {
            uploader,
            max_bytes,
        });
        self
    }

    /// The service settings.
    #[must_use]
    pub const fn settings(&self) -> &AttachmentSettings {
        &self.settings
    }

    /// Upload step 1, before the body is read (module docs).
    ///
    /// # Errors
    ///
    /// PEP errors of `upload_attachment`; `404` (chat); `INVALID_MODEL`;
    /// [`DomainError::Internal`] on plugin, provider configuration or storage
    /// failure; `503` (`Retry-After: 5`) when every upload slot is taken.
    pub async fn begin_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<UploadSession, DomainError> {
        let started = Instant::now();
        let scope = self
            .authz
            .authorize_chat(ctx, Action::UploadAttachment, Some(chat_id))
            .await?;
        let chat = self
            .ports
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let resolved = self
            .models
            .resolve_chat_model(ctx.subject_id(), &chat.model)
            .await?;
        let backend = self
            .ports
            .files
            .backend_for_provider(&resolved.model.provider_id)?;
        let secondary = self.secondary.as_ref().and_then(|copies| {
            copies
                .uploader
                .secondary_target(&resolved.model.provider_id, chat.tenant_id)
        });
        let slot = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| DomainError::upload_concurrency_limit())?;
        Ok(UploadSession {
            chat,
            scope,
            model: resolved.model,
            kill_switches: resolved.snapshot.kill_switches,
            uploader: ctx.subject_id(),
            backend,
            secondary,
            started,
            _slot: slot,
        })
    }

    /// Upload step 2, from the `file` part headers (module docs).
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED_CONTENT_TYPE`, `FEATURE_DISABLED` (images),
    /// `CODE_INTERPRETER_UNAVAILABLE`.
    pub fn classify_file(
        &self,
        session: &UploadSession,
        filename: Option<&str>,
        content_type: &str,
    ) -> Result<FileSpec, DomainError> {
        let filename = normalize_filename(filename);
        let resolved =
            resolve_content_type(content_type, &filename, self.settings.allow_csv_upload)?;
        let (for_file_search, for_code_interpreter) =
            purposes(&resolved, session.kill_switches, &session.model)?;
        Ok(FileSpec {
            filename,
            content_type: resolved.content_type,
            kind: resolved.kind,
            for_file_search,
            for_code_interpreter,
            max_bytes: effective_max_bytes(resolved.kind, &self.settings, &session.model),
            provider_extension: resolved.extension,
        })
    }

    /// Upload step 3, with the file bytes (module docs). Returns the detail
    /// in `ready`, or `uploaded` when indexing continues in the background.
    ///
    /// # Errors
    ///
    /// `FILE_TOO_LARGE`; `409 provider_mismatch`; `429 document_limit` /
    /// `storage_limit`; `503` (`Retry-After: 10`) on a provider or storage
    /// failure; [`DomainError::Internal`] on a database failure.
    pub async fn upload(
        &self,
        session: UploadSession,
        spec: FileSpec,
        data: Bytes,
    ) -> Result<AttachmentDetail, DomainError> {
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        spec.check_size(size)?;
        let chat_id = session.chat.id;
        if spec.for_file_search {
            self.check_store_backend(&session).await?;
        }
        let record = self
            .ports
            .attachments
            .insert_pending(
                &session.scope,
                NewAttachment {
                    id: Uuid::new_v4(),
                    tenant_id: session.chat.tenant_id,
                    chat_id,
                    uploaded_by_user_id: session.uploader,
                    filename: spec.filename.clone(),
                    content_type: spec.content_type.to_owned(),
                    size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
                    storage_backend: session.backend.clone(),
                    kind: spec.kind,
                    for_file_search: spec.for_file_search,
                    for_code_interpreter: spec.for_code_interpreter,
                    now: storage_now(),
                },
                self.limits(),
            )
            .await?;
        let upload = Upload {
            scope: session.scope.clone(),
            chat_id,
            id: record.id,
            target: StorageTarget {
                backend: session.backend.clone(),
                tenant_id: session.chat.tenant_id,
            },
            started: session.started,
        };
        let file_id = self.store_file(&upload, &spec, data.clone()).await?;
        match spec.kind {
            AttachmentKind::Image => {
                if let Some(target) = &session.secondary {
                    self.store_secondary(&upload, &spec, target, &data).await;
                }
                self.finish_image(&upload, data).await?;
            }
            AttachmentKind::Document if spec.for_file_search => {
                self.index_document(&upload, &file_id).await?;
            }
            AttachmentKind::Document => self.set_ready(&upload, None).await?,
        }
        drop(session);
        self.detail(&upload).await
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    ///
    /// PEP errors of `read_attachment`; `404` (chat); `404` (attachment) when
    /// unknown, deleted, in another chat or uploaded by another user.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetail, DomainError> {
        let scope = self
            .authorized_chat(ctx, Action::ReadAttachment, chat_id)
            .await?;
        let record = self
            .own_attachment(ctx, &scope, chat_id, attachment_id)
            .await?
            .filter(|record| record.deleted_at.is_none())
            .ok_or_else(|| attachment_not_found(attachment_id))?;
        Ok(AttachmentDetail::from_record(record))
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    ///
    /// PEP errors of `delete_attachment`; `404` (chat); `404` (attachment)
    /// when unknown, in another chat or uploaded by another user (checked
    /// before the idempotency check); `409 attachment_locked`;
    /// [`DomainError::Internal`] on a storage or outbox failure.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .authorized_chat(ctx, Action::DeleteAttachment, chat_id)
            .await?;
        let record = self
            .own_attachment(ctx, &scope, chat_id, attachment_id)
            .await?
            .ok_or_else(|| attachment_not_found(attachment_id))?;
        if record.deleted_at.is_some() {
            return Ok(());
        }
        let secondary_aliases = self
            .secondary
            .as_ref()
            .map(|copies| Arc::clone(&copies.uploader) as Arc<dyn SecondaryAliasResolver>);
        self.ports
            .attachments
            .soft_delete(
                &scope,
                chat_id,
                attachment_id,
                storage_now(),
                secondary_aliases,
            )
            .await
            .map(drop)
    }

    // ── Read / delete helpers ──

    async fn authorized_chat(
        &self,
        ctx: &SecurityContext,
        action: Action,
        chat_id: Uuid,
    ) -> Result<AccessScope, DomainError> {
        let scope = self
            .authz
            .authorize_chat(ctx, action, Some(chat_id))
            .await?;
        self.ports
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        Ok(scope)
    }

    /// The attachment of `chat_id` when the caller uploaded it.
    async fn own_attachment(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<AttachmentRecord>, DomainError> {
        Ok(self
            .ports
            .attachments
            .find(scope, chat_id, attachment_id)
            .await?
            .filter(|record| record.uploaded_by_user_id == ctx.subject_id()))
    }

    // ── Upload steps ──

    const fn limits(&self) -> ChatUploadLimits {
        ChatUploadLimits {
            max_documents: self.settings.max_documents_per_chat,
            max_total_bytes: self.settings.max_total_upload_bytes_per_chat,
        }
    }

    /// `409 provider_mismatch` when the chat's vector store belongs to
    /// another backend (checked before anything is written).
    async fn check_store_backend(&self, session: &UploadSession) -> Result<(), DomainError> {
        let existing = self
            .ports
            .vector_stores
            .find(&session.scope, session.chat.id)
            .await?;
        match existing {
            Some(row) if row.provider != session.backend => Err(DomainError::provider_mismatch()),
            _ => Ok(()),
        }
    }

    /// Provider Files API upload and `uploaded`; returns the provider file id.
    async fn store_file(
        &self,
        upload: &Upload,
        spec: &FileSpec,
        data: Bytes,
    ) -> Result<String, DomainError> {
        let file = FileUpload {
            filename: format!(
                "{}_{}.{}",
                upload.chat_id, upload.id, spec.provider_extension
            ),
            content_type: spec.content_type.to_owned(),
            data,
        };
        let file_id = match self.ports.files.upload_file(&upload.target, file).await {
            Ok(file_id) => file_id,
            Err(err) => {
                warn!(target: "mini_chat.attachments", attachment_id = %upload.id, error = %err, "provider file upload failed");
                self.fail(upload, attachment_error::UPLOAD_FAILED, None)
                    .await;
                return Err(DomainError::storage_unavailable());
            }
        };
        let marked = self
            .ports
            .attachments
            .mark_uploaded(
                &upload.scope,
                upload.chat_id,
                upload.id,
                &file_id,
                storage_now(),
            )
            .await;
        match marked {
            Ok(true) => Ok(file_id),
            Ok(false) => {
                self.spawn_file_delete(upload.target.clone(), file_id);
                Err(self.left_pending_error(upload).await)
            }
            Err(err) => {
                self.fail(upload, attachment_error::UPLOAD_FAILED, Some(file_id))
                    .await;
                Err(err)
            }
        }
    }

    /// The error of an attachment that left `pending` during the provider
    /// upload: `404` (chat) when the chat was deleted meanwhile — the same
    /// race reported by the vector store guard — internal otherwise (or
    /// when the chat state cannot be read).
    async fn left_pending_error(&self, upload: &Upload) -> DomainError {
        match self.chat_is_live(upload).await {
            Ok(false) => {
                log_info(
                    upload.id,
                    "chat deleted during the provider upload; the uploaded file is deleted",
                );
                chat_not_found(upload.chat_id)
            }
            Ok(true) => DomainError::internal(
                "attachment left the pending status during the provider upload",
            ),
            Err(err) => {
                log_warn(
                    upload.id,
                    "chat state not readable after the attachment left pending",
                    &err,
                );
                DomainError::internal(
                    "attachment left the pending status during the provider upload",
                )
            }
        }
    }

    /// Secondary copy of an image (module docs); never fails the upload.
    async fn store_secondary(
        &self,
        upload: &Upload,
        spec: &FileSpec,
        target: &SecondaryTarget,
        data: &Bytes,
    ) {
        let Some(copies) = &self.secondary else {
            return;
        };
        if data.len() > copies.max_bytes {
            debug!(target: "mini_chat.attachments", attachment_id = %upload.id, "image above thumbnail.max_decode_bytes gets no secondary copy");
            return;
        }
        let file = FileUpload {
            filename: format!(
                "{}_{}.{}",
                upload.chat_id, upload.id, spec.provider_extension
            ),
            content_type: spec.content_type.to_owned(),
            data: data.clone(),
        };
        let (copy, file_id) = match copies.uploader.upload_secondary_file(target, file).await {
            Ok(file_id) => (
                SecondaryCopy::Uploaded {
                    file_id: file_id.clone(),
                    provider_kind: target.provider_kind.clone(),
                    provider_id: target.provider_id.clone(),
                },
                Some(file_id),
            ),
            Err(err) => {
                warn!(target: "mini_chat.attachments", attachment_id = %upload.id, provider = %target.provider_id, error = %err, "secondary file upload failed");
                (
                    SecondaryCopy::Failed {
                        provider_kind: target.provider_kind.clone(),
                        provider_id: target.provider_id.clone(),
                    },
                    None,
                )
            }
        };
        let marked = self
            .ports
            .attachments
            .mark_secondary(
                &upload.scope,
                upload.chat_id,
                upload.id,
                copy,
                storage_now(),
            )
            .await;
        let keep = match marked {
            Ok(marked) => marked,
            Err(err) => {
                log_warn(upload.id, "secondary copy could not be recorded", &err);
                false
            }
        };
        if !keep && let Some(file_id) = file_id {
            let uploader = Arc::clone(&copies.uploader);
            let tenant_id = upload.target.tenant_id;
            let target = target.clone();
            tokio::spawn(async move {
                if let Err(err) = uploader
                    .delete_secondary_file(
                        tenant_id,
                        &file_id,
                        &target.provider_kind,
                        &target.upstream_alias,
                    )
                    .await
                {
                    warn!(target: "mini_chat.attachments", error = %err, "best-effort secondary file delete failed");
                }
            });
        }
    }

    /// Image: thumbnail (best effort) and `ready`.
    async fn finish_image(&self, upload: &Upload, data: Bytes) -> Result<(), DomainError> {
        let renderer = Arc::clone(&self.ports.thumbnails);
        let thumbnail = tokio::task::spawn_blocking(move || renderer.render(&data))
            .await
            .unwrap_or_else(|err| {
                warn!(target: "mini_chat.attachments", error = %err, "thumbnail task failed");
                None
            });
        self.set_ready(upload, thumbnail).await
    }

    /// `uploaded -> ready`; a row changed meanwhile (deleted, claimed by a
    /// cleanup) is left as it is.
    async fn set_ready(
        &self,
        upload: &Upload,
        thumbnail: Option<crate::domain::ports::StoredThumbnail>,
    ) -> Result<(), DomainError> {
        let updated = self
            .ports
            .attachments
            .mark_ready(
                &upload.scope,
                upload.chat_id,
                upload.id,
                thumbnail,
                storage_now(),
            )
            .await?;
        if !updated {
            debug!(target: "mini_chat.attachments", attachment_id = %upload.id, "attachment changed before it became ready");
        }
        Ok(())
    }

    /// Document with `file_search`: vector store, add, indexing wait.
    async fn index_document(&self, upload: &Upload, file_id: &str) -> Result<(), DomainError> {
        let (vector_store_id, initial) = self.add_to_vector_store(upload, file_id).await?;
        match self
            .wait_indexing(upload, &vector_store_id, file_id, initial)
            .await
        {
            Indexing::Completed => self.set_ready(upload, None).await,
            Indexing::Failed(reason) => Err(self.indexing_failed(upload, file_id, &reason).await),
            Indexing::Pending => {
                self.spawn_background(upload, vector_store_id, file_id.to_owned());
                Ok(())
            }
        }
    }

    /// The chat vector store and the add of `file_id`; returns the store id
    /// and the indexing status of the add.
    async fn add_to_vector_store(
        &self,
        upload: &Upload,
        file_id: &str,
    ) -> Result<(String, VectorStoreFileStatus), DomainError> {
        let vector_store_id = match self.chat_vector_store(upload).await {
            Ok(id) => id,
            Err(err) => {
                self.fail(
                    upload,
                    attachment_error::VECTOR_STORE_FAILED,
                    Some(file_id.to_owned()),
                )
                .await;
                return Err(err);
            }
        };
        match self
            .ports
            .stores
            .add_file(&upload.target, &vector_store_id, file_id, upload.id)
            .await
        {
            Ok(status) => Ok((vector_store_id, status)),
            Err(err) => Err(self.indexing_failed(upload, file_id, &err).await),
        }
    }

    /// `failed` / `indexing_failed`, best-effort file delete; returns the
    /// `503` of the request.
    async fn indexing_failed(
        &self,
        upload: &Upload,
        file_id: &str,
        reason: &(dyn std::fmt::Display + Sync),
    ) -> DomainError {
        log_warn(upload.id, "document indexing failed", reason);
        self.fail(
            upload,
            attachment_error::INDEXING_FAILED,
            Some(file_id.to_owned()),
        )
        .await;
        DomainError::storage_unavailable()
    }

    /// The indexing wait inside the request (module docs).
    async fn wait_indexing(
        &self,
        upload: &Upload,
        vector_store_id: &str,
        file_id: &str,
        initial: VectorStoreFileStatus,
    ) -> Indexing {
        let timing = &self.settings.timing;
        let deadline = upload.started + timing.indexing_deadline;
        let mut status = initial;
        let mut wait = timing.poll_first_wait;
        loop {
            match status {
                VectorStoreFileStatus::Completed => return Indexing::Completed,
                VectorStoreFileStatus::InProgress => {}
                other => return Indexing::Failed(format!("indexing status {other:?}")),
            }
            sleep_until(deadline.min(Instant::now() + wait)).await;
            wait = (wait * 2).min(timing.poll_max_wait);
            if Instant::now() >= deadline {
                return Indexing::Pending;
            }
            let read = timeout_at(
                deadline,
                self.ports
                    .stores
                    .file_status(&upload.target, vector_store_id, file_id),
            )
            .await;
            match read {
                Err(_) => return Indexing::Pending,
                Ok(Ok(next)) => status = next,
                Ok(Err(err)) if err.is_transient() => {
                    debug!(target: "mini_chat.attachments", attachment_id = %upload.id, error = %err, "transient indexing status read error");
                    status = VectorStoreFileStatus::InProgress;
                }
                Ok(Err(err)) => return Indexing::Failed(err.to_string()),
            }
        }
    }

    // ── Vector store creation protocol ──

    /// The provider vector store of the chat, created on first use (module
    /// docs).
    ///
    /// # Errors
    ///
    /// `409 provider_mismatch`; `503` when the store cannot be created or
    /// does not appear; [`DomainError::Internal`] on a database failure.
    async fn chat_vector_store(&self, upload: &Upload) -> Result<String, DomainError> {
        let repo = &self.ports.vector_stores;
        let mut reclaimed = false;
        loop {
            match repo.find(&upload.scope, upload.chat_id).await? {
                Some(row) if row.provider != upload.target.backend => {
                    return Err(DomainError::provider_mismatch());
                }
                Some(row) => {
                    if let Some(id) = row.vector_store_id {
                        return Ok(id);
                    }
                    if reclaimed || !self.is_stale(row.created_at) {
                        return self.await_vector_store(upload).await;
                    }
                    log_info(upload.id, "reclaiming a stale vector store placeholder");
                    repo.delete_placeholder(&upload.scope, upload.chat_id, row.id)
                        .await?;
                    reclaimed = true;
                }
                None => return self.create_vector_store(upload).await,
            }
        }
    }

    fn is_stale(&self, created_at: OffsetDateTime) -> bool {
        let max_age = self.settings.timing.stale_placeholder_after;
        let age = storage_now() - created_at;
        age.whole_milliseconds() > i128::try_from(max_age.as_millis()).unwrap_or(i128::MAX)
    }

    /// Placeholder insert and, for the winner, the provider create and the
    /// compare-and-set.
    async fn create_vector_store(&self, upload: &Upload) -> Result<String, DomainError> {
        if !self.chat_is_live(upload).await? {
            log_info(
                upload.id,
                "chat deleted before its vector store was created",
            );
            return Err(chat_not_found(upload.chat_id));
        }
        let repo = &self.ports.vector_stores;
        let row_id = Uuid::new_v4();
        let won = repo
            .insert_placeholder(
                &upload.scope,
                NewVectorStorePlaceholder {
                    id: row_id,
                    tenant_id: upload.target.tenant_id,
                    chat_id: upload.chat_id,
                    provider: upload.target.backend.clone(),
                    now: storage_now(),
                },
            )
            .await?;
        if !won {
            return self.await_vector_store(upload).await;
        }
        let vector_store_id = self.create_provider_store(upload, row_id).await?;
        if let Err(err) = self.check_chat_after_create(upload, row_id).await {
            self.spawn_store_delete(upload.target.clone(), vector_store_id);
            return Err(err);
        }
        if repo
            .set_vector_store_id(&upload.scope, upload.chat_id, row_id, &vector_store_id)
            .await?
        {
            return Ok(vector_store_id);
        }
        log_info(
            upload.id,
            "vector store placeholder was reclaimed during creation",
        );
        self.spawn_store_delete(upload.target.clone(), vector_store_id);
        self.await_vector_store(upload).await
    }

    /// Whether the chat of `upload` is still not deleted.
    async fn chat_is_live(&self, upload: &Upload) -> Result<bool, DomainError> {
        Ok(self
            .ports
            .chats
            .find_active(&upload.scope, upload.chat_id)
            .await?
            .is_some())
    }

    /// The winner's re-check after the provider create: `Err` (the
    /// placeholder removed best effort) when the chat was deleted meanwhile
    /// or its state cannot be read; the caller then deletes the new store.
    async fn check_chat_after_create(
        &self,
        upload: &Upload,
        row_id: Uuid,
    ) -> Result<(), DomainError> {
        let err = match self.chat_is_live(upload).await {
            Ok(true) => return Ok(()),
            Ok(false) => {
                log_info(
                    upload.id,
                    "chat deleted during vector store creation; the new store is deleted",
                );
                chat_not_found(upload.chat_id)
            }
            Err(err) => {
                log_warn(
                    upload.id,
                    "chat state not readable after vector store creation; the new store is deleted",
                    &err,
                );
                err
            }
        };
        if let Err(err) = self
            .ports
            .vector_stores
            .delete_placeholder(&upload.scope, upload.chat_id, row_id)
            .await
        {
            log_warn(upload.id, "vector store placeholder not removed", &err);
        }
        Err(err)
    }

    /// The winner's provider create; a failure removes the placeholder and
    /// is `503`.
    async fn create_provider_store(
        &self,
        upload: &Upload,
        row_id: Uuid,
    ) -> Result<String, DomainError> {
        let created = self
            .ports
            .stores
            .create_vector_store(&upload.target, upload.chat_id)
            .await;
        match created {
            Ok(id) => Ok(id),
            Err(err) => {
                log_warn(upload.id, "vector store creation failed", &err);
                if let Err(err) = self
                    .ports
                    .vector_stores
                    .delete_placeholder(&upload.scope, upload.chat_id, row_id)
                    .await
                {
                    log_warn(upload.id, "vector store placeholder not removed", &err);
                }
                Err(DomainError::storage_unavailable())
            }
        }
    }

    /// Loser path: polls the chat's row until it has a store id.
    async fn await_vector_store(&self, upload: &Upload) -> Result<String, DomainError> {
        let timing = &self.settings.timing;
        let mut wait = timing.store_poll_first_wait;
        for _ in 0..timing.store_polls {
            sleep(wait).await;
            wait *= 2;
            match self
                .ports
                .vector_stores
                .find(&upload.scope, upload.chat_id)
                .await?
            {
                Some(row) if row.provider != upload.target.backend => {
                    return Err(DomainError::provider_mismatch());
                }
                Some(row) => {
                    if let Some(id) = row.vector_store_id {
                        return Ok(id);
                    }
                }
                None => {}
            }
        }
        log_warn(
            upload.id,
            "vector store of the chat did not appear",
            &upload.chat_id,
        );
        Err(DomainError::storage_unavailable())
    }

    // ── Failure handling ──

    /// Marks the row `failed` with `error_code` and deletes the provider
    /// file best effort (fire-and-forget).
    async fn fail(&self, upload: &Upload, error_code: &str, file_id: Option<String>) {
        if let Err(err) = self
            .ports
            .attachments
            .mark_failed(
                &upload.scope,
                upload.chat_id,
                upload.id,
                error_code,
                storage_now(),
            )
            .await
        {
            error!(target: "mini_chat.attachments", attachment_id = %upload.id, error = %err, "failed upload could not be recorded");
        }
        if let Some(file_id) = file_id {
            self.spawn_file_delete(upload.target.clone(), file_id);
        }
    }

    fn spawn_file_delete(&self, target: StorageTarget, file_id: String) {
        let files = Arc::clone(&self.ports.files);
        tokio::spawn(async move {
            if let Err(err) = files.delete_file(&target, &file_id).await {
                warn!(target: "mini_chat.attachments", error = %err, "best-effort provider file delete failed");
            }
        });
    }

    fn spawn_store_delete(&self, target: StorageTarget, vector_store_id: String) {
        let stores = Arc::clone(&self.ports.stores);
        tokio::spawn(async move {
            if let Err(err) = stores.delete_vector_store(&target, &vector_store_id).await {
                warn!(target: "mini_chat.attachments", error = %err, "best-effort vector store delete failed");
            }
        });
    }

    fn spawn_background(&self, upload: &Upload, vector_store_id: String, file_id: String) {
        let task = BackgroundIndexing {
            attachments: Arc::clone(&self.ports.attachments),
            stores: Arc::clone(&self.ports.stores),
            timing: self.settings.timing,
            shutdown: self.shutdown.clone(),
            scope: upload.scope.clone(),
            chat_id: upload.chat_id,
            id: upload.id,
            target: upload.target.clone(),
            vector_store_id,
            file_id,
        };
        info!(target: "mini_chat.attachments", attachment_id = %upload.id, "indexing continues in the background");
        tokio::spawn(task.run());
    }

    async fn detail(&self, upload: &Upload) -> Result<AttachmentDetail, DomainError> {
        self.ports
            .attachments
            .find(&upload.scope, upload.chat_id, upload.id)
            .await?
            .map(AttachmentDetail::from_record)
            .ok_or_else(|| DomainError::internal("uploaded attachment row is missing"))
    }
}

// ── Background indexing ──────────────────────────────────────────────────

/// Outcome of one background status read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Poll {
    Completed,
    InProgress,
    Failed(String),
    Stopped,
}

/// The background indexing task of one attachment (module docs). Not
/// persisted: a stopped process leaves the row `uploaded` for the upload
/// reaper.
pub struct BackgroundIndexing {
    attachments: Arc<dyn AttachmentStore>,
    stores: Arc<dyn VectorStores>,
    timing: UploadTiming,
    shutdown: CancellationToken,
    scope: AccessScope,
    chat_id: Uuid,
    id: Uuid,
    target: StorageTarget,
    vector_store_id: String,
    file_id: String,
}

impl std::fmt::Debug for BackgroundIndexing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundIndexing")
            .field("attachment_id", &self.id)
            .finish_non_exhaustive()
    }
}

impl BackgroundIndexing {
    /// Runs the task to its end.
    pub async fn run(self) {
        let limit = Instant::now() + self.timing.background_limit;
        let mut wait = self.timing.poll_first_wait;
        let mut last_transient: Option<String> = None;
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            let now = Instant::now();
            if now >= limit {
                let reason = last_transient.map_or_else(
                    || "background indexing timed out".to_owned(),
                    |err| format!("background indexing timed out; last read error: {err}"),
                );
                self.fail(&reason).await;
                return;
            }
            if !self.heartbeat().await {
                return;
            }
            let round_end = limit.min(now + self.timing.background_round);
            while Instant::now() < round_end {
                match self.poll(round_end, &mut wait, &mut last_transient).await {
                    Poll::InProgress => {}
                    Poll::Completed => {
                        self.set_ready().await;
                        return;
                    }
                    Poll::Failed(reason) => {
                        self.fail(&reason).await;
                        return;
                    }
                    Poll::Stopped => return,
                }
            }
        }
    }

    /// Refreshes `updated_at`; `false` stops the task (the row is no longer
    /// `uploaded`, was deleted or claimed by a cleanup).
    async fn heartbeat(&self) -> bool {
        match self
            .attachments
            .touch_uploaded(&self.scope, self.chat_id, self.id, storage_now())
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                debug!(target: "mini_chat.attachments", attachment_id = %self.id, "background indexing stopped: attachment changed");
                false
            }
            Err(err) => {
                warn!(target: "mini_chat.attachments", attachment_id = %self.id, error = %err, "background indexing heartbeat failed");
                true
            }
        }
    }

    /// One wait and status read inside the round ending at `round_end`.
    async fn poll(
        &self,
        round_end: Instant,
        wait: &mut Duration,
        last_transient: &mut Option<String>,
    ) -> Poll {
        let wake = round_end.min(Instant::now() + *wait);
        *wait = (*wait * 2).min(self.timing.background_max_wait);
        tokio::select! {
            () = self.shutdown.cancelled() => return Poll::Stopped,
            () = sleep_until(wake) => {}
        }
        let read = tokio::select! {
            () = self.shutdown.cancelled() => return Poll::Stopped,
            read = timeout(
                self.timing.background_round,
                self.stores.file_status(&self.target, &self.vector_store_id, &self.file_id),
            ) => read,
        };
        match read {
            Ok(Ok(VectorStoreFileStatus::Completed)) => Poll::Completed,
            Ok(Ok(VectorStoreFileStatus::InProgress)) => Poll::InProgress,
            Ok(Ok(other)) => Poll::Failed(format!("indexing status {other:?}")),
            Ok(Err(err)) if err.is_transient() => {
                if last_transient.is_none() {
                    warn!(target: "mini_chat.attachments", attachment_id = %self.id, error = %err, "transient indexing status read error");
                }
                *last_transient = Some(err.to_string());
                Poll::InProgress
            }
            Ok(Err(err)) => Poll::Failed(err.to_string()),
            Err(_) => {
                *last_transient = Some("status read timed out".to_owned());
                Poll::InProgress
            }
        }
    }

    /// `ready`, 4 attempts 1 / 2 / 4 s apart; a row that changed meanwhile
    /// is left as it is.
    async fn set_ready(&self) {
        let mut backoff = self.timing.set_ready_backoff.iter();
        loop {
            let err = match self
                .attachments
                .mark_ready(&self.scope, self.chat_id, self.id, None, storage_now())
                .await
            {
                Ok(true) => return log_info(self.id, "background indexing completed"),
                Ok(false) => {
                    return log_debug(self.id, "attachment changed before it became ready");
                }
                Err(err) => err,
            };
            let Some(delay) = backoff.next() else {
                return log_error(
                    self.id,
                    "attachment indexed but not set ready (set_ready_failed); left to the upload reaper",
                    &err,
                );
            };
            log_warn(
                self.id,
                "setting the attachment ready failed; retrying",
                &err,
            );
            if self.pause(*delay).await {
                return;
            }
        }
    }

    /// Waits `delay`; `true` when the gear stopped meanwhile.
    async fn pause(&self, delay: Duration) -> bool {
        tokio::select! {
            () = self.shutdown.cancelled() => true,
            () = sleep(delay) => false,
        }
    }

    /// `failed` / `indexing_failed` with the cleanup message (one
    /// transaction).
    async fn fail(&self, reason: &str) {
        match self
            .attachments
            .fail_indexing_with_cleanup(&self.scope, self.chat_id, self.id, storage_now())
            .await
        {
            Ok(true) => log_warn(self.id, "background indexing failed", &reason),
            Ok(false) => log_debug(
                self.id,
                "attachment changed before the indexing failure was recorded",
            ),
            Err(err) => log_error(
                self.id,
                "background indexing failure could not be recorded",
                &err,
            ),
        }
    }
}

// ── Logging ──────────────────────────────────────────────────────────────

fn log_debug(attachment_id: Uuid, message: &str) {
    debug!(target: "mini_chat.attachments", attachment_id = %attachment_id, "{message}");
}

fn log_info(attachment_id: Uuid, message: &str) {
    info!(target: "mini_chat.attachments", attachment_id = %attachment_id, "{message}");
}

fn log_warn(attachment_id: Uuid, message: &str, detail: &dyn std::fmt::Display) {
    warn!(target: "mini_chat.attachments", attachment_id = %attachment_id, detail = %detail, "{message}");
}

fn log_error(attachment_id: Uuid, message: &str, detail: &dyn std::fmt::Display) {
    error!(target: "mini_chat.attachments", attachment_id = %attachment_id, detail = %detail, "{message}");
}

#[cfg(test)]
#[path = "attachment_service_tests.rs"]
pub(crate) mod tests;
