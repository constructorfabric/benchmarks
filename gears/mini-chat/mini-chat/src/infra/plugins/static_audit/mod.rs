//! `static-mini-chat-audit-plugin`: logs audit events delivered by the `mini-chat.audit` queue.

mod gear;

pub use gear::{StaticAuditConfig, StaticAuditService, StaticMiniChatAuditPlugin};
