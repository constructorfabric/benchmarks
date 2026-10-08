//! Domain error type and its mapping to the canonical error contract
//! (ADR-0004). Reasons are machine-readable; `detail` texts are not contract.

use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

pub const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";
pub const MESSAGE_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.message.v1~";
pub const TURN_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.turn.v1~";
pub const ATTACHMENT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";
pub const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.model.v1~";

#[resource_error("gts.cf.core.mini_chat.chat.v1~")]
pub struct ChatResource;
#[resource_error("gts.cf.core.mini_chat.message.v1~")]
pub struct MessageResource;
#[resource_error("gts.cf.core.mini_chat.turn.v1~")]
pub struct TurnResource;
#[resource_error("gts.cf.core.mini_chat.attachment.v1~")]
pub struct AttachmentResource;
#[resource_error("gts.cf.core.mini_chat.model.v1~")]
pub struct ModelResource;

/// Resource a domain error refers to (selects the canonical resource type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Res {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub field: String,
    pub description: String,
    pub reason: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    #[error("{res:?} not found: {name}")]
    NotFound { res: Res, name: String },
    #[error("invalid argument")]
    InvalidArgument { res: Res, violations: Vec<Violation> },
    #[error("invalid format: {message}")]
    InvalidFormat { res: Res, message: String },
    #[error("out of range")]
    OutOfRange { res: Res, field: String, reason: String, description: String },
    #[error("failed precondition {subject}/{kind}")]
    FailedPrecondition { res: Res, subject: String, kind: String, description: String },
    #[error("resource exhausted: {subject}")]
    ResourceExhausted { res: Res, subject: String, description: String },
    #[error("aborted: {reason}")]
    Aborted { res: Res, reason: String, detail: String },
    #[error("already exists: {name}")]
    AlreadyExists { res: Res, name: String, detail: String },
    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },
    #[error("service unavailable: {detail}")]
    ServiceUnavailable { detail: String, retry_after: u64 },
    #[error("internal: {0}")]
    Internal(String),
    #[error("odata: {0}")]
    OData(String),
    #[error("canonical")]
    Canonical(Box<CanonicalError>),
}

