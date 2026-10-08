//! Domain error type. Mapped to canonical `Problem` responses in `api::error`.

use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

/// Resource kinds used for `context.resource_type` of error responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

/// Domain-level error. Every variant maps to exactly one canonical category.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    /// 404 `not_found` (missing, foreign or soft-deleted resource).
    #[error("{resource:?} not found: {id}")]
    NotFound { resource: Resource, id: String },

    /// 400 `invalid_argument` with one field violation.
    #[error("invalid argument {field}: {reason} ({description})")]
    InvalidArgument { resource: Resource, field: String, reason: String, description: String },

    /// 400 `out_of_range` with one field violation.
    #[error("out of range {field}: {reason} ({description})")]
    OutOfRange { resource: Resource, field: String, reason: String, description: String },

    /// 400 `failed_precondition` with one violation `{subject, description, type}`.
    #[error("failed precondition {subject}: {violation_type} ({description})")]
    FailedPrecondition { resource: Resource, subject: String, violation_type: String, description: String },

    /// 409 `aborted` with `context.reason`.
    #[error("aborted: {reason} ({detail})")]
    Aborted { resource: Resource, reason: String, detail: String },

    /// 409 `already_exists` with `context.resource_name`.
    #[error("already exists: {name} ({detail})")]
    AlreadyExists { resource: Resource, name: String, detail: String },

    /// 429 `resource_exhausted` with one quota violation `{subject, description}`.
    #[error("resource exhausted: {subject} ({description})")]
    ResourceExhausted { resource: Resource, subject: String, description: String, detail: String },

    /// 403 `permission_denied` (`AUTHZ_DENIED`).
    #[error("permission denied")]
    PermissionDenied { reason: String },

    /// 503 `service_unavailable` with `Retry-After`.
    #[error("service unavailable: {detail}")]
    ServiceUnavailable { retry_after_secs: u64, detail: String },

    /// 400 `invalid_argument` with a free-form `format` message (e.g. payload too large).
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// OData query / pagination error (400 with `gts.cf.core.odata.query.v1~`).
    #[error("odata error: {0}")]
    OData(toolkit_odata::Error),

    /// 500 `internal`.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    #[must_use]
    pub fn not_found(resource: Resource, id: impl ToString) -> Self {
        Self::NotFound { resource, id: id.to_string() }
    }

    #[must_use]
    pub fn invalid(resource: Resource, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::InvalidArgument {
            resource,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn out_of_range(resource: Resource, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::OutOfRange { resource, field: field.to_owned(), reason: reason.to_owned(), description: description.into() }
    }

    #[must_use]
    pub fn precondition(resource: Resource, subject: &str, violation_type: &str, description: impl Into<String>) -> Self {
        Self::FailedPrecondition {
            resource,
            subject: subject.to_owned(),
            violation_type: violation_type.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn feature_disabled(subject: &str) -> Self {
        Self::precondition(Resource::Chat, subject, "FEATURE_DISABLED", format!("{subject} is disabled"))
    }

    #[must_use]
    pub fn aborted(resource: Resource, reason: &str, detail: impl Into<String>) -> Self {
        Self::Aborted { resource, reason: reason.to_owned(), detail: detail.into() }
    }

    #[must_use]
    pub fn quota_exceeded(scope: &str) -> Self {
        Self::ResourceExhausted {
            resource: Resource::Chat,
            subject: scope.to_owned(),
            description: "quota_exceeded".to_owned(),
            detail: format!("Quota exceeded ({scope})"),
        }
    }

    #[must_use]
    pub fn permission_denied() -> Self {
        Self::PermissionDenied { reason: "AUTHZ_DENIED".to_owned() }
    }

    #[must_use]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    #[must_use]
    pub fn unavailable(retry_after_secs: u64, detail: impl Into<String>) -> Self {
        Self::ServiceUnavailable { retry_after_secs, detail: detail.into() }
    }

    /// `true` for transient lock contention (SQLite busy/locked, PostgreSQL serialization
    /// failure / deadlock): the transaction can be retried.
    #[must_use]
    pub fn is_contention(&self) -> bool {
        matches!(self, Self::Internal(m) if m.starts_with(CONTENTION_PREFIX))
    }

    /// `true` for a unique-constraint violation surfaced by the DB layer.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::AlreadyExists { name, .. } if name == "unique_violation")
    }
}

const CONTENTION_PREFIX: &str = "db contention: ";

fn is_contention_text(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("database is locked")
        || m.contains("database table is locked")
        || m.contains("(code: 5)")
        || m.contains("(code: 6)")
        || m.contains("(code: 517)")
        || m.contains("(code: 262)")
        || m.contains("could not serialize access")
        || m.contains("deadlock detected")
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(e) => Self::from(ScopeError::Db(e)),
            other => {
                let text = other.to_string();
                if is_contention_text(&text) {
                    Self::Internal(format!("{CONTENTION_PREFIX}{text}"))
                } else {
                    Self::Internal(format!("database error: {text}"))
                }
            }
        }
    }
}

impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        if err.is_unique_violation() {
            tracing::debug!(error = %err, "unique violation");
            return Self::AlreadyExists {
                resource: Resource::Chat,
                name: "unique_violation".to_owned(),
                detail: "Resource already exists".to_owned(),
            };
        }
        let text = err.to_string();
        if is_contention_text(&text) {
            return Self::Internal(format!("{CONTENTION_PREFIX}{text}"));
        }
        Self::Internal(format!("database error: {text}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(err: sea_orm::DbErr) -> Self {
        Self::from(ScopeError::Db(err))
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::OData(err)
    }
}
