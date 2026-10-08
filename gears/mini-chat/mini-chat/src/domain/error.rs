//! Domain errors and their mapping to the canonical error contract
//! (ADR-0004): REST errors are RFC 9457 `Problem`s; the category decides the
//! HTTP status and the machine-readable reason lives in `context`.

use toolkit_canonical_errors::{CanonicalError, resource_error};

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatError;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageError;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnError;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentError;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelError;

#[resource_error(gts_id!("cf.core.mini_chat.user_quota.v1~"))]
pub struct QuotaError;

/// Resource a resource-scoped error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Res {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
    Quota,
}

/// Quota scope of a 429 (`context.violations[0].subject`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Domain error of the mini-chat gear.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("{0:?} not found: {1}")]
    NotFound(Res, String),

    /// `invalid_argument` with one field violation.
    #[error("invalid argument {field}: {reason} ({description})")]
    InvalidArgument {
        res: Res,
        field: &'static str,
        reason: &'static str,
        description: String,
    },

    /// `out_of_range` with one field violation.
    #[error("out of range {field}: {reason} ({description})")]
    OutOfRange {
        res: Res,
        field: &'static str,
        reason: &'static str,
        description: String,
    },

    /// `failed_precondition` with one violation.
    #[error("failed precondition {subject}/{kind}: {description}")]
    FailedPrecondition {
        res: Res,
        subject: &'static str,
        kind: &'static str,
        description: String,
    },

    /// `aborted` with a reason (409).
    #[error("aborted ({reason}): {detail}")]
    Aborted {
        res: Res,
        reason: &'static str,
        detail: String,
    },

    /// `already_exists` with a resource name (409).
    #[error("already exists ({name}): {detail}")]
    AlreadyExists {
        res: Res,
        name: String,
        detail: String,
    },

    /// `resource_exhausted` (429), quota scopes.
    #[error("quota exceeded ({})", .0.as_str())]
    QuotaExceeded(QuotaScope),

    /// `resource_exhausted` (429), per-chat limit (`document_limit` / `storage_limit`).
    #[error("per-chat limit exceeded ({subject})")]
    ChatLimit {
        subject: &'static str,
        description: String,
    },

    /// PDP denied or constraints could not be compiled (403).
    #[error("authorization denied")]
    AuthzDenied,

    /// Retry/edit/delete of a turn requested by another user (403).
    #[error("permission denied")]
    PermissionDenied,

    /// PDP could not evaluate the request (503, Retry-After 5).
    #[error("authorization unavailable: {0}")]
    AuthzUnavailable(String),

    /// Service unavailable with a retry hint.
    #[error("service unavailable ({retry_after_secs}s): {diagnostic}")]
    Unavailable {
        retry_after_secs: u64,
        diagnostic: String,
    },

    /// `invalid_argument` with a format message (outbox payload too large on
    /// chat delete).
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// Transient write contention (SQLite busy / PG serialization failure);
    /// callers retry the transaction.
    #[error("database contention: {0}")]
    Contention(String),

    /// `OData` query error (`invalid_argument`, resource `gts.cf.core.odata.query.v1~`).
    #[error("odata: {0}")]
    OData(toolkit_odata::Error),

    /// Internal error; the diagnostic is only logged.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    pub fn internal(diag: impl Into<String>) -> Self {
        Self::Internal(diag.into())
    }

    pub fn not_found(res: Res, name: impl Into<String>) -> Self {
        Self::NotFound(res, name.into())
    }

    pub fn invalid(
        res: Res,
        field: &'static str,
        reason: &'static str,
        description: impl Into<String>,
    ) -> Self {
        Self::InvalidArgument {
            res,
            field,
            reason,
            description: description.into(),
        }
    }

    pub fn out_of_range(
        res: Res,
        field: &'static str,
        reason: &'static str,
        description: impl Into<String>,
    ) -> Self {
        Self::OutOfRange {
            res,
            field,
            reason,
            description: description.into(),
        }
    }

    pub fn precondition(
        res: Res,
        subject: &'static str,
        kind: &'static str,
        description: impl Into<String>,
    ) -> Self {
        Self::FailedPrecondition {
            res,
            subject,
            kind,
            description: description.into(),
        }
    }

    pub fn aborted(res: Res, reason: &'static str, detail: impl Into<String>) -> Self {
        Self::Aborted {
            res,
            reason,
            detail: detail.into(),
        }
    }

    pub fn feature_disabled(subject: &'static str) -> Self {
        Self::precondition(
            Res::Message,
            subject,
            "FEATURE_DISABLED",
            format!("{subject} is disabled"),
        )
    }

    pub fn invalid_model(res: Res) -> Self {
        Self::invalid(res, "model", "INVALID_MODEL", "Unknown or disabled model")
    }

    pub fn invalid_attachment(description: impl Into<String>) -> Self {
        Self::invalid(
            Res::Message,
            "attachment",
            "invalid_attachment",
            description,
        )
    }

    pub fn turn_already_running() -> Self {
        Self::aborted(
            Res::Turn,
            "turn_already_running",
            "Another turn is already running in this chat",
        )
    }

    pub fn request_id_conflict() -> Self {
        Self::aborted(
            Res::Turn,
            "request_id_conflict",
            "The request_id is already used by another turn",
        )
    }

    pub fn storage_unavailable(diag: impl Into<String>) -> Self {
        Self::Unavailable {
            retry_after_secs: 10,
            diagnostic: diag.into(),
        }
    }

    /// `true` for a unique-violation conflict raised by the DB layer.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::AlreadyExists { name, .. } if name == "unique_violation")
    }
}

