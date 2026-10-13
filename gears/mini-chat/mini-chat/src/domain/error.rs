//! Domain error taxonomy and its canonical REST mapping (DESIGN section 3.3
//! "Error Codes", ADR-0004).
//!
//! Every pre-stream REST failure is a [`DomainError`] variant carrying exactly
//! the data of its wire contract. `From<DomainError> for CanonicalError`
//! produces the canonical category (and thereby the HTTP status) plus the
//! machine-readable fields: `context.reason`, `context.field_violations[]`,
//! `context.violations[]`, `context.resource_type`, `context.resource_name`
//! and `context.retry_after_seconds`.
//!
//! `detail` texts are generic and fixed per variant: they never carry internal
//! ids, driver messages or provider text. Diagnostics of [`DomainError::Internal`]
//! stay server-side (logged by the platform error middleware, never serialized).

use mini_chat_sdk::resource_types;
use thiserror::Error;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_macros::domain_model;

// ── Resource kinds ───────────────────────────────────────────────────────

/// Mini-chat resources named by `context.resource_type` (ADR-0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

impl ResourceKind {
    /// GTS error resource type (`gts.cf.core.mini_chat.<kind>.v1~`).
    #[must_use]
    pub const fn resource_type(self) -> &'static str {
        match self {
            Self::Chat => resource_types::CHAT_RESOURCE_TYPE,
            Self::Message => resource_types::MESSAGE_RESOURCE_TYPE,
            Self::Turn => resource_types::TURN_RESOURCE_TYPE,
            Self::Attachment => resource_types::ATTACHMENT_RESOURCE_TYPE,
            Self::Model => resource_types::MODEL_RESOURCE_TYPE,
        }
    }

    /// Short lowercase name used in texts.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Message => "message",
            Self::Turn => "turn",
            Self::Attachment => "attachment",
            Self::Model => "model",
        }
    }

    const fn not_found_detail(self) -> &'static str {
        match self {
            Self::Chat => "Chat not found",
            Self::Message => "Message not found",
            Self::Turn => "Turn not found",
            Self::Attachment => "Attachment not found",
            Self::Model => "Model not found",
        }
    }
}

// ── Machine-readable reasons ─────────────────────────────────────────────

/// `invalid_argument` reasons (`context.field_violations[].reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidArgumentReason {
    /// Unknown or disabled model, or the chat's model left the catalog.
    InvalidModel,
    /// Empty or whitespace-only message content.
    EmptyContent,
    /// Chat title empty after trim or longer than 255 characters.
    InvalidTitle,
    /// Reaction value other than `like` / `dislike`.
    InvalidReaction,
    /// Invalid, duplicate, foreign or not-ready `attachment_ids`, or too many.
    InvalidAttachment,
    /// Unsupported upload MIME type.
    UnsupportedContentType,
    /// Image on a model without `VISION_INPUT`.
    VisionNotSupported,
    /// Code-interpreter-only upload while code interpreter is unavailable.
    CodeInterpreterUnavailable,
    /// Multipart `Content-Type` without a boundary.
    BoundaryRequired,
    /// Unreadable multipart body.
    MultipartError,
    /// No `file` field in the multipart body.
    MissingFile,
    /// `file` part without a content type.
    MissingContentType,
}

