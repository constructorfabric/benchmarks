//! Static audit plugin: gear `static-mini-chat-audit-plugin`. It logs the audit events it receives.

mod config;
mod gear;
mod service;

pub use config::StaticAuditConfig;
pub use gear::StaticAuditPlugin;
pub use service::StaticAuditService;
