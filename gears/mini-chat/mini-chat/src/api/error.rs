//! `DomainError` → canonical `Problem` mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, Resource};

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

macro_rules! by_resource {
    ($res:expr, $method:ident ( $($arg:expr),* ) $(. $chain:ident ( $($carg:expr),* ))* ) => {
        match $res {
            Resource::Chat => ChatResourceError::$method($($arg),*)$(.$chain($($carg),*))*.create(),
            Resource::Message => MessageResourceError::$method($($arg),*)$(.$chain($($carg),*))*.create(),
            Resource::Turn => TurnResourceError::$method($($arg),*)$(.$chain($($carg),*))*.create(),
            Resource::Attachment => AttachmentResourceError::$method($($arg),*)$(.$chain($($carg),*))*.create(),
            Resource::Model => ModelResourceError::$method($($arg),*)$(.$chain($($carg),*))*.create(),
        }
    };
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound { resource, id } => {
                let detail = format!("{} not found", resource_label(resource));
                by_resource!(resource, not_found(detail).with_resource(id))
            }
            DomainError::InvalidArgument { resource, field, reason, description } => {
                by_resource!(resource, invalid_argument().with_field_violation(field, description, reason))
            }
            DomainError::OutOfRange { resource, field, reason, description } => {
                let detail = description.clone();
                by_resource!(resource, out_of_range(detail).with_field_violation(field, description, reason))
            }
            DomainError::FailedPrecondition { resource, subject, violation_type, description } => {
                by_resource!(resource, failed_precondition().with_precondition_violation(subject, description, violation_type))
            }
            DomainError::Aborted { resource, reason, detail } => {
                by_resource!(resource, aborted(detail).with_reason(reason))
            }
            DomainError::AlreadyExists { resource, name, detail } => {
                by_resource!(resource, already_exists(detail).with_resource(name))
            }
            DomainError::ResourceExhausted { resource, subject, description, detail } => {
                by_resource!(resource, resource_exhausted(detail).with_quota_violation(subject, description))
            }
            DomainError::PermissionDenied { reason } => {
                ChatResourceError::permission_denied().with_reason(reason).create()
            }
            DomainError::ServiceUnavailable { retry_after_secs, detail } => {
                tracing::warn!(%detail, "service unavailable");
                CanonicalError::service_unavailable().with_retry_after_seconds(retry_after_secs).create()
            }
            DomainError::InvalidFormat(msg) => ChatResourceError::invalid_argument().with_format(msg).create(),
            DomainError::OData(e) => CanonicalError::from(e),
            DomainError::Internal(msg) => {
                tracing::error!(error = %msg, "mini-chat internal error");
                CanonicalError::internal(msg).create()
            }
        }
    }
}

const fn resource_label(r: Resource) -> &'static str {
    match r {
        Resource::Chat => "Chat",
        Resource::Message => "Message",
        Resource::Turn => "Turn",
        Resource::Attachment => "Attachment",
        Resource::Model => "Model",
    }
}
