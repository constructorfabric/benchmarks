//! Domain errors and their mapping to the canonical error contract
//! (REST `Problem`, ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.user_quota.v1~"))]
pub struct QuotaResourceError;

/// Resource a domain error refers to (selects the `resource_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Res {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
    Quota,
}

/// Domain error. Every variant maps to exactly one canonical category.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    #[error("{res:?} not found")]
    NotFound { res: Res, id: String },

    #[error("invalid argument {field}: {reason}")]
    InvalidArgument {
        res: Res,
        field: String,
        reason: String,
        description: String,
    },

    #[error("invalid format: {message}")]
    InvalidFormat { res: Res, message: String },

    #[error("out of range {field}: {reason}")]
    OutOfRange {
        res: Res,
        field: String,
        reason: String,
        description: String,
    },

    #[error("failed precondition {subject}: {type_}")]
    FailedPrecondition {
        res: Res,
        subject: String,
        type_: String,
        description: String,
    },

    #[error("resource exhausted: {subject}")]
    ResourceExhausted {
        res: Res,
        subject: String,
        description: String,
    },

    #[error("aborted: {reason}")]
    Aborted {
        res: Res,
        reason: String,
        detail: String,
    },

    #[error("already exists: {resource_name}")]
    AlreadyExists {
        res: Res,
        resource_name: String,
        detail: String,
    },

    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },

    #[error("service unavailable: {detail}")]
    ServiceUnavailable { retry_after: u64, detail: String },

    #[error("internal error: {0}")]
    Internal(String),

    #[error("database error: {0}")]
    Db(std::sync::Arc<sea_orm::DbErr>),

    #[error("{0}")]
    Canonical(CanonicalError),
}

impl DomainError {
    #[allow(clippy::needless_pass_by_value, reason = "accepts any displayable id")]
    pub fn not_found(res: Res, id: impl ToString) -> Self {
        Self::NotFound {
            res,
            id: id.to_string(),
        }
    }

