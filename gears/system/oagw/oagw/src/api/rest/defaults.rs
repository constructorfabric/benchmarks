//! Shared serde default constructors for the REST request DTOs.

use crate::domain::models::Protocol;

/// `true`, the default of every `enabled` field (PRD §5.1).
#[must_use]
pub fn default_true() -> bool {
    true
}

/// Default upstream protocol: the HTTP protocol GTS id.
#[must_use]
pub fn default_protocol() -> Protocol {
    Protocol::Http
}
