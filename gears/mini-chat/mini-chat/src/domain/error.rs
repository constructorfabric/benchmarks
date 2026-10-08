//! Domain error type and its mapping to the canonical REST error contract (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, ResourceErrorBuilder};
use toolkit_db::secure::ScopeError;

/// GTS resource types reported in `context.resource_type`.
pub mod resource_types {
    pub const CHAT: &str = "gts.cf.core.mini_chat.chat.v1~";
    pub const MESSAGE: &str = "gts.cf.core.mini_chat.message.v1~";
    pub const TURN: &str = "gts.cf.core.mini_chat.turn.v1~";
    pub const ATTACHMENT: &str = "gts.cf.core.mini_chat.attachment.v1~";
    pub const MODEL: &str = "gts.cf.core.mini_chat.model.v1~";
}

/// Machine-readable reasons (ADR-0004 / DESIGN §3.3).
pub mod reasons {
    pub const INVALID_MODEL: &str = "INVALID_MODEL";
    pub const EMPTY_CONTENT: &str = "EMPTY_CONTENT";
    pub const INVALID_TITLE: &str = "INVALID_TITLE";
    pub const INVALID_REACTION: &str = "INVALID_REACTION";
    pub const INVALID_ATTACHMENT: &str = "invalid_attachment";
    pub const UNSUPPORTED_CONTENT_TYPE: &str = "UNSUPPORTED_CONTENT_TYPE";
    pub const CODE_INTERPRETER_UNAVAILABLE: &str = "CODE_INTERPRETER_UNAVAILABLE";
    pub const BOUNDARY_REQUIRED: &str = "BOUNDARY_REQUIRED";
    pub const MULTIPART_ERROR: &str = "MULTIPART_ERROR";
    pub const MISSING_FILE: &str = "MISSING_FILE";
    pub const MISSING_CONTENT_TYPE: &str = "MISSING_CONTENT_TYPE";
    pub const VISION_NOT_SUPPORTED: &str = "VISION_NOT_SUPPORTED";
    pub const FILE_TOO_LARGE: &str = "FILE_TOO_LARGE";
    pub const TOO_MANY_IMAGES: &str = "TOO_MANY_IMAGES";
    pub const INPUT_TOO_LONG: &str = "INPUT_TOO_LONG";
    pub const CONTEXT_BUDGET_EXCEEDED: &str = "CONTEXT_BUDGET_EXCEEDED";
    pub const FEATURE_DISABLED: &str = "FEATURE_DISABLED";
    pub const STATE: &str = "STATE";
    pub const AUTHZ_DENIED: &str = "AUTHZ_DENIED";
    pub const TURN_ALREADY_RUNNING: &str = "turn_already_running";
    pub const REQUEST_ID_CONFLICT: &str = "request_id_conflict";
    pub const NOT_LATEST_TURN: &str = "NOT_LATEST_TURN";
    pub const GENERATION_IN_PROGRESS: &str = "GENERATION_IN_PROGRESS";
    pub const REPLAY: &str = "REPLAY";
    pub const ATTACHMENT_LOCKED: &str = "attachment_locked";
    pub const PROVIDER_MISMATCH: &str = "provider_mismatch";
    pub const UNIQUE_VIOLATION: &str = "unique_violation";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    pub const DOCUMENT_LIMIT: &str = "document_limit";
    pub const STORAGE_LIMIT: &str = "storage_limit";
}

/// Streaming (SSE `event: error`) codes and turn `error_code` values.
pub mod stream_codes {
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const PROVIDER_TIMEOUT: &str = "provider_timeout";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
    pub const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
    pub const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
    pub const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
    pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
    pub const FINALIZATION_FAILED: &str = "finalization_failed";
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
    pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";
    pub const TURN_SETUP_FAILED: &str = "turn_setup_failed";
    pub const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
}

/// Domain error. Converted to a canonical `Problem` at the REST boundary.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// 404 with `context.resource_type = resource`.
    #[error("{resource} not found")]
    NotFound { resource: &'static str },
    /// 400 `invalid_argument` with one field violation.
    #[error("invalid argument {field}: {reason}")]
    InvalidArgument {
        resource: &'static str,
        field: String,
        reason: String,
        description: String,
    },
    /// 400 `invalid_argument` with `context.format`.
    #[error("invalid argument: {message}")]
    InvalidFormat {
        resource: &'static str,
        message: String,
    },
    /// 400 `out_of_range` with one field violation.
    #[error("out of range {field}: {reason}")]
    OutOfRange {
        resource: &'static str,
        field: String,
        reason: String,
        description: String,
    },
    /// 400 `failed_precondition` with one violation `{subject, description, type}`.
    #[error("failed precondition {subject}: {kind}")]
    FailedPrecondition {
        resource: &'static str,
        subject: String,
        kind: String,
        description: String,
    },
    /// 409 `aborted` with `context.reason`.
    #[error("aborted: {reason}")]
    Aborted {
        resource: &'static str,
        reason: String,
        detail: String,
    },
    /// 409 `already_exists` with `context.resource_name = name`.
    #[error("already exists: {name}")]
    AlreadyExists {
        resource: &'static str,
        name: String,
        detail: String,
    },
    /// 403 `permission_denied` with `context.reason`.
    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },
    /// 429 `resource_exhausted` with `violations[{subject, description}]`.
    #[error("resource exhausted: {subject}")]
    ResourceExhausted {
        resource: &'static str,
        subject: String,
        description: String,
        detail: String,
    },
    /// 503 with `Retry-After`.
    #[error("service unavailable: {detail}")]
    ServiceUnavailable { retry_after_secs: u64, detail: String },
    /// 500; the diagnostic is logged, never sent.
    #[error("internal error: {0}")]
    Internal(String),
    /// OData query error (keeps the platform reason codes).
    #[error("odata error: {0}")]
    OData(toolkit_odata::Error),
}

