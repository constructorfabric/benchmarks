//! Axum handlers. Services and the caller's `SecurityContext` come from request
//! extensions; bodies and paths use the platform extractors (canonical rejections).

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod models;
pub mod quota;
pub mod reactions;
pub mod stream;
pub mod turns;