impl InvalidArgumentReason {
    /// Wire reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidModel => "INVALID_MODEL",
            Self::EmptyContent => "EMPTY_CONTENT",
            Self::InvalidTitle => "INVALID_TITLE",
            Self::InvalidReaction => "INVALID_REACTION",
            Self::InvalidAttachment => "invalid_attachment",
            Self::UnsupportedContentType => "UNSUPPORTED_CONTENT_TYPE",
            Self::VisionNotSupported => "VISION_NOT_SUPPORTED",
            Self::CodeInterpreterUnavailable => "CODE_INTERPRETER_UNAVAILABLE",
            Self::BoundaryRequired => "BOUNDARY_REQUIRED",
            Self::MultipartError => "MULTIPART_ERROR",
            Self::MissingFile => "MISSING_FILE",
            Self::MissingContentType => "MISSING_CONTENT_TYPE",
        }
    }

    /// Field named by the violation (DESIGN error table).
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::InvalidModel => "model",
            Self::EmptyContent => "content",
            Self::InvalidTitle => "title",
            Self::InvalidReaction => "reaction",
            Self::InvalidAttachment => "attachment",
            Self::UnsupportedContentType
            | Self::VisionNotSupported
            | Self::BoundaryRequired
            | Self::MissingContentType => "content_type",
            Self::CodeInterpreterUnavailable | Self::MissingFile => "file",
            Self::MultipartError => "multipart",
        }
    }

    /// Resource the error is scoped to (`context.resource_type`).
    #[must_use]
    pub const fn resource(self) -> ResourceKind {
        match self {
            Self::InvalidModel => ResourceKind::Model,
            Self::InvalidTitle => ResourceKind::Chat,
            Self::EmptyContent | Self::InvalidReaction | Self::VisionNotSupported => {
                ResourceKind::Message
            }
            Self::InvalidAttachment
            | Self::UnsupportedContentType
            | Self::CodeInterpreterUnavailable
            | Self::BoundaryRequired
            | Self::MultipartError
            | Self::MissingFile
            | Self::MissingContentType => ResourceKind::Attachment,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::InvalidModel => "The model is unknown or not available",
            Self::EmptyContent => "Message content must not be empty",
            Self::InvalidTitle => "Chat title must not be empty and must be at most 255 characters",
            Self::InvalidReaction => "Reaction must be `like` or `dislike`",
            Self::InvalidAttachment => {
                "Attachments must be distinct, ready attachments of this chat"
            }
            Self::UnsupportedContentType => "The file type is not supported",
            Self::VisionNotSupported => "The model does not accept image input",
            Self::CodeInterpreterUnavailable => {
                "This file type requires code interpreter, which is not available"
            }
            Self::BoundaryRequired => "The multipart content type has no boundary",
            Self::MultipartError => "The multipart body could not be read",
            Self::MissingFile => "The multipart body has no `file` field",
            Self::MissingContentType => "The `file` part has no content type",
        }
    }
}

/// `out_of_range` reasons (`context.field_violations[].reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfRangeReason {
    /// Upload larger than the effective per-file limit.
    FileTooLarge,
    /// More images in one message than `rag.max_images_per_message`.
    TooManyImages,
    /// Message estimate above the model's `max_input_tokens`.
    InputTooLong,
    /// Mandatory context does not fit the input budget.
    ContextBudgetExceeded,
}

impl OutOfRangeReason {
    /// Wire reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileTooLarge => "FILE_TOO_LARGE",
            Self::TooManyImages => "TOO_MANY_IMAGES",
            Self::InputTooLong => "INPUT_TOO_LONG",
            Self::ContextBudgetExceeded => "CONTEXT_BUDGET_EXCEEDED",
        }
    }

    /// Field named by the violation (DESIGN error table).
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::FileTooLarge => "content_length",
            Self::TooManyImages => "image_count",
            Self::InputTooLong | Self::ContextBudgetExceeded => "content",
        }
    }

    /// Resource the error is scoped to (`context.resource_type`).
    #[must_use]
    pub const fn resource(self) -> ResourceKind {
        match self {
            Self::FileTooLarge => ResourceKind::Attachment,
            Self::TooManyImages | Self::InputTooLong | Self::ContextBudgetExceeded => {
                ResourceKind::Message
            }
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::FileTooLarge => "The file exceeds the maximum upload size",
            Self::TooManyImages => "The message has too many images",
            Self::InputTooLong => "The message exceeds the model's input limit",
            Self::ContextBudgetExceeded => {
                "The required context does not fit the model's input budget"
            }
        }
    }
}

/// `failed_precondition` subjects (`context.violations[].subject`); the
/// violation `type` follows from the subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreconditionSubject {
    /// Web search kill switch is on (`FEATURE_DISABLED`).
    WebSearch,
    /// Image kill switch is on (`FEATURE_DISABLED`).
    Images,
    /// Retry/edit/delete of a non-terminal turn (`STATE`).
    TurnState,
    /// Reaction on a non-assistant message (`STATE`).
    ReactionTarget,
}

impl PreconditionSubject {
    /// Wire subject.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::Images => "images",
            Self::TurnState => "turn_state",
            Self::ReactionTarget => "reaction_target",
        }
    }

    /// Wire violation type.
    #[must_use]
    pub const fn violation_type(self) -> &'static str {
        match self {
            Self::WebSearch | Self::Images => "FEATURE_DISABLED",
            Self::TurnState | Self::ReactionTarget => "STATE",
        }
    }

    const fn resource(self) -> ResourceKind {
        match self {
            Self::WebSearch | Self::Images | Self::ReactionTarget => ResourceKind::Message,
            Self::TurnState => ResourceKind::Turn,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::WebSearch => "Web search is currently disabled",
            Self::Images => "Image input is currently disabled",
            Self::TurnState => "The turn has not finished yet",
            Self::ReactionTarget => "Only assistant messages can receive reactions",
        }
    }
}