impl DomainError {
    #[must_use]
    pub fn internal(diag: impl std::fmt::Display) -> Self {
        Self::Internal(diag.to_string())
    }

    #[must_use]
    pub const fn chat_not_found() -> Self {
        Self::NotFound { resource: resource_types::CHAT }
    }

    #[must_use]
    pub fn invalid(
        resource: &'static str,
        field: &str,
        reason: &str,
        description: impl Into<String>,
    ) -> Self {
        Self::InvalidArgument {
            resource,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn out_of_range(
        resource: &'static str,
        field: &str,
        reason: &str,
        description: impl Into<String>,
    ) -> Self {
        Self::OutOfRange {
            resource,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn precondition(subject: &str, kind: &str, description: impl Into<String>) -> Self {
        Self::FailedPrecondition {
            resource: resource_types::CHAT,
            subject: subject.to_owned(),
            kind: kind.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn feature_disabled(subject: &str) -> Self {
        Self::precondition(subject, reasons::FEATURE_DISABLED, format!("{subject} is disabled"))
    }

    #[must_use]
    pub fn aborted(reason: &str, detail: impl Into<String>) -> Self {
        Self::Aborted {
            resource: resource_types::CHAT,
            reason: reason.to_owned(),
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn quota_exceeded(subject: &str) -> Self {
        Self::ResourceExhausted {
            resource: resource_types::CHAT,
            subject: subject.to_owned(),
            description: reasons::QUOTA_EXCEEDED.to_owned(),
            detail: format!("Quota exceeded ({subject})"),
        }
    }

    #[must_use]
    pub fn authz_denied() -> Self {
        Self::PermissionDenied { reason: reasons::AUTHZ_DENIED.to_owned() }
    }

    #[must_use]
    pub fn invalid_model() -> Self {
        Self::invalid(
            resource_types::CHAT,
            "model",
            reasons::INVALID_MODEL,
            "Model is not available in the catalog",
        )
    }

    /// True when this is a unique-constraint violation reported by the DB layer.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::AlreadyExists { name, .. } if name == reasons::UNIQUE_VIOLATION)
    }
}

fn unique_violation() -> DomainError {
    DomainError::AlreadyExists {
        resource: resource_types::CHAT,
        name: reasons::UNIQUE_VIOLATION.to_owned(),
        detail: "Resource already exists".to_owned(),
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        if let toolkit_db::DbError::Sea(db) = &err
            && toolkit_db::secure::is_unique_violation(db)
        {
            return unique_violation();
        }
        Self::Internal(format!("database error: {err}"))
    }
}

impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        if err.is_unique_violation() {
            return unique_violation();
        }
        Self::Internal(format!("database error: {err}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(err: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&err) {
            return unique_violation();
        }
        Self::Internal(format!("database error: {err}"))
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::OData(err)
    }
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound { resource } => ResourceErrorBuilder::__not_found(
                resource,
                format!("{} not found", short_name(resource)),
            )
            .with_resource("")
            .create(),
            DomainError::InvalidArgument {
                resource,
                field,
                reason,
                description,
            } => ResourceErrorBuilder::__invalid_argument(resource, description.clone())
                .with_field_violation(field, description, reason)
                .create(),
            DomainError::InvalidFormat { resource, message } => {
                ResourceErrorBuilder::__invalid_argument(resource, message.clone())
                    .with_format(message)
                    .create()
            }
            DomainError::OutOfRange {
                resource,
                field,
                reason,
                description,
            } => ResourceErrorBuilder::__out_of_range(resource, description.clone())
                .with_field_violation(field, description, reason)
                .create(),
            DomainError::FailedPrecondition {
                resource,
                subject,
                kind,
                description,
            } => ResourceErrorBuilder::__failed_precondition(resource, description.clone())
                .with_precondition_violation(subject, description, kind)
                .create(),
            DomainError::Aborted {
                resource,
                reason,
                detail,
            } => ResourceErrorBuilder::__aborted(resource, detail)
                .with_reason(reason)
                .create(),
            DomainError::AlreadyExists {
                resource,
                name,
                detail,
            } => ResourceErrorBuilder::__already_exists(resource, detail)
                .with_resource(name)
                .create(),
            DomainError::PermissionDenied { reason } => {
                ResourceErrorBuilder::__permission_denied(resource_types::CHAT, "Access denied")
                    .with_reason(reason)
                    .create()
            }
            DomainError::ResourceExhausted {
                resource,
                subject,
                description,
                detail,
            } => ResourceErrorBuilder::__resource_exhausted(resource, detail)
                .with_quota_violation(subject, description)
                .create(),
            DomainError::ServiceUnavailable {
                retry_after_secs,
                detail,
            } => CanonicalError::service_unavailable()
                .with_retry_after_seconds(retry_after_secs)
                .with_detail(detail)
                .create(),
            DomainError::Internal(diag) => {
                tracing::error!(error = %diag, "mini-chat internal error");
                CanonicalError::internal(diag).create()
            }
            DomainError::OData(e) => CanonicalError::from(e),
        }
    }
}

fn short_name(resource: &str) -> &'static str {
    match resource {
        resource_types::CHAT => "Chat",
        resource_types::MESSAGE => "Message",
        resource_types::TURN => "Turn",
        resource_types::ATTACHMENT => "Attachment",
        resource_types::MODEL => "Model",
        _ => "Resource",
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
