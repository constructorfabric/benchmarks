//! Domain error type and its canonical mapping (ADR-0004, DESIGN 3.3 "REST
//! error mapping").
//!
//! Services raise [`DomainError`]; the conversion into
//! [`CanonicalError`] picks the category, status and the machine-readable
//! fields. The `Display` of a variant never carries a cause, so a logged or
//! serialized error cannot leak driver, PDP or plugin text; causes are only
//! reachable through [`std::error::Error::source`] for logging.

use thiserror::Error;
use toolkit_canonical_errors::{CanonicalError, resource_error};

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Conflict code of an unhandled unique-constraint violation.
pub const UNIQUE_VIOLATION: &str = "unique_violation";

/// Resource kinds that can be reported as not found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// A chat.
    Chat,
    /// A message.
    Message,
    /// A turn.
    Turn,
    /// An attachment.
    Attachment,
    /// A catalog model.
    Model,
}

/// Errors raised by the domain services.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// The resource does not exist or is not visible to the caller.
    #[error("{resource:?} not found")]
    NotFound {
        /// Kind of the missing resource.
        resource: ResourceKind,
        /// Identifier as given by the caller.
        id: String,
    },
    /// Unknown or disabled model.
    #[error("invalid model")]
    InvalidModel,
    /// Invalid chat title.
    #[error("invalid title: {detail}")]
    InvalidTitle {
        /// Human readable reason.
        detail: String,
    },
    /// Empty or whitespace-only message content.
    #[error("empty content")]
    EmptyContent,
    /// Reaction other than `like` / `dislike`.
    #[error("invalid reaction")]
    InvalidReaction,
    /// Invalid, duplicate, foreign or not-ready attachment reference.
    #[error("invalid attachment")]
    InvalidAttachment,
    /// Field level validation error with a caller supplied reason code.
    #[error("invalid input: {field}")]
    InvalidInput {
        /// Resource the violation is reported against (`context.resource_type`).
        resource: ResourceKind,
        /// Offending field.
        field: &'static str,
        /// Machine readable reason.
        reason: &'static str,
        /// Human readable detail.
        detail: String,
    },
    /// Value out of the accepted range.
    #[error("out of range: {field}")]
    OutOfRange {
        /// Resource the violation is reported against (`context.resource_type`).
        resource: ResourceKind,
        /// Offending field.
        field: &'static str,
        /// Machine readable reason.
        reason: &'static str,
        /// Human readable detail.
        detail: String,
    },
    /// A kill switch disabled the requested feature.
    #[error("feature disabled: {feature}")]
    FeatureDisabled {
        /// Disabled feature (`web_search`, `images`, ...).
        feature: &'static str,
    },
    /// The turn is not in a state that allows the operation.
    #[error("invalid turn state")]
    InvalidTurnState,
    /// Reaction on a non-assistant message.
    #[error("invalid reaction target")]
    InvalidReactionTarget,
    /// The PDP denied the request (fail-closed).
    #[error("access denied")]
    AccessDenied {
        /// Underlying cause, logged only.
        #[source]
        cause: Option<BoxError>,
    },
    /// A dependency is unavailable (for example the PDP could not evaluate).
    #[error("service unavailable: {detail}")]
    ServiceUnavailable {
        /// Generic human readable detail.
        detail: String,
        /// `Retry-After` seconds, when known.
        retry_after_secs: Option<u64>,
        /// Underlying cause, logged only.
        #[source]
        cause: Option<BoxError>,
    },
    /// Another turn is running in the chat.
    #[error("turn already running")]
    TurnAlreadyRunning,
    /// `request_id` reused for a non-completed or deleted turn.
    #[error("request id conflict")]
    RequestIdConflict,
    /// Mutation of a turn that is not the latest one.
    #[error("not latest turn")]
    NotLatestTurn,
    /// A concurrent mutation lost the running-turn race.
    #[error("generation in progress")]
    GenerationInProgress,
    /// Defensive fallback: the turn is completed and must be replayed.
    #[error("replay")]
    Replay,
    /// The attachment is referenced by a message.
    #[error("attachment locked")]
    AttachmentLocked,
    /// The chat's vector store belongs to another provider backend.
    #[error("provider mismatch")]
    ProviderMismatch,
    /// Unhandled unique-constraint violation.
    #[error("conflict: {code}")]
    Conflict {
        /// Conflict code reported in `resource_name`.
        code: &'static str,
    },
    /// Quota exhausted.
    #[error("quota exceeded: {scope}")]
    QuotaExceeded {
        /// Quota scope (`tokens`, `web_search`, `code_interpreter`).
        scope: &'static str,
    },
    /// Per-chat limit reached (`document_limit`, `storage_limit`).
    #[error("limit exceeded: {limit}")]
    LimitExceeded {
        /// Limit name.
        limit: &'static str,
    },
    /// The chat-cleanup outbox payload exceeds the outbox size limit
    /// (`DELETE /chats/{id}` only; other outbox writers report `Internal`).
    #[error("outbox payload too large")]
    OutboxPayloadTooLarge {
        /// Human readable message, also reported in `context.format`.
        message: String,
    },
    /// Invalid `OData` query (filter, order, cursor, page size) rejected by
    /// the platform `OData` layer while paginating.
    #[error("invalid odata query")]
    OData(#[from] toolkit_odata::Error),
    /// Internal failure; the wire detail is always generic.
    #[error("internal error")]
    Internal {
        /// Diagnostic for logs, never serialized.
        diagnostic: String,
        /// Underlying cause, logged only.
        #[source]
        cause: Option<BoxError>,
    },
}

impl DomainError {
    /// An internal error with only a diagnostic.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
            cause: None,
        }
    }
}

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

