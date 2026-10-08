//! Domain error type.
//!
//! One variant per row of the ADR-0004 error table (REST mapping lives in
//! `api::rest::error`). Messages here are for logs; REST clients only ever see
//! the canonical `Problem` built from the variant, so nothing in a payload
//! (provider text, driver messages, ids) reaches the wire except where the
//! variant is explicitly client-facing.

use toolkit_db::DbError;
use toolkit_db::outbox::OutboxError;
use toolkit_db::secure::{ScopeError, is_unique_violation};

use crate::domain::credits::CreditError;

/// The resource a `not_found` error names (`context.resource_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

impl ResourceKind {
    /// GTS type id of the resource, `gts.cf.core.mini_chat.<kind>.v1~`.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Chat => "gts.cf.core.mini_chat.chat.v1~",
            Self::Message => "gts.cf.core.mini_chat.message.v1~",
            Self::Turn => "gts.cf.core.mini_chat.turn.v1~",
            Self::Attachment => "gts.cf.core.mini_chat.attachment.v1~",
            Self::Model => "gts.cf.core.mini_chat.model.v1~",
        }
    }

    /// Lower-case singular name.
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
}

/// Which quota ran out (`context.violations[0].subject`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QuotaScope {
    Tokens,
    WebSearch,
    CodeInterpreter,
}

impl QuotaScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("{} not found", .resource.name())]
    NotFound { resource: ResourceKind },
    #[error("unknown or disabled model")]
    InvalidModel,
    #[error("message content is empty")]
    EmptyContent,
    #[error("invalid chat title")]
    InvalidTitle,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid, duplicate, foreign or not-ready attachment")]
    InvalidAttachment,
    #[error("too many images in one message")]
    TooManyImages,
    #[error("message exceeds the input token limit")]
    InputTooLong,
    #[error("mandatory context does not fit the budget")]
    ContextBudgetExceeded,
    #[error("the effective model does not support images")]
    VisionNotSupported,
    #[error("file exceeds the upload size limit")]
    FileTooLarge,
    #[error("unsupported content type")]
    UnsupportedContentType,
    #[error("code interpreter is unavailable")]
    CodeInterpreterUnavailable,
    #[error("invalid multipart upload: {reason} ({field})")]
    Multipart {
        reason: &'static str,
        field: &'static str,
    },
    /// Kill switch; `subject` is `web_search` or `images`.
    #[error("feature disabled: {subject}")]
    FeatureDisabled { subject: &'static str },
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are only allowed on assistant messages")]
    ReactionTargetNotAssistant,
    #[error("authorization denied")]
    AuthzDenied,
    #[error("authorization could not be evaluated")]
    AuthzUnavailable,
    #[error("caller is not the turn requester")]
    NotRequester,
    #[error("another turn is already running in this chat")]
    TurnAlreadyRunning,
    #[error("request id is reused for a non-completed or deleted turn")]
    RequestIdConflict,
    #[error("turn is not the latest turn")]
    NotLatestTurn,
    #[error("a generation is already in progress")]
    GenerationInProgress,
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("vector store was created for another provider backend")]
    ProviderMismatch,
    #[error("unique constraint violation")]
    UniqueViolation,
    #[error("quota exceeded: {}", .scope.as_str())]
    QuotaExceeded { scope: QuotaScope },
    #[error("per-chat document limit reached")]
    DocumentLimit,
    #[error("per-chat storage limit reached")]
    StorageLimit,
    #[error("storage backend unavailable")]
    StorageUnavailable,
    #[error("upload concurrency limit reached")]
    UploadConcurrency,
    /// The outbox payload exceeds the size limit. Chat delete reports it as a
    /// client error (400); everywhere else it is an internal error.
    #[error("outbox payload too large")]
    OutboxPayloadTooLarge { during_chat_delete: bool },
    /// Provider or policy resolution failed before streaming (logged only).
    #[error("provider resolution failed: {0}")]
    ProviderResolution(String),
    /// A completed turn's `request_id` was replayed (defensive 409).
    #[error("replay of a completed turn")]
    Replay,
    /// Bad `OData` list query (`$filter`, `$orderby`, cursor, ...) or a
    /// pagination failure; mapped by `toolkit-odata` (resource type
    /// `gts.cf.core.odata.query.v1~`).
    #[error("list query: {0}")]
    Query(QueryError),
    /// The policy plugin no longer has the snapshot of a requested policy
    /// version (permanent, unlike a transient plugin failure). Used by the
    /// orphan watchdog so a dropped version cannot keep a turn running
    /// forever; mapped like `Internal` (logged only).
    #[error("policy snapshot not found: {0}")]
    PolicySnapshotGone(String),
    /// Internal failure (logged only, never sent to clients).
    #[error("internal error: {0}")]
    Internal(String),
}

/// A `toolkit_odata::Error` with value equality (by message) so it can live
/// in [`DomainError`].
#[derive(Clone, Debug, thiserror::Error)]
#[error(transparent)]
pub struct QueryError(pub toolkit_odata::Error);

impl PartialEq for QueryError {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

impl Eq for QueryError {}

impl From<toolkit_odata::Error> for DomainError {
    fn from(e: toolkit_odata::Error) -> Self {
        Self::Query(QueryError(e))
    }
}

impl DomainError {
    /// The SSE `error` event code for a failure after the stream has opened
    /// (DESIGN section 3.3, "Streaming error codes"). Pre-stream failures are
    /// REST `Problem`s and never use this.
    ///
    /// A provider/policy resolution failure is `provider_error`; any other
    /// failure while streaming comes from persisting or finalizing the turn, so
    /// it is `finalization_failed`.
    #[must_use]
    pub const fn sse_code(&self) -> &'static str {
        match self {
            Self::ProviderResolution(_) => "provider_error",
            _ => "finalization_failed",
        }
    }
}

impl From<DbError> for DomainError {
    fn from(e: DbError) -> Self {
        Self::Internal(format!("database: {e}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        if is_unique_violation(&e) {
            Self::UniqueViolation
        } else {
            Self::Internal(format!("database: {e}"))
        }
    }
}

impl From<ScopeError> for DomainError {
    fn from(e: ScopeError) -> Self {
        if e.is_unique_violation() {
            Self::UniqueViolation
        } else {
            Self::Internal(format!("scoped query: {e}"))
        }
    }
}

impl From<OutboxError> for DomainError {
    fn from(e: OutboxError) -> Self {
        match e {
            OutboxError::PayloadTooLarge { .. } => Self::OutboxPayloadTooLarge {
                during_chat_delete: false,
            },
            other => Self::Internal(format!("outbox: {other}")),
        }
    }
}

impl From<CreditError> for DomainError {
    fn from(e: CreditError) -> Self {
        Self::Internal(format!("credit computation: {e}"))
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
