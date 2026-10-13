//! Domain error taxonomy.
//!
//! Every condition of the REST error mapping table (DESIGN section 3.3, ADR-0004) has a variant
//! here; the mapping to canonical errors lives in `api::rest::error`.

use thiserror::Error;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The protected resource an authorization failure refers to.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedResource {
    /// A chat and everything reached through it.
    Chat,
    /// The model catalog.
    Model,
    /// The per-user quota.
    UserQuota,
}

/// A problem with an upload multipart request.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartIssue {
    /// No boundary in the `Content-Type`.
    BoundaryRequired,
    /// The multipart body is unreadable.
    Malformed,
    /// No `file` field.
    MissingFile,
    /// The `file` part has no content type.
    MissingContentType,
}

/// A feature turned off by a kill switch.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledFeature {
    /// Web search.
    WebSearch,
    /// Image inputs and uploads.
    Images,
}

/// The scope of an exhausted quota.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    /// Credit (token) quota.
    Tokens,
    /// Daily web search calls.
    WebSearch,
    /// Daily code interpreter calls.
    CodeInterpreter,
}

/// The domain error of the mini-chat gear.
#[domain_model]
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// The chat does not exist, is soft-deleted or belongs to someone else.
    #[error("chat {chat_id} not found")]
    ChatNotFound {
        /// The requested chat.
        chat_id: Uuid,
    },
    /// The message does not exist in the chat.
    #[error("message {message_id} not found")]
    MessageNotFound {
        /// The requested message.
        message_id: Uuid,
    },
    /// The turn does not exist in the chat.
    #[error("turn {request_id} not found")]
    TurnNotFound {
        /// The turn's request id.
        request_id: Uuid,
    },
    /// The attachment does not exist in the chat.
    #[error("attachment {attachment_id} not found")]
    AttachmentNotFound {
        /// The requested attachment.
        attachment_id: Uuid,
    },
    /// The model is not visible (unknown or globally disabled).
    #[error("model {model_id} not found")]
    ModelNotFound {
        /// The requested model id.
        model_id: String,
    },
    /// Unknown or disabled model (`field_violations[model].reason = INVALID_MODEL`).
    #[error("invalid model: {detail}")]
    InvalidModel {
        /// Why the model was refused.
        detail: String,
    },
    /// Empty, blank or over-long chat title.
    #[error("invalid chat title: {detail}")]
    InvalidTitle {
        /// Why the title was refused.
        detail: String,
    },
    /// Empty or whitespace-only message content.
    #[error("message content must not be empty")]
    EmptyContent,
    /// The reaction is neither `like` nor `dislike`.
    #[error("reaction must be `like` or `dislike`")]
    InvalidReaction,
    /// Unsupported upload MIME type.
    #[error("unsupported upload content type")]
    UnsupportedContentType,
    /// Code-interpreter-only upload while the tool is unavailable.
    #[error("code interpreter is unavailable")]
    CodeInterpreterUnavailable,
    /// The upload request is not valid multipart.
    #[error("invalid multipart upload request")]
    InvalidMultipart {
        /// The specific problem.
        issue: MultipartIssue,
    },
    /// Image input on a model without vision.
    #[error("the effective model does not support image input")]
    VisionNotSupported,
    /// Invalid, duplicate, foreign or not-ready attachment ids.
    #[error("invalid attachment reference")]
    InvalidAttachment,
    /// Upload larger than the limit.
    #[error("file too large")]
    FileTooLarge,
    /// Too many images in one message.
    #[error("too many images in one message")]
    TooManyImages,
    /// The message exceeds `max_input_tokens`.
    #[error("input too long")]
    InputTooLong,
    /// The mandatory context does not fit the budget.
    #[error("context budget exceeded")]
    ContextBudgetExceeded,
    /// A kill switch refuses the request.
    #[error("feature disabled")]
    FeatureDisabled {
        /// The disabled feature.
        feature: DisabledFeature,
    },
    /// Retry, edit or delete of a non-terminal turn.
    #[error("the turn is not in a terminal state")]
    InvalidTurnState,
    /// Reaction on a non-assistant message.
    #[error("reactions apply to assistant messages only")]
    InvalidReactionTarget,
    /// The PDP denied access (or its constraints could not be compiled): fail-closed.
    #[error("authorization denied")]
    AuthzDenied {
        /// The resource the decision was about.
        resource: ProtectedResource,
    },
    /// The PDP could not evaluate the request: access is still refused.
    #[error("authorization service unavailable")]
    AuthzUnavailable,
    /// Another turn is running in the chat.
    #[error("another turn is already running in the chat")]
    TurnAlreadyRunning,
    /// `request_id` reused for a non-completed or deleted turn.
    #[error("request id conflict")]
    RequestIdConflict,
    /// Mutation of a turn that is not the latest.
    #[error("not the latest turn")]
    NotLatestTurn,
    /// A concurrent mutation lost the running-turn race.
    #[error("generation in progress")]
    GenerationInProgress,
    /// Defensive fallback: a completed turn is replayed.
    #[error("replay")]
    Replay,
    /// Deleting an attachment referenced by a message.
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    /// The chat's vector store belongs to another provider backend.
    #[error("vector store provider mismatch")]
    ProviderMismatch,
    /// A unique constraint the caller does not handle.
    #[error("unique constraint violated")]
    UniqueViolation,
    /// A quota is exhausted.
    #[error("quota exceeded")]
    QuotaExceeded {
        /// The exhausted quota.
        scope: QuotaScope,
    },
    /// Per-chat document count limit.
    #[error("document limit reached")]
    DocumentLimit,
    /// Per-chat storage limit.
    #[error("storage limit reached")]
    StorageLimit,
    /// The provider storage backend failed on upload.
    #[error("storage backend unavailable")]
    StorageUnavailable,
    /// Upload concurrency limit.
    #[error("upload concurrency limit reached")]
    UploadConcurrencyLimit,
    /// The chat-cleanup outbox payload exceeds the outbox size limit.
    #[error("{detail}")]
    OutboxPayloadTooLarge {
        /// Human-readable description.
        detail: String,
    },
    /// An `OData` query error (filter, order, cursor, page size).
    #[error("odata query error: {0}")]
    Odata(toolkit_odata::Error),
    /// Provider or policy resolution failure.
    #[error("policy unavailable: {diagnostic}")]
    PolicyUnavailable {
        /// Operator-facing cause; never sent to clients.
        diagnostic: String,
    },
    /// Internal or database error.
    #[error("internal error: {diagnostic}")]
    Internal {
        /// Operator-facing cause; never sent to clients.
        diagnostic: String,
    },
}

impl DomainError {
    /// An internal error with an operator-facing diagnostic.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
        }
    }

    /// Wraps a `toolkit-odata` error, keeping the query-error category.
    #[must_use]
    pub fn odata(err: toolkit_odata::Error) -> Self {
        Self::Odata(err)
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::odata(err)
    }
}
