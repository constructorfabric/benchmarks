//! Axum handlers. Each handler reads `SecurityContext` and `Arc<AppServices>` from request
//! extensions and returns `ApiResult` (`CanonicalError` problems).

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod models;
pub mod quota;
pub mod reactions;
pub mod stream;
pub mod turns;