macro_rules! by_res {
    ($res:expr, |$e:ident| $body:expr) => {
        match $res {
            Res::Chat => {
                type $e = ChatError;
                $body
            }
            Res::Message => {
                type $e = MessageError;
                $body
            }
            Res::Turn => {
                type $e = TurnError;
                $body
            }
            Res::Attachment => {
                type $e = AttachmentError;
                $body
            }
            Res::Model => {
                type $e = ModelError;
                $body
            }
            Res::Quota => {
                type $e = QuotaError;
                $body
            }
        }
    };
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::cognitive_complexity)] // one arm per error category
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound(res, name) => by_res!(res, |E| E::not_found(format!(
                "{} not found",
                res_label(res)
            ))
            .with_resource(name)
            .create()),
            DomainError::InvalidArgument {
                res,
                field,
                reason,
                description,
            } => by_res!(res, |E| E::invalid_argument()
                .with_field_violation(field, description, reason)
                .create()),
            DomainError::OutOfRange {
                res,
                field,
                reason,
                description,
            } => by_res!(res, |E| E::out_of_range(description.clone())
                .with_field_violation(field, description, reason)
                .create()),
            DomainError::FailedPrecondition {
                res,
                subject,
                kind,
                description,
            } => by_res!(res, |E| E::failed_precondition()
                .with_precondition_violation(subject, description, kind)
                .create()),
            DomainError::Aborted {
                res,
                reason,
                detail,
            } => {
                by_res!(res, |E| E::aborted(detail).with_reason(reason).create())
            }
            DomainError::AlreadyExists { res, name, detail } => {
                by_res!(res, |E| E::already_exists(detail)
                    .with_resource(name)
                    .create())
            }
            DomainError::QuotaExceeded(scope) => {
                QuotaError::resource_exhausted(format!("Quota exceeded ({})", scope.as_str()))
                    .with_quota_violation(scope.as_str(), "quota_exceeded")
                    .create()
            }
            DomainError::ChatLimit {
                subject,
                description,
            } => AttachmentError::resource_exhausted(description.clone())
                .with_quota_violation(subject, description)
                .create(),
            DomainError::AuthzDenied | DomainError::PermissionDenied => {
                ChatError::permission_denied()
                    .with_reason("AUTHZ_DENIED")
                    .create()
            }
            DomainError::AuthzUnavailable(diag) => {
                tracing::error!(error = %diag, "authorization evaluation failed");
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(5)
                    .create()
            }
            DomainError::Unavailable {
                retry_after_secs,
                diagnostic,
            } => {
                tracing::warn!(error = %diagnostic, "service unavailable");
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(retry_after_secs)
                    .create()
            }
            DomainError::InvalidFormat(msg) => {
                ChatError::invalid_argument().with_format(msg).create()
            }
            DomainError::OData(e) => CanonicalError::from(e),
            DomainError::Contention(diag) => {
                tracing::warn!(error = %diag, "database contention");
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(1)
                    .create()
            }
            DomainError::Internal(diag) => {
                tracing::error!(error = %diag, "mini-chat internal error");
                CanonicalError::internal(diag).create()
            }
        }
    }
}

const fn res_label(res: Res) -> &'static str {
    match res {
        Res::Chat => "Chat",
        Res::Message => "Message",
        Res::Turn => "Turn",
        Res::Attachment => "Attachment",
        Res::Model => "Model",
        Res::Quota => "Quota",
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        match err {
            toolkit_db::DbError::Sea(db) => crate::infra::db::classify_db_err(db),
            other => Self::Internal(format!("database error: {other}")),
        }
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        crate::infra::db::map_scope_err(err)
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::OData(err)
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
