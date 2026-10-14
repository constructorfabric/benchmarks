//! Conversions of storage and outbox errors into [`DomainError`].
//!
//! A unique-constraint violation becomes `UniqueViolation { code: "unique_violation" }`;
//! an outbox payload-size failure becomes `OutboxPayloadTooLarge` (the caller supplies the
//! site through [`outbox_error_for`]); every other database failure becomes `Internal`
//! with the driver text only in the diagnostic.

use toolkit_db::DbError;
use toolkit_db::outbox::OutboxError;
use toolkit_db::secure::{ScopeError, is_unique_violation};

use crate::domain::error::{DomainError, EnqueueSite};

/// The conflict code of an unhandled unique-constraint violation.
const UNIQUE_VIOLATION_CODE: &str = "unique_violation";

fn unique_violation() -> DomainError {
    DomainError::UniqueViolation {
        code: UNIQUE_VIOLATION_CODE.to_owned(),
    }
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(db_err) if is_unique_violation(&db_err) => unique_violation(),
            // driver text stays in the diagnostic, which is logged and never returned
            other => Self::internal(format!("database error: {other}")),
        }
    }
}

impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        if err.is_unique_violation() {
            return unique_violation();
        }
        Self::internal(format!("scoped query failed: {err}"))
    }
}

/// Map an outbox enqueue error for the given call site.
#[must_use]
pub fn outbox_error_for(err: OutboxError, site: EnqueueSite) -> DomainError {
    match err {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-payload-error
        OutboxError::PayloadTooLarge { .. } => DomainError::OutboxPayloadTooLarge { site },
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-payload-error
        other => DomainError::internal(format!("outbox enqueue failed: {other}")),
    }
}

#[cfg(test)]
#[path = "error_conv_tests.rs"]
mod error_conv_tests;