    pub fn invalid(res: Res, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::InvalidArgument {
            res,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    pub fn out_of_range(
        res: Res,
        field: &str,
        reason: &str,
        description: impl Into<String>,
    ) -> Self {
        Self::OutOfRange {
            res,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    pub fn precondition(
        res: Res,
        subject: &str,
        type_: &str,
        description: impl Into<String>,
    ) -> Self {
        Self::FailedPrecondition {
            res,
            subject: subject.to_owned(),
            type_: type_.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn feature_disabled(subject: &str) -> Self {
        Self::precondition(
            Res::Chat,
            subject,
            "FEATURE_DISABLED",
            format!("{subject} is disabled by the operator"),
        )
    }

    #[must_use]
    pub fn quota_exceeded(scope: &str) -> Self {
        Self::ResourceExhausted {
            res: Res::Quota,
            subject: scope.to_owned(),
            description: "quota_exceeded".to_owned(),
        }
    }

    pub fn aborted(reason: &str, detail: impl Into<String>) -> Self {
        Self::Aborted {
            res: Res::Chat,
            reason: reason.to_owned(),
            detail: detail.into(),
        }
    }

    pub fn already_exists(res: Res, name: &str, detail: impl Into<String>) -> Self {
        Self::AlreadyExists {
            res,
            resource_name: name.to_owned(),
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn forbidden() -> Self {
        Self::PermissionDenied {
            reason: "AUTHZ_DENIED".to_owned(),
        }
    }

    #[must_use]
    pub fn unavailable(retry_after: u64) -> Self {
        Self::ServiceUnavailable {
            retry_after,
            detail: "Service temporarily unavailable".to_owned(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn invalid_model(description: impl Into<String>) -> Self {
        Self::invalid(Res::Chat, "model", "INVALID_MODEL", description)
    }

    pub fn invalid_attachment(description: impl Into<String>) -> Self {
        Self::invalid(Res::Chat, "attachment", "invalid_attachment", description)
    }

    /// The wrapped database error (transaction retry classification).
    #[must_use]
    pub fn db_err(&self) -> Option<&sea_orm::DbErr> {
        match self {
            Self::Db(e) => Some(e.as_ref()),
            _ => None,
        }
    }

    /// Whether this error wraps a unique-constraint violation.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        self.db_err()
            .is_some_and(toolkit_db::secure::is_unique_violation)
    }

    /// Whether this error is a 409 `aborted` with the given reason.
    #[must_use]
    pub fn is_aborted_reason(&self, r: &str) -> bool {
        matches!(self, Self::Aborted { reason, .. } if reason == r)
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        match e {
            toolkit_db::secure::ScopeError::Denied(_)
            | toolkit_db::secure::ScopeError::TenantNotInScope { .. } => {
                tracing::warn!(error = %e, "secure ORM denied operation");
                Self::forbidden()
            }
            toolkit_db::secure::ScopeError::Db(e) => Self::Db(std::sync::Arc::new(e)),
            other => Self::Internal(format!("database error: {other}")),
        }
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Db(std::sync::Arc::new(e))
    }
}

impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(e: authz_resolver_sdk::EnforcerError) -> Self {
        match e {
            authz_resolver_sdk::EnforcerError::Denied { .. }
            | authz_resolver_sdk::EnforcerError::CompileFailed(_) => {
                tracing::info!(error = %e, "authorization denied");
                Self::forbidden()
            }
            authz_resolver_sdk::EnforcerError::EvaluationFailed(err) => {
                tracing::error!(error = %err, "PDP evaluation failed");
                Self::ServiceUnavailable {
                    retry_after: 5,
                    detail: "Authorization service temporarily unavailable".to_owned(),
                }
            }
        }
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(e: toolkit_odata::Error) -> Self {
        Self::Canonical(CanonicalError::from(e))
    }
}

fn invalid_argument(res: Res, field: &str, description: &str, reason: &str) -> CanonicalError {
    match res {
        Res::Chat => ChatResourceError::invalid_argument()
            .with_field_violation(field, description, reason)
            .create(),
        Res::Message => MessageResourceError::invalid_argument()
            .with_field_violation(field, description, reason)
            .create(),
        Res::Turn => TurnResourceError::invalid_argument()
            .with_field_violation(field, description, reason)
            .create(),
        Res::Attachment => AttachmentResourceError::invalid_argument()
            .with_field_violation(field, description, reason)
            .create(),
        Res::Model => ModelResourceError::invalid_argument()
            .with_field_violation(field, description, reason)
            .create(),
        Res::Quota => QuotaResourceError::invalid_argument()
            .with_field_violation(field, description, reason)
            .create(),
    }
}

macro_rules! per_res {
    ($res:expr, $ty:ident => $body:expr) => {
        match $res {
            Res::Chat => {
                type $ty = ChatResourceError;
                $body
            }
            Res::Message => {
                type $ty = MessageResourceError;
                $body
            }
            Res::Turn => {
                type $ty = TurnResourceError;
                $body
            }
            Res::Attachment => {
                type $ty = AttachmentResourceError;
                $body
            }
            Res::Model => {
                type $ty = ModelResourceError;
                $body
            }
            Res::Quota => {
                type $ty = QuotaResourceError;
                $body
            }
        }
    };
}

impl From<DomainError> for CanonicalError {
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound { res, id } => {
                let detail = match res {
                    Res::Chat => "Chat not found",
                    Res::Message => "Message not found",
                    Res::Turn => "Turn not found",
                    Res::Attachment => "Attachment not found",
                    Res::Model => "Model not found",
                    Res::Quota => "Quota not found",
                };
                per_res!(res, R => R::not_found(detail).with_resource(id).create())
            }
            DomainError::InvalidArgument {
                res,
                field,
                reason,
                description,
            } => invalid_argument(res, &field, &description, &reason),
            DomainError::InvalidFormat { res, message } => {
                per_res!(res, R => R::invalid_argument().with_format(message).create())
            }
            DomainError::OutOfRange {
                res,
                field,
                reason,
                description,
            } => per_res!(res, R => R::out_of_range(description.clone())
                .with_field_violation(field, description, reason)
                .create()),
            DomainError::FailedPrecondition {
                res,
                subject,
                type_,
                description,
            } => per_res!(res, R => R::failed_precondition()
                .with_precondition_violation(subject, description, type_)
                .create()),
            DomainError::ResourceExhausted {
                res,
                subject,
                description,
            } => {
                let detail = if description == "quota_exceeded" {
                    format!("Quota exceeded ({subject})")
                } else {
                    format!("Limit reached ({subject})")
                };
                per_res!(res, R => R::resource_exhausted(detail)
                    .with_quota_violation(subject, description)
                    .create())
            }
            DomainError::Aborted {
                res,
                reason,
                detail,
            } => {
                per_res!(res, R => R::aborted(detail).with_reason(reason).create())
            }
            DomainError::AlreadyExists {
                res,
                resource_name,
                detail,
            } => {
                per_res!(res, R => R::already_exists(detail).with_resource(resource_name).create())
            }
            DomainError::PermissionDenied { reason } => ChatResourceError::permission_denied()
                .with_reason(reason)
                .create(),
            DomainError::ServiceUnavailable {
                retry_after,
                detail,
            } => CanonicalError::service_unavailable()
                .with_retry_after_seconds(retry_after)
                .with_detail(detail)
                .create(),
            DomainError::Internal(msg) => {
                tracing::error!(error = %msg, "internal error");
                CanonicalError::internal(msg).create()
            }
            DomainError::Canonical(e) => e,
            DomainError::Db(e) if toolkit_db::secure::is_unique_violation(&e) => {
                tracing::warn!(error = %e, "unhandled unique constraint violation");
                ChatResourceError::already_exists("The resource conflicts with an existing one")
                    .with_resource("unique_violation")
                    .create()
            }
            DomainError::Db(e) => {
                tracing::error!(error = %e, "database error");
                CanonicalError::internal(format!("database error: {e}")).create()
            }
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;
