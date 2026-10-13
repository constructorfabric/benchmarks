//! Domain error taxonomy shared by every mini-chat feature.
//!
//! One enum covers every row of the mapping table of
//! `cpt-cf-mini-chat-algo-gear-foundation-map-error`. The mapping to the
//! canonical `Problem` lives in `api::rest::error_mapping`.

use thiserror::Error;

/// Resource kinds that can be reported as not found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// A chat.
    Chat,
    /// A message of a chat.
    Message,
    /// A turn of a chat.
    Turn,
    /// An attachment of a chat.
    Attachment,
    /// A catalog model.
    Model,
}

impl ResourceKind {
    /// GTS resource type id reported in `context.resource_type`.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Chat => "gts.cf.core.mini_chat.chat.v1~",
            Self::Message => "gts.cf.core.mini_chat.message.v1~",
            Self::Turn => "gts.cf.core.mini_chat.turn.v1~",
            Self::Attachment => "gts.cf.core.mini_chat.attachment.v1~",
            Self::Model => "gts.cf.core.mini_chat.model.v1~",
        }
    }
}

/// Multipart parsing failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartIssue {
    /// No multipart boundary in `Content-Type`.
    BoundaryRequired,
    /// The multipart body is malformed.
    MultipartError,
    /// No `file` part.
    MissingFile,
    /// The file part has no content type.
    MissingContentType,
}

/// Features a kill switch can disable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledFeature {
    /// Web search.
    WebSearch,
    /// Images.
    Images,
}

/// Quota dimensions that can be exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSubject {
    /// Token credits.
    Tokens,
    /// Web search calls.
    WebSearch,
    /// Code interpreter calls.
    CodeInterpreter,
}

/// Domain error of the gear.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// Resource not found (chat, message, turn, attachment, model).
    #[error("{resource:?} not found")]
    NotFound {
        /// The missing resource.
        resource: ResourceKind,
    },
    /// Invalid or disabled model, or chat model removed from the catalog.
    #[error("invalid model")]
    InvalidModel,
    /// Empty message content.
    #[error("empty content")]
    EmptyContent,
    /// Invalid chat title.
    #[error("invalid title")]
    InvalidTitle,
    /// Invalid reaction value.
    #[error("invalid reaction")]
    InvalidReaction,
    /// Bad `OData` query.
    #[error("invalid odata query: {0}")]
    InvalidQuery(toolkit_odata::Error),
    /// Unsupported upload MIME type.
    #[error("unsupported content type")]
    UnsupportedContentType,
    /// Code-interpreter-only upload unavailable.
    #[error("code interpreter unavailable")]
    CodeInterpreterUnavailable,
    /// Invalid multipart request.
    #[error("invalid multipart: {0:?}")]
    InvalidMultipart(MultipartIssue),
    /// Chat-cleanup payload too large on chat delete.
    #[error("cleanup payload too large: {detail}")]
    CleanupPayloadTooLarge {
        /// Safe detail mirrored in `context.format`.
        detail: String,
    },
    /// Vision not supported by the model.
    #[error("vision not supported")]
    VisionNotSupported,
    /// Invalid attachment ids.
    #[error("invalid attachment ids")]
    InvalidAttachmentIds,
    /// File too large.
    #[error("file too large")]
    FileTooLarge,
    /// Too many images.
    #[error("too many images")]
    TooManyImages,
    /// Input too long.
    #[error("input too long")]
    InputTooLong,
    /// Context budget exceeded.
    #[error("context budget exceeded")]
    ContextBudgetExceeded,
    /// Feature disabled by a kill switch.
    #[error("feature disabled: {0:?}")]
    FeatureDisabled(DisabledFeature),
    /// Turn is not in a terminal state.
    #[error("turn not terminal")]
    TurnNotTerminal,
    /// Reaction on a non-assistant message.
    #[error("reaction target is not an assistant message")]
    ReactionTargetNotAssistant,
    /// Authorization denied (PDP deny, compile failure, foreign turn requester).
    #[error("authorization denied")]
    AuthzDenied,
    /// PDP unreachable, timed out or failed.
    #[error("pdp unavailable")]
    PdpUnavailable,
    /// A turn is already running in the chat.
    #[error("turn already running")]
    TurnAlreadyRunning,
    /// Request id conflict.
    #[error("request id conflict")]
    RequestIdConflict,
    /// The turn is not the latest one.
    #[error("not latest turn")]
    NotLatestTurn,
    /// Generation in progress.
    #[error("generation in progress")]
    GenerationInProgress,
    /// Replay of a completed request.
    #[error("replay")]
    Replay,
    /// Attachment locked.
    #[error("attachment locked")]
    AttachmentLocked,
    /// Provider mismatch.
    #[error("provider mismatch")]
    ProviderMismatch,
    /// Other unique violation.
    #[error("unique violation")]
    UniqueViolation,
    /// Quota exhausted.
    #[error("quota exceeded: {0:?}")]
    QuotaExceeded(QuotaSubject),
    /// Per-chat document limit.
    #[error("document limit")]
    DocumentLimit,
    /// Per-chat storage limit.
    #[error("storage limit")]
    StorageLimit,
    /// Storage backend failure.
    #[error("storage unavailable")]
    StorageUnavailable,
    /// Upload concurrency limit.
    #[error("upload concurrency limit")]
    UploadConcurrencyLimit,
    /// Provider resolution failure (diagnostic is logged, never sent).
    #[error("provider resolution failed: {0}")]
    ProviderResolution(String),
    /// Policy plugin resolution or call failure (diagnostic is logged, never sent).
    #[error("policy resolution failed: {0}")]
    PolicyResolution(String),
    /// Database error (diagnostic is logged, never sent).
    #[error("database error: {0}")]
    Database(String),
    /// Any other internal error (diagnostic is logged, never sent).
    #[error("internal error: {0}")]
    Internal(String),
}
