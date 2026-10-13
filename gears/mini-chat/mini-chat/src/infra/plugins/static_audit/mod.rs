//! Bundled static audit plugin (`cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`).

pub mod config;
pub mod gear;
pub mod service;

pub use config::{StaticAuditConfig, StaticAuditConfigError};
pub use gear::StaticAuditPlugin;
pub use service::StaticAuditService;
