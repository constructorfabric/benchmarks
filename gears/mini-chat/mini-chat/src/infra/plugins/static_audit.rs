//! Static audit plugin (`static-mini-chat-audit-plugin`): registers an audit plugin instance
//! in types-registry and logs the audit events it receives while `enabled`.
//!
//! The module stays reachable as `mini_chat::infra::plugins::static_audit`.

pub mod config;
pub mod gear;
pub mod service;

pub use config::StaticAuditConfig;
pub use gear::StaticMiniChatAuditPlugin;
pub use service::StaticAuditService;
