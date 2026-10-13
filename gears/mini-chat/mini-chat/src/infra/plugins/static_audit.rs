//! `static-mini-chat-audit-plugin`: logs audit events (or ignores them when
//! disabled).

mod config;
mod gear;
mod service;

pub use config::StaticAuditConfig;
pub use gear::StaticAuditGear;
pub use service::StaticAuditService;
