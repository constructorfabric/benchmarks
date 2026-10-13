//! Domain errors and their mapping to the canonical error contract (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResource;
#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResource;
#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResource;
#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResource;
#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResource;

/// Which resource type an error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("not found: {detail}")]
    NotFound {
        resource: Resource,
        detail: String,
        name: String,
    },
    #[error("invalid argument {field}: {reason}")]
    InvalidArgument {
        resource: Resource,
        field: &'static str,
        reason: &'static str,
        description: String,
    },
    #[error("out of range {field}: {reason}")]
    OutOfRange {
        resource: Resource,
        field: &'static str,
        reason: &'static str,
        description: String,
    },
    #[error("failed precondition {subject}: {type_}")]
    FailedPrecondition {
        resource: Resource,
        subject: &'static str,
        type_: &'static str,
        description: String,
    },
    #[error("aborted: {reason}")]
    Aborted {
        resource: Resource,
        reason: &'static str,
        detail: String,
    },
    #[error("already exists: {name}")]
    AlreadyExists {
        resource: Resource,
        name: &'static str,
        detail: String,
    },
    #[error("resource exhausted: {subject}")]
    ResourceExhausted {
        resource: Resource,
        subject: String,
        description: String,
    },
    #[error("permission denied: {reason}")]
    PermissionDenied {
        resource: Resource,
        reason: &'static str,
    },
    #[error("service unavailable: {detail}")]
    ServiceUnavailable { retry_after: u64, detail: String },
    #[error("internal: {0}")]
    Internal(String),
    #[error("odata: {0}")]
    OData(toolkit_odata::Error),
    #[error("canonical: {0}")]
    Canonical(Box<CanonicalError>),
}

impl DomainError {
    #[must_use]
    pub fn not_found<T: ToString + ?Sized>(resource: Resource, id: &T) -> Self {
        let name = id.to_string();
        let what = match resource {
            Resource::Chat => "chat",
            Resource::Message => "message",
            Resource::Turn => "turn",
            Resource::Attachment => "attachment",
            Resource::Model => "model",
        };
        Self::NotFound {
            resource,
            detail: format!("{what} not found"),
            name,
        }
    }

    #[must_use]
    pub fn invalid(
        resource: Resource,
        field: &'static str,
        reason: &'static str,
        description: impl Into<String>,
    ) -> Self {
        Self::InvalidArgument {
            resource,
            field,
            reason,
            description: description.into(),
        }
    }

    #[must_use]
    pub fn out_of_range(
        resource: Resource,
        field: &'static str,
        reason: &'static str,
        description: impl Into<String>,
    ) -> Self {
        Self::OutOfRange {
            resource,
            field,
            reason,
            description: description.into(),
        }
    }

    #[must_use]
    pub fn precondition(
        resource: Resource,
        subject: &'static str,
        type_: &'static str,
        description: impl Into<String>,
    ) -> Self {
        Self::FailedPrecondition {
            resource,
            subject,
            type_,
            description: description.into(),
        }
    }

    #[must_use]
    pub fn aborted(resource: Resource, reason: &'static str, detail: impl Into<String>) -> Self {
        Self::Aborted {
            resource,
            reason,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn quota_exceeded(scope: &str) -> Self {
        Self::ResourceExhausted {
            resource: Resource::Chat,
            subject: scope.to_owned(),
            description: "quota_exceeded".to_owned(),
        }
    }

    #[must_use]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    #[must_use]
    pub fn feature_disabled(subject: &'static str) -> Self {
        Self::precondition(
            Resource::Chat,
            subject,
            "FEATURE_DISABLED",
            format!("{subject} is disabled"),
        )
    }

    #[must_use]
    pub fn invalid_model() -> Self {
        Self::invalid(
            Resource::Chat,
            "model",
            "INVALID_MODEL",
            "model is not available in the model catalog",
        )
    }

    #[must_use]
    pub fn authz_denied(resource: Resource) -> Self {
        Self::PermissionDenied {
            resource,
            reason: "AUTHZ_DENIED",
        }
    }

    #[must_use]
    pub fn pdp_unavailable() -> Self {
        Self::ServiceUnavailable {
            retry_after: 5,
            detail: "Authorization service temporarily unavailable".to_owned(),
        }
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        Self::Internal(format!("database scope error: {e}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(e: toolkit_db::outbox::OutboxError) -> Self {
        Self::Internal(format!("outbox error: {e}"))
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(e: toolkit_odata::Error) -> Self {
        Self::OData(e)
    }
}

impl From<CanonicalError> for DomainError {
    fn from(e: CanonicalError) -> Self {
        Self::Canonical(Box::new(e))
    }
}

macro_rules! by_resource {
    ($res:expr, $m:ident ( $($a:expr),* ) $( . $rest:ident ( $($b:expr),* ) )* ) => {
        match $res {
            Resource::Chat => ChatResource::$m($($a),*)$(.$rest($($b),*))*.create(),
            Resource::Message => MessageResource::$m($($a),*)$(.$rest($($b),*))*.create(),
            Resource::Turn => TurnResource::$m($($a),*)$(.$rest($($b),*))*.create(),
            Resource::Attachment => AttachmentResource::$m($($a),*)$(.$rest($($b),*))*.create(),
            Resource::Model => ModelResource::$m($($a),*)$(.$rest($($b),*))*.create(),
        }
    };
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound {
                resource,
                detail,
                name,
            } => {
                by_resource!(resource, not_found(detail).with_resource(name))
            }
            DomainError::InvalidArgument {
                resource,
                field,
                reason,
                description,
            } => {
                by_resource!(
                    resource,
                    invalid_argument().with_field_violation(field, description, reason)
                )
            }
            DomainError::OutOfRange {
                resource,
                field,
                reason,
                description,
            } => {
                by_resource!(
                    resource,
                    out_of_range(description.clone()).with_field_violation(
                        field,
                        description,
                        reason
                    )
                )
            }
            DomainError::FailedPrecondition {
                resource,
                subject,
                type_,
                description,
            } => {
                by_resource!(
                    resource,
                    failed_precondition().with_precondition_violation(subject, description, type_)
                )
            }
            DomainError::Aborted {
                resource,
                reason,
                detail,
            } => {
                by_resource!(resource, aborted(detail).with_reason(reason))
            }
            DomainError::AlreadyExists {
                resource,
                name,
                detail,
            } => {
                by_resource!(resource, already_exists(detail).with_resource(name))
            }
            DomainError::ResourceExhausted {
                resource,
                subject,
                description,
            } => {
                by_resource!(
                    resource,
                    resource_exhausted("Quota exceeded").with_quota_violation(subject, description)
                )
            }
            DomainError::PermissionDenied { resource, reason } => {
                by_resource!(resource, permission_denied().with_reason(reason))
            }
            DomainError::ServiceUnavailable {
                retry_after,
                detail,
            } => CanonicalError::service_unavailable()
                .with_retry_after_seconds(retry_after)
                .with_detail(detail)
                .create(),
            DomainError::Internal(diag) => {
                tracing::error!(error = %diag, "mini-chat internal error");
                CanonicalError::internal(diag).create()
            }
            DomainError::OData(e) => CanonicalError::from(e),
            DomainError::Canonical(e) => *e,
        }
    }
}