/// `aborted` reasons (`context.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    /// Another turn is running in the chat (including the insert race).
    TurnAlreadyRunning,
    /// `request_id` reused for a non-completed or deleted turn.
    RequestIdConflict,
    /// Mutation of a turn that is not the latest (or already deleted).
    NotLatestTurn,
    /// Concurrent mutation lost the running-turn race.
    GenerationInProgress,
    /// Replay of a completed turn (defensive; the stream handler serves the replay).
    Replay,
}

impl AbortReason {
    /// Wire reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnAlreadyRunning => "turn_already_running",
            Self::RequestIdConflict => "request_id_conflict",
            Self::NotLatestTurn => "NOT_LATEST_TURN",
            Self::GenerationInProgress => "GENERATION_IN_PROGRESS",
            Self::Replay => "REPLAY",
        }
    }

    const fn detail(self) -> &'static str {
        match self {
            Self::TurnAlreadyRunning => "Another turn is already running in this chat",
            Self::RequestIdConflict => "The request id is already used by another turn",
            Self::NotLatestTurn => "Only the latest turn can be modified",
            Self::GenerationInProgress => "Another change to this chat is in progress",
            Self::Replay => "The turn for this request id has already completed",
        }
    }
}

/// Per-user quota scopes (`context.violations[].subject` of `quota_exceeded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    Tokens,
    WebSearch,
    CodeInterpreter,
}

impl QuotaScope {
    /// Wire subject.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

/// Exhausted limit of a `resource_exhausted` error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExhaustedLimit {
    /// Per-user quota: `violations[{subject: <scope>, description: "quota_exceeded"}]`.
    Quota(QuotaScope),
    /// Per-chat document count (`rag.max_documents_per_chat`).
    DocumentLimit,
    /// Per-chat total upload size (`rag.max_total_upload_mb_per_chat`).
    StorageLimit,
}

impl ExhaustedLimit {
    /// Wire violation subject.
    #[must_use]
    pub const fn subject(self) -> &'static str {
        match self {
            Self::Quota(scope) => scope.as_str(),
            Self::DocumentLimit => "document_limit",
            Self::StorageLimit => "storage_limit",
        }
    }

    /// Wire violation description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Quota(_) => "quota_exceeded",
            Self::DocumentLimit => "The chat has reached its document limit",
            Self::StorageLimit => "The chat has reached its storage limit",
        }
    }

    const fn resource(self) -> ResourceKind {
        match self {
            Self::Quota(_) => ResourceKind::Message,
            Self::DocumentLimit | Self::StorageLimit => ResourceKind::Attachment,
        }
    }

    const fn detail(self) -> &'static str {
        match self {
            Self::Quota(_) => "Quota exceeded",
            Self::DocumentLimit => "Document limit reached",
            Self::StorageLimit => "Storage limit reached",
        }
    }
}

/// `already_exists` conflict codes (`context.resource_name`).
pub mod conflict {
    /// Deleting an attachment referenced by a submitted message.
    pub const ATTACHMENT_LOCKED: &str = "attachment_locked";
    /// Upload into a chat whose vector store belongs to another provider backend.
    pub const PROVIDER_MISMATCH: &str = "provider_mismatch";
    /// Unhandled unique-constraint violation reported by the persistence layer.
    pub const UNIQUE_VIOLATION: &str = "unique_violation";
}

/// `Retry-After` of a PDP failure (seconds).
pub const PDP_RETRY_AFTER_SECS: u64 = 5;
/// `Retry-After` of a storage backend failure on upload (seconds).
pub const STORAGE_RETRY_AFTER_SECS: u64 = 10;
/// `Retry-After` of the upload concurrency limit (seconds).
pub const UPLOAD_CONCURRENCY_RETRY_AFTER_SECS: u64 = 5;

// ── DomainError ──────────────────────────────────────────────────────────

/// Mini-chat domain error; one variant per row family of the REST error table.
#[domain_model]
#[derive(Debug, Error)]
pub enum DomainError {
    /// 404 `not_found`; `id` is the identifier the caller asked for and is
    /// reported as `context.resource_name`.
    #[error("{} not found", resource.name())]
    NotFound { resource: ResourceKind, id: String },