impl DomainError {
    pub fn not_found(res: Res, name: impl Into<String>) -> Self {
        Self::NotFound { res, name: name.into() }
    }
    pub fn chat_not_found(id: uuid::Uuid) -> Self {
        Self::not_found(Res::Chat, id.to_string())
    }
    pub fn field(res: Res, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::InvalidArgument {
            res,
            violations: vec![Violation {
                field: field.to_owned(),
                description: description.into(),
                reason: reason.to_owned(),
            }],
        }
    }
    pub fn out_of_range(res: Res, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::OutOfRange {
            res,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }
    pub fn precondition(res: Res, subject: &str, kind: &str, description: impl Into<String>) -> Self {
        Self::FailedPrecondition {
            res,
            subject: subject.to_owned(),
            kind: kind.to_owned(),
            description: description.into(),
        }
    }
    pub fn feature_disabled(subject: &str) -> Self {
        Self::precondition(
            Res::Chat,
            subject,
            "FEATURE_DISABLED",
            format!("{subject} is disabled"),
        )
    }
    pub fn quota_exceeded(subject: &str) -> Self {
        Self::ResourceExhausted {
            res: Res::Chat,
            subject: subject.to_owned(),
            description: "quota_exceeded".to_owned(),
        }
    }
    pub fn aborted(res: Res, reason: &str, detail: impl Into<String>) -> Self {
        Self::Aborted {
            res,
            reason: reason.to_owned(),
            detail: detail.into(),
        }
    }
    pub fn already_exists(res: Res, name: &str, detail: impl Into<String>) -> Self {
        Self::AlreadyExists {
            res,
            name: name.to_owned(),
            detail: detail.into(),
        }
    }
    pub fn denied() -> Self {
        Self::PermissionDenied {
            reason: "AUTHZ_DENIED".to_owned(),
        }
    }
    pub fn unavailable(retry_after: u64) -> Self {
        Self::ServiceUnavailable {
            detail: "Service temporarily unavailable".to_owned(),
            retry_after,
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn invalid_model() -> Self {
        Self::field(Res::Chat, "model", "INVALID_MODEL", "model is not available in the catalog")
    }
    pub fn invalid_attachment(description: impl Into<String>) -> Self {
        Self::field(Res::Chat, "attachment", "invalid_attachment", description)
    }
    pub fn empty_content() -> Self {
        Self::field(Res::Chat, "content", "EMPTY_CONTENT", "content must not be empty")
    }

    /// `true` for a DB unique-constraint violation.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::AlreadyExists { name, .. } if name == "unique_violation")
    }
}

impl From<ScopeError> for DomainError {
    fn from(e: ScopeError) -> Self {
        if e.is_unique_violation() {
            return Self::AlreadyExists {
                res: Res::Chat,
                name: "unique_violation".to_owned(),
                detail: e.to_string(),
            };
        }
        match e {
            ScopeError::Denied(_) | ScopeError::TenantNotInScope { .. } => Self::denied(),
            other => Self::Internal(format!("database error: {other}")),
        }
    }
}

impl From<DbError> for DomainError {
    fn from(e: DbError) -> Self {
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(e: toolkit_db::outbox::OutboxError) -> Self {
        match e {
            toolkit_db::outbox::OutboxError::PayloadTooLarge { size, max } => Self::InvalidFormat {
                res: Res::Chat,
                message: format!("outbox payload size {size} exceeds maximum {max}"),
            },
            other => Self::Internal(format!("outbox error: {other}")),
        }
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(e: toolkit_odata::Error) -> Self {
        Self::Canonical(Box::new(CanonicalError::from(e)))
    }
}

impl From<CanonicalError> for DomainError {
    fn from(e: CanonicalError) -> Self {
        Self::Canonical(Box::new(e))
    }
}

macro_rules! by_res {
    ($res:expr, $method:ident ( $($arg:expr),* ) $(. $rest:ident ( $($rarg:expr),* ))* ) => {
        match $res {
            Res::Chat => ChatResource::$method($($arg),*)$(.$rest($($rarg),*))*.create(),
            Res::Message => MessageResource::$method($($arg),*)$(.$rest($($rarg),*))*.create(),
            Res::Turn => TurnResource::$method($($arg),*)$(.$rest($($rarg),*))*.create(),
            Res::Attachment => AttachmentResource::$method($($arg),*)$(.$rest($($rarg),*))*.create(),
            Res::Model => ModelResource::$method($($arg),*)$(.$rest($($rarg),*))*.create(),
        }
    };
}

fn invalid_argument(res: Res, violations: Vec<Violation>) -> CanonicalError {
    macro_rules! build {
        ($ty:ident) => {{
            let mut it = violations.into_iter();
            let first = it.next().unwrap_or(Violation {
                field: "request".to_owned(),
                description: "invalid request".to_owned(),
                reason: "INVALID_ARGUMENT".to_owned(),
            });
            let mut b = $ty::invalid_argument().with_field_violation(
                first.field,
                first.description,
                first.reason,
            );
            for v in it {
                b = b.with_field_violation(v.field, v.description, v.reason);
            }
            b.create()
        }};
    }
    match res {
        Res::Chat => build!(ChatResource),
        Res::Message => build!(MessageResource),
        Res::Turn => build!(TurnResource),
        Res::Attachment => build!(AttachmentResource),
        Res::Model => build!(ModelResource),
    }
}

impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::NotFound { res, name } => {
                by_res!(res, not_found("Resource not found").with_resource(name))
            }
            DomainError::InvalidArgument { res, violations } => invalid_argument(res, violations),
            DomainError::InvalidFormat { res, message } => {
                by_res!(res, invalid_argument().with_format(message))
            }
            DomainError::OutOfRange {
                res,
                field,
                reason,
                description,
            } => by_res!(
                res,
                out_of_range(description.clone()).with_field_violation(field, description, reason)
            ),
            DomainError::FailedPrecondition {
                res,
                subject,
                kind,
                description,
            } => by_res!(
                res,
                failed_precondition().with_precondition_violation(subject, description, kind)
            ),
            DomainError::ResourceExhausted {
                res,
                subject,
                description,
            } => by_res!(
                res,
                resource_exhausted("Quota exceeded").with_quota_violation(subject, description)
            ),
            DomainError::Aborted { res, reason, detail } => {
                by_res!(res, aborted(detail).with_reason(reason))
            }
            DomainError::AlreadyExists { res, name, detail } => {
                let detail = if name == "unique_violation" {
                    tracing::warn!(detail = %detail, "unique constraint violation");
                    "Resource already exists".to_owned()
                } else {
                    detail
                };
                by_res!(res, already_exists(detail).with_resource(name))
            }
            DomainError::PermissionDenied { reason } => {
                ChatResource::permission_denied().with_reason(reason).create()
            }
            DomainError::ServiceUnavailable { detail, retry_after } => {
                CanonicalError::service_unavailable()
                    .with_detail(detail)
                    .with_retry_after_seconds(retry_after)
                    .create()
            }
            DomainError::Internal(msg) => {
                tracing::error!(error = %msg, "mini-chat internal error");
                CanonicalError::internal(msg).create()
            }
            DomainError::OData(msg) => CanonicalError::internal(msg).create(),
            DomainError::Canonical(c) => *c,
        }
    }
}

/// Map a PEP enforcer error (fail-closed: denial/compile → 403, evaluation → 503).
#[must_use]
pub fn map_enforcer_error(e: &authz_resolver_sdk::EnforcerError) -> DomainError {
    match e {
        authz_resolver_sdk::EnforcerError::Denied { .. }
        | authz_resolver_sdk::EnforcerError::CompileFailed(_) => DomainError::denied(),
        authz_resolver_sdk::EnforcerError::EvaluationFailed(err) => {
            tracing::error!(error = %err, "PDP evaluation failed");
            DomainError::unavailable(5)
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;
