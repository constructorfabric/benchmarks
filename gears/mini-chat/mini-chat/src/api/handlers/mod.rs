//! Route handlers grouped by area. Each area exposes `register(router, openapi, ...)`.

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod models;
pub mod quota;
pub mod stream;
pub mod turns;

use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature};

/// Interim license gate: the platform base license feature (ADR-0008).
pub struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Route path prefix.
pub const V1: &str = "/mini-chat/v1";