    /// 400 `invalid_argument` with `field_violations[{field, reason}]`.
    #[error("invalid argument `{field}`: {}", reason.as_str())]
    InvalidArgument {
        field: &'static str,
        reason: InvalidArgumentReason,
    },

    /// 400 `invalid_argument` with `context.format` (e.g. the chat-cleanup
    /// outbox payload exceeds the outbox size limit). `message` is client-safe
    /// text chosen by the caller; it becomes `detail` and `context.format`.
    #[error("invalid request: {message}")]
    InvalidFormat { message: String },

    /// 400 `invalid_argument` from the `OData` query layer; reasons and fields
    /// come from `toolkit-odata` and `context.resource_type` is the `OData`
    /// query type. Its `Db` / `ParsingUnavailable` cases map to 500 `internal`.
    #[error("invalid query: {0}")]
    InvalidQuery(toolkit_odata::Error),

    /// 400 `out_of_range` with `field_violations[{field, reason}]`.
    #[error("out of range `{field}`: {}", reason.as_str())]
    OutOfRange {
        field: &'static str,
        reason: OutOfRangeReason,
    },

    /// 400 `failed_precondition` with `violations[{subject, type}]`.
    #[error("failed precondition: {}", subject.as_str())]
    FailedPrecondition { subject: PreconditionSubject },

    /// 403 `permission_denied`, `context.reason = AUTHZ_DENIED` (fail-closed).
    #[error("access to {} denied", resource.name())]
    PermissionDenied { resource: ResourceKind },

    /// 503 `service_unavailable`, `Retry-After: 5`: the PDP could not evaluate
    /// the request; access is refused. The cause is only logged.
    #[error("authorization decision unavailable")]
    PdpUnavailable,

    /// 409 `aborted`, `context.reason`.
    #[error("aborted: {}", reason.as_str())]
    Aborted { reason: AbortReason },

    /// 409 `already_exists`; `resource_name` is the conflict code (see [`conflict`]).
    #[error("conflict: {resource_name}")]
    AlreadyExists {
        resource: ResourceKind,
        resource_name: &'static str,
    },

    /// 429 `resource_exhausted` with `violations[{subject, description}]`.
    #[error("resource exhausted: {}", limit.subject())]
    ResourceExhausted { limit: ExhaustedLimit },

    /// 503 `service_unavailable` with `Retry-After` (storage backend failure:
    /// 10 s; upload concurrency limit: 5 s).
    #[error("service unavailable, retry after {retry_after_secs}s")]
    ServiceUnavailable { retry_after_secs: u64 },

    /// 500 `internal`; the diagnostic is never serialized.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// 404 for `resource` identified by `id`.
    #[must_use]
    pub fn not_found(resource: ResourceKind, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource,
            id: id.into(),
        }
    }

    /// 400 `invalid_argument` on the reason's canonical field.
    #[must_use]
    pub const fn invalid_argument(reason: InvalidArgumentReason) -> Self {
        Self::InvalidArgument {
            field: reason.field(),
            reason,
        }
    }

    /// 400 `out_of_range` on the reason's canonical field.
    #[must_use]
    pub const fn out_of_range(reason: OutOfRangeReason) -> Self {
        Self::OutOfRange {
            field: reason.field(),
            reason,
        }
    }

    /// 400 `failed_precondition`.
    #[must_use]
    pub const fn failed_precondition(subject: PreconditionSubject) -> Self {
        Self::FailedPrecondition { subject }
    }

    /// 409 `aborted`.
    #[must_use]
    pub const fn aborted(reason: AbortReason) -> Self {
        Self::Aborted { reason }
    }

    /// 409 `already_exists`, `resource_name = attachment_locked`.
    #[must_use]
    pub const fn attachment_locked() -> Self {
        Self::AlreadyExists {
            resource: ResourceKind::Attachment,
            resource_name: conflict::ATTACHMENT_LOCKED,
        }
    }

    /// 409 `already_exists`, `resource_name = provider_mismatch`.
    #[must_use]
    pub const fn provider_mismatch() -> Self {
        Self::AlreadyExists {
            resource: ResourceKind::Attachment,
            resource_name: conflict::PROVIDER_MISMATCH,
        }
    }

    /// 409 `already_exists`, `resource_name = unique_violation`.
    #[must_use]
    pub const fn unique_violation(resource: ResourceKind) -> Self {
        Self::AlreadyExists {
            resource,
            resource_name: conflict::UNIQUE_VIOLATION,
        }
    }

    /// 429 `quota_exceeded` for `scope`.
    #[must_use]
    pub const fn quota_exceeded(scope: QuotaScope) -> Self {
        Self::ResourceExhausted {
            limit: ExhaustedLimit::Quota(scope),
        }
    }

    /// 503, `Retry-After: 10`: provider Files / vector store failure on upload.
    #[must_use]
    pub const fn storage_unavailable() -> Self {
        Self::ServiceUnavailable {
            retry_after_secs: STORAGE_RETRY_AFTER_SECS,
        }
    }

    /// 503, `Retry-After: 5`: the per-process upload concurrency limit is reached.
    #[must_use]
    pub const fn upload_concurrency_limit() -> Self {
        Self::ServiceUnavailable {
            retry_after_secs: UPLOAD_CONCURRENCY_RETRY_AFTER_SECS,
        }
    }

    /// 500 with a server-side diagnostic.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal(diagnostic.into())
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::InvalidQuery(err)
    }
}