/// Logs the cause of an error whose wire form is deliberately generic.
fn log_cause(what: &str, cause: Option<&(dyn std::error::Error + Send + Sync + 'static)>) {
    if let Some(cause) = cause {
        tracing::warn!(error = %cause, "{what}: cause");
    }
}

fn not_found(resource: ResourceKind, id: String) -> CanonicalError {
    match resource {
        ResourceKind::Chat => ChatResource::not_found("chat not found")
            .with_resource(id)
            .create(),
        ResourceKind::Message => MessageResource::not_found("message not found")
            .with_resource(id)
            .create(),
        ResourceKind::Turn => TurnResource::not_found("turn not found")
            .with_resource(id)
            .create(),
        ResourceKind::Attachment => AttachmentResource::not_found("attachment not found")
            .with_resource(id)
            .create(),
        ResourceKind::Model => ModelResource::not_found("model not found")
            .with_resource(id)
            .create(),
    }
}

fn invalid_input(
    resource: ResourceKind,
    field: &'static str,
    reason: &'static str,
    detail: String,
) -> CanonicalError {
    match resource {
        ResourceKind::Chat => ChatResource::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Message => MessageResource::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Turn => TurnResource::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Attachment => AttachmentResource::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Model => ModelResource::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create(),
    }
}

fn out_of_range(
    resource: ResourceKind,
    field: &'static str,
    reason: &'static str,
    detail: String,
) -> CanonicalError {
    match resource {
        ResourceKind::Chat => ChatResource::out_of_range(detail.clone())
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Message => MessageResource::out_of_range(detail.clone())
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Turn => TurnResource::out_of_range(detail.clone())
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Attachment => AttachmentResource::out_of_range(detail.clone())
            .with_field_violation(field, detail, reason)
            .create(),
        ResourceKind::Model => ModelResource::out_of_range(detail.clone())
            .with_field_violation(field, detail, reason)
            .create(),
    }
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound { resource, id } => not_found(resource, id),
            DomainError::InvalidModel => ChatResource::invalid_argument()
                .with_field_violation("model", "unknown or disabled model", "INVALID_MODEL")
                .create(),
            DomainError::InvalidTitle { detail } => ChatResource::invalid_argument()
                .with_field_violation("title", detail, "INVALID_TITLE")
                .create(),
            DomainError::EmptyContent => ChatResource::invalid_argument()
                .with_field_violation("content", "content is empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation("reaction", "must be like or dislike", "INVALID_REACTION")
                .create(),
            DomainError::InvalidAttachment => ChatResource::invalid_argument()
                .with_field_violation("attachment", "invalid attachment", "invalid_attachment")
                .create(),
            DomainError::InvalidInput {
                resource,
                field,
                reason,
                detail,
            } => invalid_input(resource, field, reason, detail),
            DomainError::OutOfRange {
                resource,
                field,
                reason,
                detail,
            } => out_of_range(resource, field, reason, detail),
            DomainError::FeatureDisabled { feature } => ChatResource::failed_precondition()
                .with_precondition_violation(feature, "feature disabled", "FEATURE_DISABLED")
                .create(),
            DomainError::InvalidTurnState => TurnResource::failed_precondition()
                .with_precondition_violation("turn_state", "turn is not terminal", "STATE")
                .create(),
            DomainError::InvalidReactionTarget => MessageResource::failed_precondition()
                .with_precondition_violation("reaction_target", "not an assistant message", "STATE")
                .create(),
            DomainError::AccessDenied { cause } => {
                log_cause("access denied", cause.as_deref());
                ChatResource::permission_denied()
                    .with_reason("AUTHZ_DENIED")
                    .create()
            }
            DomainError::ServiceUnavailable {
                detail,
                retry_after_secs,
                cause,
            } => {
                log_cause("service unavailable", cause.as_deref());
                let builder = CanonicalError::service_unavailable().with_detail(detail);
                match retry_after_secs {
                    Some(secs) => builder.with_retry_after_seconds(secs).create(),
                    None => builder.create(),
                }
            }
            DomainError::TurnAlreadyRunning => TurnResource::aborted("a turn is already running")
                .with_reason("turn_already_running")
                .create(),
            DomainError::RequestIdConflict => TurnResource::aborted("request id conflict")
                .with_reason("request_id_conflict")
                .create(),
            DomainError::NotLatestTurn => TurnResource::aborted("not the latest turn")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => TurnResource::aborted("generation in progress")
                .with_reason("GENERATION_IN_PROGRESS")
                .create(),
            DomainError::Replay => TurnResource::aborted("turn already completed")
                .with_reason("REPLAY")
                .create(),
            DomainError::AttachmentLocked => {
                AttachmentResource::already_exists("attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => {
                AttachmentResource::already_exists("vector store provider mismatch")
                    .with_resource("provider_mismatch")
                    .create()
            }
            DomainError::Conflict { code } => ChatResource::already_exists("conflict")
                .with_resource(code)
                .create(),
            DomainError::QuotaExceeded { scope } => {
                ChatResource::resource_exhausted("quota exceeded")
                    .with_quota_violation(scope, "quota_exceeded")
                    .create()
            }
            DomainError::LimitExceeded { limit } => {
                AttachmentResource::resource_exhausted("limit exceeded")
                    .with_quota_violation(limit, "limit_exceeded")
                    .create()
            }
            DomainError::OutboxPayloadTooLarge { message } => ChatResource::invalid_argument()
                .with_format(message)
                .create(),
            DomainError::OData(err) => CanonicalError::from(err),
            DomainError::Internal { diagnostic, cause } => {
                tracing::error!(diagnostic = %diagnostic, "internal error");
                log_cause("internal error", cause.as_deref());
                CanonicalError::internal(diagnostic).create()
            }
        }
    }
}
