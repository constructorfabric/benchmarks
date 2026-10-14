//! Domain error taxonomy.
//!
//! Domain failures keep their operational causes; the boundary adapter in
//! `api::rest::error` projects only curated canonical errors (no provider
//! identifiers, no driver text, no top-level `code`).

use thiserror::Error;
use toolkit_macros::domain_model;

/// The resource kinds a `not_found` can name (`context.resource_type`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

/// The call site of an outbox enqueue, which decides how a payload-size failure is
/// reported: chat deletion maps to 400 `invalid_argument`, every other site to 500.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueSite {
    ChatDelete,
    AttachmentDelete,
    TurnRetry,
    TurnEdit,
    TurnDelete,
    /// The usage message enqueued by the turn settlement transaction.
    TurnFinalize,
}

/// The quota scope of a 429 `resource_exhausted` (`context.violations[0].subject`): clients
/// branch on it, never on the detail. The per-user daily image quota and `image_bytes_exceeded`
/// are not implemented and have no scope (ADR-0008).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    /// Cascade reject, no enabled model in any tier, uncomputable reserve, reserve re-check.
    Tokens,
    /// The daily `web_search_calls` reached `quota.web_search_daily_quota`.
    WebSearch,
    /// The daily `code_interpreter_calls` reached `quota.code_interpreter_daily_quota`.
    CodeInterpreter,
}

impl QuotaScope {
    /// The wire value: `tokens`, `web_search` or `code_interpreter`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

/// The reason of a 409 `aborted` on `messages:stream` (`context.reason`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The `request_id` belongs to a `running`, `failed` or `cancelled` turn, or to a
    /// soft-deleted turn of any state.
    RequestIdConflict,
    /// Another turn of the chat is `running` (also the running-index insert race).
    TurnAlreadyRunning,
    /// A turn mutation targets a turn that is not the latest non-deleted turn of the chat
    /// (including a soft-deleted one, and one behind a newer `running` turn).
    NotLatestTurn,
    /// A turn mutation lost the insert race on the running-turn unique index.
    GenerationInProgress,
}

impl ConflictReason {
    /// The wire value: `request_id_conflict`, `turn_already_running`, `NOT_LATEST_TURN` or
    /// `GENERATION_IN_PROGRESS`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestIdConflict => "request_id_conflict",
            Self::TurnAlreadyRunning => "turn_already_running",
            Self::NotLatestTurn => "NOT_LATEST_TURN",
            Self::GenerationInProgress => "GENERATION_IN_PROGRESS",
        }
    }
}

/// The attachment-route failures of FEATURE 0005 that have no other home in [`DomainError`]
/// (`cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract`). `INVALID_MODEL`,
/// `FEATURE_DISABLED` for images, the attachment `NotFound` and the chat `NotFound` reuse the
/// existing variants.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentError {
    /// 400 `invalid_argument`, `field_violations[content_type].reason = BOUNDARY_REQUIRED`.
    BoundaryRequired,
    /// 400 `invalid_argument`, `field_violations[multipart].reason = MULTIPART_ERROR`.
    MultipartError,
    /// 400 `invalid_argument`, `field_violations[file].reason = MISSING_FILE`.
    MissingFile,
    /// 400 `invalid_argument`, `field_violations[content_type].reason = MISSING_CONTENT_TYPE`.
    MissingContentType,
    /// 400 `invalid_argument`, `field_violations[content_type].reason = UNSUPPORTED_CONTENT_TYPE`.
    UnsupportedContentType,
    /// 400 `invalid_argument`, `field_violations[file].reason = CODE_INTERPRETER_UNAVAILABLE`,
    /// attachment `resource_type`.
    CodeInterpreterUnavailable,
    /// 400 `out_of_range`, `field_violations[content_length].reason = FILE_TOO_LARGE`.
    FileTooLarge,
    /// 429 `resource_exhausted`, `violations[{subject: document_limit}]`.
    DocumentLimit,
    /// 429 `resource_exhausted`, `violations[{subject: storage_limit}]`.
    StorageLimit,
    /// 409 `already_exists`, `resource_name = provider_mismatch`.
    ProviderMismatch,
    /// 503 `service_unavailable`, `Retry-After: 10`, generic detail: a provider Files or vector
    /// store failure, or an indexing failure within the deadline.
    StorageUnavailable,
    /// 503 `service_unavailable`, `Retry-After: 5`: the per-process upload concurrency limit.
    UploadBusy,
    /// 409 `already_exists`, `resource_name = attachment_locked`: a submitted message references
    /// the attachment that a `DELETE` targets.
    Locked,
}