// ── Canonical mapping ────────────────────────────────────────────────────

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
struct ChatResource;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
struct MessageResource;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
struct TurnResource;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
struct AttachmentResource;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
struct ModelResource;

/// Dispatches a builder constructor of the resource marker of a [`ResourceKind`].
macro_rules! for_resource {
    ($kind:expr, |$marker:ident| $build:expr) => {
        match $kind {
            ResourceKind::Chat => {
                type $marker = ChatResource;
                $build
            }
            ResourceKind::Message => {
                type $marker = MessageResource;
                $build
            }
            ResourceKind::Turn => {
                type $marker = TurnResource;
                $build
            }
            ResourceKind::Attachment => {
                type $marker = AttachmentResource;
                $build
            }
            ResourceKind::Model => {
                type $marker = ModelResource;
                $build
            }
        }
    };
}

const AUTHZ_DENIED: &str = "AUTHZ_DENIED";

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound { resource, id } => for_resource!(resource, |R| {
                R::not_found(resource.not_found_detail())
                    .with_resource(id)
                    .create()
            }),
            DomainError::InvalidArgument { field, reason } => {
                for_resource!(reason.resource(), |R| {
                    R::invalid_argument()
                        .with_field_violation(field, reason.description(), reason.as_str())
                        .create()
                })
            }
            DomainError::InvalidFormat { message } => ChatResource::invalid_argument()
                .with_format(message)
                .create(),
            DomainError::InvalidQuery(odata) => Self::from(odata),
            DomainError::OutOfRange { field, reason } => for_resource!(reason.resource(), |R| {
                R::out_of_range(reason.description())
                    .with_field_violation(field, reason.description(), reason.as_str())
                    .create()
            }),
            DomainError::FailedPrecondition { subject } => {
                for_resource!(subject.resource(), |R| {
                    R::failed_precondition()
                        .with_precondition_violation(
                            subject.as_str(),
                            subject.description(),
                            subject.violation_type(),
                        )
                        .create()
                })
            }
            DomainError::PermissionDenied { resource } => for_resource!(resource, |R| {
                R::permission_denied().with_reason(AUTHZ_DENIED).create()
            }),
            DomainError::PdpUnavailable => Self::service_unavailable()
                .with_retry_after_seconds(PDP_RETRY_AFTER_SECS)
                .create(),
            DomainError::Aborted { reason } => TurnResource::aborted(reason.detail())
                .with_reason(reason.as_str())
                .create(),
            DomainError::AlreadyExists {
                resource,
                resource_name,
            } => for_resource!(resource, |R| {
                R::already_exists(conflict_detail(resource_name))
                    .with_resource(resource_name)
                    .create()
            }),
            DomainError::ResourceExhausted { limit } => for_resource!(limit.resource(), |R| {
                R::resource_exhausted(limit.detail())
                    .with_quota_violation(limit.subject(), limit.description())
                    .create()
            }),
            DomainError::ServiceUnavailable { retry_after_secs } => Self::service_unavailable()
                .with_retry_after_seconds(retry_after_secs)
                .create(),
            DomainError::Internal(diagnostic) => Self::internal(diagnostic).create(),
        }
    }
}

fn conflict_detail(code: &str) -> &'static str {
    match code {
        conflict::ATTACHMENT_LOCKED => "The attachment is referenced by a message",
        conflict::PROVIDER_MISMATCH => "The chat's documents are stored by another provider",
        _ => "The resource already exists",
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