/// Domain-level error.
#[domain_model]
#[derive(Debug, Error)]
pub enum DomainError {
    /// A hidden, missing, soft-deleted or foreign resource.
    #[error("{resource:?} not found")]
    NotFound {
        resource: ResourceKind,
        id: Option<String>,
    },
    /// Unknown or disabled model on chat creation: `field_violations[model] = INVALID_MODEL`.
    #[error("invalid model")]
    InvalidModel { model: String },
    /// A chat title that is empty or whitespace-only after trimming, or longer than 255
    /// characters: `field_violations[title] = INVALID_TITLE`.
    #[error("invalid title")]
    InvalidTitle,
    /// A list query rejected while paginating (cursor and filter or order mismatch, unknown
    /// filter field, operator outside the field's set, ...): `context.resource_type` is the
    /// platform `OData` query type.
    #[error("invalid list query: {0}")]
    ODataQuery(toolkit_odata::Error),
    /// PDP deny, a decision without usable constraints, or a constraint compile failure.
    #[error("authorization denied")]
    AuthzDenied,
    /// The PDP could not evaluate the request (unreachable, timeout, evaluation error).
    #[error("authorization unavailable")]
    AuthzUnavailable { cause: String },
    /// The model-policy plugin cannot be resolved or failed.
    #[error("model policy unavailable")]
    PolicyUnavailable { cause: String },
    /// An unhandled unique-constraint violation; `code` is the conflict code.
    #[error("unique violation: {code}")]
    UniqueViolation { code: String },
    /// An outbox payload exceeds the size limit.
    #[error("outbox payload too large")]
    OutboxPayloadTooLarge { site: EnqueueSite },
    /// A quota rejection before any reserve: 429 `resource_exhausted` with the scope as the
    /// violation subject (the reserve re-check rejection uses [`QuotaScope::Tokens`] too).
    #[error("quota exceeded: {}", .scope.as_str())]
    QuotaExceeded { scope: QuotaScope },
    /// `web_search.enabled` while the `disable_web_search` kill switch is on: 400
    /// `failed_precondition`, `violations[{subject, type: FEATURE_DISABLED}]`.
    #[error("feature disabled: {subject}")]
    FeatureDisabled { subject: &'static str },
    /// Empty or whitespace-only `content` on `messages:stream`: 400 `invalid_argument`,
    /// `field_violations[content].reason = EMPTY_CONTENT`.
    #[error("empty content")]
    EmptyContent,
    /// Duplicate, too many, unknown, foreign, deleted or not-ready `attachment_ids`: 400
    /// `invalid_argument`, `field_violations[attachment].reason = invalid_attachment`.
    #[error("invalid attachment")]
    InvalidAttachment,
    /// More image attachments than `rag.max_images_per_message`: 400 `out_of_range`,
    /// `field_violations[image_count].reason = TOO_MANY_IMAGES`.
    #[error("too many images")]
    TooManyImages,
    /// Images on an effective model without `VISION_INPUT`: 400 `invalid_argument`,
    /// `field_violations[content_type].reason = VISION_NOT_SUPPORTED`.
    #[error("vision input is not supported")]
    VisionNotSupported,
    /// The message exceeds `max_input_tokens` of the effective model: 400 `out_of_range`,
    /// `field_violations[content].reason = INPUT_TOO_LONG`.
    #[error("input too long")]
    InputTooLong,
    /// The mandatory context does not fit the budget: 400 `out_of_range`,
    /// `field_violations[context].reason = CONTEXT_BUDGET_EXCEEDED`.
    #[error("context budget exceeded")]
    ContextBudgetExceeded,
    /// A retry, edit or delete of a turn that is not in a terminal state (`running`): 400
    /// `failed_precondition`, `violations[{subject: turn_state, type: STATE}]`.
    #[error("the turn is not terminal")]
    TurnNotTerminal,
    /// A reaction value other than `like` or `dislike`: 400 `invalid_argument`,
    /// `field_violations[reaction].reason = INVALID_REACTION`.
    #[error("invalid reaction")]
    InvalidReaction,
    /// A reaction on a user or system message: 400 `failed_precondition`,
    /// `violations[{subject: reaction_target, type: STATE}]`.
    #[error("the reaction target is not an assistant message")]
    ReactionTargetNotAssistant,
    /// A turn admission conflict or an insert race: 409 `aborted` with `context.reason`.
    #[error("conflict: {}", .reason.as_str())]
    Conflict { reason: ConflictReason },
    /// An attachment-route failure of FEATURE 0005.
    #[error("attachment failure: {0:?}")]
    Attachment(AttachmentError),
    /// Any other internal or database failure; the diagnostic is logged, never returned.
    #[error("internal error")]
    Internal { diagnostic: String },
}

impl DomainError {
    /// Shorthand for [`DomainError::Internal`].
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
        }
    }
}

/// Codes of the SSE `event: error` payload (`{code, message}`), DESIGN 3.3
/// `Streaming error codes`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorCode {
    ProviderError,
    ProviderTimeout,
    RateLimited,
    WebSearchCallsExceeded,
    CodeInterpreterCallsExceeded,
    AgenticIterationsExceeded,
    UnexpectedToolUse,
    MessagePersistenceFailed,
    FinalizationFailed,
    StreamInterrupted,
}

impl StreamErrorCode {
    /// Every streaming error code.
    pub const ALL: [Self; 10] = [
        Self::ProviderError,
        Self::ProviderTimeout,
        Self::RateLimited,
        Self::WebSearchCallsExceeded,
        Self::CodeInterpreterCallsExceeded,
        Self::AgenticIterationsExceeded,
        Self::UnexpectedToolUse,
        Self::MessagePersistenceFailed,
        Self::FinalizationFailed,
        Self::StreamInterrupted,
    ];

    /// The wire value, for example `provider_error`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
            Self::WebSearchCallsExceeded => "web_search_calls_exceeded",
            Self::CodeInterpreterCallsExceeded => "code_interpreter_calls_exceeded",
            Self::AgenticIterationsExceeded => "agentic_iterations_exceeded",
            Self::UnexpectedToolUse => "unexpected_tool_use",
            Self::MessagePersistenceFailed => "message_persistence_failed",
            Self::FinalizationFailed => "finalization_failed",
            Self::StreamInterrupted => "stream_interrupted",
        }
    }
}
